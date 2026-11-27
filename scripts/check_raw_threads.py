#!/usr/bin/env python3
"""Fail when a fixture plugin spawns a raw thread outside `#[cfg(test)]`.

Why
---
Every cross-invoke background thread in a plugin dylib must be spawned through
`cordis_plugin_sdk::ServiceThreads::spawn`, so that the runtime can join it
before `dlclose` (see `crates/cordis-plugin-sdk/src/service_threads.rs` and
`docs/architecture/contracts-and-loading.md` §4.1). A bare
`std::thread::spawn` is invisible to that registry: the plugin may declare
`allow_plugin_unload!()`, the runtime sees "0 live threads", unloads the image,
and the detached thread later executes unmapped code — the `qq::qq_serve`
SIGSEGV class.

The declaration itself (`_cordis_allows_unload`) is not verifiable at run time,
but it *is* verifiable statically. This script is that check.

Rule
----
In `fixtures/plugins/*/src/**/*.rs`, outside `#[cfg(test)]` items:

    std::thread::spawn / thread::spawn   is a violation,

unless the previous non-empty line is

    // cordis:allow-raw-thread: <non-empty reason>

The reason is mandatory: an empty marker is a violation too. A spawn whose
backing line is commented out is not code and is ignored; `tests/` directories
are not scanned (the rule is about production plugin code).

Usage
-----
    python3 scripts/check_raw_threads.py [--plugins-root PATH]

Exit status is 0 when the tree is clean, 1 when at least one violation is
found, 2 when the plugins root is missing.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# `thread::spawn` at a token boundary: this also matches `std::thread::spawn`
# and `::thread::spawn`, but not `my_thread::spawn` (preceded by a word char).
SPAWN_RE = re.compile(r"(?<![\w])thread\s*::\s*spawn\b")

# The exact attribute this rule is about. `#[cfg(test)]` on a module, function,
# const, ... makes the whole item test-only.
CFG_TEST_RE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")

# The exemption marker, matched against a line whose comments are preserved.
MARKER_RE = re.compile(r"^\s*//\s*cordis:allow-raw-thread:\s*(.*)$")

# Raw string prefix (`r"..."`, `r#"..."#`, `br"..."`, `cr"..."`).
RAW_STR_RE = re.compile(r'(?:b|c)?r(#*)"')

# One-character char literal (including escapes like `'\n'`, `'\''`).
CHAR_LIT_RE = re.compile(r"'(?:\\.|[^\\'])'")

IDENT_CHARS = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_")

VIOLATION_NO_MARKER = (
    "裸线程未走 ServiceThreads 契约（需在上一非空行标注 "
    "// cordis:allow-raw-thread: <理由>）"
)
VIOLATION_EMPTY_REASON = "豁免标记 // cordis:allow-raw-thread: 缺少非空理由"


def _blank(out: list[str], start: int, end: int) -> None:
    """Replace `out[start:end]` with spaces, preserving line breaks."""
    for index in range(start, end):
        if out[index] != "\n":
            out[index] = " "


def _token_start(source: str, index: int) -> bool:
    """True when `index` begins a token (not the middle of an identifier)."""
    return index == 0 or source[index - 1] not in IDENT_CHARS


def mask_source(source: str, *, keep_line_comments: bool) -> str:
    """Blank comments and literals, keeping the source length and line count.

    Offsets in the returned string are valid offsets into `source`, which is
    what lets the scanner work on "real code" without losing line numbers.
    Block comments nest in Rust, so they are matched with a depth counter.
    """
    out = list(source)
    length = len(source)
    index = 0
    while index < length:
        char = source[index]
        if char == "/" and index + 1 < length:
            nxt = source[index + 1]
            if nxt == "/":
                end = source.find("\n", index)
                if end < 0:
                    end = length
                if not keep_line_comments:
                    _blank(out, index, end)
                index = end
                continue
            if nxt == "*":
                depth = 1
                cursor = index + 2
                while cursor < length and depth:
                    if source.startswith("/*", cursor):
                        depth += 1
                        cursor += 2
                    elif source.startswith("*/", cursor):
                        depth -= 1
                        cursor += 2
                    else:
                        cursor += 1
                _blank(out, index, cursor)
                index = cursor
                continue
        if char == "r" or (
            char in "bc" and index + 1 < length and source[index + 1] == "r"
        ):
            match = RAW_STR_RE.match(source, index)
            if match is not None and _token_start(source, index):
                closing = '"' + match.group(1)
                end = source.find(closing, match.end())
                end = length if end < 0 else end + len(closing)
                _blank(out, index, end)
                index = end
                continue
        if char == '"' or (
            char == "b" and index + 1 < length and source[index + 1] == '"'
        ):
            start = index
            cursor = index + (2 if char == "b" else 1)
            while cursor < length:
                if source[cursor] == "\\":
                    cursor += 2
                    continue
                if source[cursor] == '"':
                    cursor += 1
                    break
                cursor += 1
            _blank(out, start, cursor)
            index = cursor
            continue
        if char == "'" and _token_start(source, index):
            match = CHAR_LIT_RE.match(source, index)
            if match is not None:
                _blank(out, index, match.end())
                index = match.end()
                continue
        index += 1
    return "".join(out)


def _skip_ws(text: str, index: int) -> int:
    while index < len(text) and text[index].isspace():
        index += 1
    return index


def _match_delim(text: str, index: int, open_char: str, close_char: str) -> int:
    """Index of the delimiter closing `text[index]`; `len(text)` when unbalanced."""
    depth = 0
    for cursor in range(index, len(text)):
        char = text[cursor]
        if char == open_char:
            depth += 1
        elif char == close_char:
            depth -= 1
            if depth == 0:
                return cursor
    return len(text)


def _item_end(masked: str, index: int) -> int:
    """End offset of the item starting at the first token after `index`.

    Handles the attribute stack (`#[cfg(test)] #[allow(...)] mod tests { .. }`),
    `mod tests;` declarations, and body-less items ending in `;`.
    """
    length = len(masked)
    index = _skip_ws(masked, index)
    while index < length and masked[index] == "#":
        bracket = _skip_ws(masked, index + 1)
        if bracket >= length or masked[bracket] != "[":
            break
        index = _skip_ws(masked, _match_delim(masked, bracket, "[", "]") + 1)
    start = index
    while index < length and masked[index] in IDENT_CHARS:
        index += 1
    if index == start:
        return start
    depth = 0
    while index < length:
        char = masked[index]
        if char == "{":
            return _match_delim(masked, index, "{", "}") + 1
        if char in "([":
            depth += 1
        elif char in ")]":
            depth = max(depth - 1, 0)
        elif char == ";" and depth == 0:
            return index + 1
        index += 1
    return length


def test_spans(source: str) -> list[tuple[int, int]]:
    """Character spans of every `#[cfg(test)]` item in `source`."""
    masked = mask_source(source, keep_line_comments=False)
    return [(_match_start.start(), _item_end(masked, _match_start.end()))
            for _match_start in CFG_TEST_RE.finditer(masked)]


def _line_starts(source: str) -> list[int]:
    return [0] + [index + 1 for index, char in enumerate(source) if char == "\n"]


def _line_of(line_starts: list[int], offset: int) -> int:
    """1-based line number containing `offset`."""
    lo, hi = 0, len(line_starts)
    while lo + 1 < hi:
        mid = (lo + hi) // 2
        if line_starts[mid] <= offset:
            lo = mid
        else:
            hi = mid
    return lo + 1


def check_file(path: Path) -> dict[str, object]:
    """Classify every raw spawn in one source file.

    Returns `{"violations": [(line, message)], "exemptions": [(line, reason)],
    "raw_spawns": int}` where `raw_spawns` counts only production sites.
    """
    source = path.read_text(encoding="utf-8")
    code = mask_source(source, keep_line_comments=False)
    commented = mask_source(source, keep_line_comments=True)
    source_lines = source.splitlines()
    comment_lines = commented.splitlines()
    spans = test_spans(source)
    line_starts = _line_starts(source)

    violations: list[tuple[int, str]] = []
    exemptions: list[tuple[int, str]] = []
    raw_spawns = 0
    for match in SPAWN_RE.finditer(code):
        offset = match.start()
        if any(start <= offset < end for start, end in spans):
            continue
        raw_spawns += 1
        line = _line_of(line_starts, offset)
        previous = line - 2  # 0-based index of the line above `line`
        while previous >= 0 and not source_lines[previous].strip():
            previous -= 1
        reason = ""
        if previous >= 0:
            marker = MARKER_RE.match(comment_lines[previous])
            if marker is not None:
                reason = marker.group(1).strip()
        if reason:
            exemptions.append((line, reason))
        elif previous >= 0 and MARKER_RE.match(comment_lines[previous]) is not None:
            violations.append((line, VIOLATION_EMPTY_REASON))
        else:
            violations.append((line, VIOLATION_NO_MARKER))
    return {"violations": violations, "exemptions": exemptions, "raw_spawns": raw_spawns}


def display_path(path: Path, plugins_root: Path) -> str:
    """Path relative to the repository root that owns `plugins_root`."""
    try:
        return path.relative_to(plugins_root.parent.parent).as_posix()
    except ValueError:
        return path.as_posix()


def run(plugins_root: Path, out) -> int:
    """Run the check over `plugins_root/**/src/**/*.rs`, returning the exit code."""
    files = sorted(plugins_root.glob("*/src/**/*.rs"))
    violations: list[tuple[str, int, str]] = []
    raw_spawns = 0
    exemptions = 0
    for path in files:
        result = check_file(path)
        raw_spawns += int(result["raw_spawns"])
        exemptions += len(result["exemptions"])
        for line, message in result["violations"]:
            violations.append((display_path(path, plugins_root), line, message))

    for file_name, line, message in violations:
        print(f"{file_name}:{line}: 违规: {message}", file=out)
    print(
        "raw-thread check: "
        f"{len(files)} file(s), {raw_spawns} raw spawn(s) outside #[cfg(test)], "
        f"{len(violations)} violation(s), {exemptions} exemption(s)",
        file=out,
    )
    return 1 if violations else 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    default_root = Path(__file__).resolve().parent.parent / "fixtures" / "plugins"
    parser = argparse.ArgumentParser(
        description="Reject raw std::thread::spawn outside #[cfg(test)] in fixture plugins."
    )
    parser.add_argument(
        "--plugins-root",
        type=Path,
        default=default_root,
        help=f"fixtures/plugins directory (default: {default_root})",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    plugins_root: Path = args.plugins_root
    if not plugins_root.is_dir():
        print(f"raw-thread check: plugins root not found: {plugins_root}", file=sys.stderr)
        return 2
    return run(plugins_root, sys.stdout)


if __name__ == "__main__":
    sys.exit(main())
