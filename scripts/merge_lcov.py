#!/usr/bin/env python3
"""Merge llvm-cov LCOV fragments produced by parallel coverage shards.

Why this exists
---------------
`cargo llvm-cov` merges every test binary it runs *itself* (one `llvm-profdata
merge` + one `llvm-cov export`), but the coverage workflow now splits the test
binaries across several GitHub Actions jobs, each on its own runner.  A job can
therefore only export the coverage of the binaries it ran, and the 100% line
gate needs the union of all shards.

Merge semantics
---------------
For every source file (`SF:`) and every line (`DA:<line>,<count>` we keep the
**maximum** hit count seen in any fragment:

    merged[file][line] = max(count for each fragment that reported the line)

That is the correct semantics for combining several *test binaries* (or
several runs of the same binary): a line counts as executed as soon as one
test executed it, and no single shard may contribute more than "covered" to a
line.  Summing counts instead would still satisfy the binary >0 gate, but it
would invent execution counts that no test performed, and it would make the
merged numbers depend on how many shards happen to touch a file — max keeps the
result identical to the single-run merge that `llvm-cov` itself performs.

Records other than `DA:` are dropped on purpose: function/branch aggregates
(`FN`, `FNDA`, `FNF`, `FNH`, `BRDA`, `BRF`, `BRH`) cannot be merged correctly
outside `llvm-profdata` (a function's hit count only exists per profdata
merge), and the gate is line-only.  `LF:`/`LH:` are recomputed from the merged
`DA:` lines so the output stays a well-formed lcov file.

Usage
-----
    python3 scripts/merge_lcov.py -o merged.info fragments/*.info

    # fail loudly when a shard's artifact never arrived:
    python3 scripts/merge_lcov.py -o merged.info \
        --expect lcov-agent-runtime-host.info --expect lcov-runtime.info \
        fragments/*.info
"""

from __future__ import annotations

import argparse
import os
import sys
from collections import OrderedDict

class Fragment:
    """Parsed contents of a single lcov file."""

    def __init__(self, path: str) -> None:
        self.path = path
        self.files: "OrderedDict[str, dict[int, int]]" = OrderedDict()

    @property
    def lines(self) -> int:
        return sum(len(v) for v in self.files.values())

    @property
    def covered(self) -> int:
        return sum(1 for v in self.files.values() for hits in v.values() if hits > 0)


def normalize(path: str, root: str, warn: list[str]) -> str:
    """Turn a reported source path into a repository-relative one.

    llvm-cov reports paths relative to the compilation working directory, but
    a `--manifest-path`/nested build can also surface absolute paths.  The
    shards all run in the same checkout layout, so normalizing to a path
    relative to `--root` keeps one file from showing up twice under two
    spellings (which would double-count its lines in the gate).
    """
    path = path.strip()
    if not path:
        return path
    if os.path.isabs(path):
        candidate = os.path.relpath(path, root)
        if not candidate.startswith(".."):
            return candidate.replace(os.sep, "/")
        warn.append(path)
        return path.replace(os.sep, "/")
    if path.startswith("./"):
        path = path[2:]
    return path.replace(os.sep, "/")


def parse(fragment: Fragment, root: str, warn: list[str]) -> None:
    current: "dict[int, int] | None" = None
    with open(fragment.path, encoding="utf-8", errors="replace") as handle:
        for raw in handle:
            line = raw.strip()
            if not line:
                continue
            if line.startswith("SF:"):
                current = fragment.files.setdefault(
                    normalize(line[3:], root, warn), {}
                )
            elif line.startswith("DA:"):
                if current is None:
                    raise SystemExit(
                        f"{fragment.path}: DA record before any SF record: {line!r}"
                    )
                fields = line[3:].split(",")
                if len(fields) < 2:
                    raise SystemExit(f"{fragment.path}: malformed DA record: {line!r}")
                try:
                    number, hits = int(fields[0]), int(fields[1])
                except ValueError:
                    raise SystemExit(f"{fragment.path}: malformed DA record: {line!r}")
                # Keep zero-hit lines too: a line that no shard executed must
                # stay in the merged file so the gate can report it.
                if number not in current or hits > current[number]:
                    current[number] = hits
            elif line == "end_of_record":
                current = None
            # TN:, FN:, FNDA:, FNF:, FNH:, BRDA:, BRF:, BRH:, VER:, ...
            # carry no line-level information we need; skip them.


def merge(fragments: list[Fragment]) -> "OrderedDict[str, dict[int, int]]":
    merged: "OrderedDict[str, dict[int, int]]" = OrderedDict()
    for fragment in fragments:
        for source, lines in fragment.files.items():
            target = merged.setdefault(source, {})
            for number, hits in lines.items():
                if number not in target or hits > target[number]:
                    target[number] = hits
    return merged


def pct(covered: int, total: int) -> float:
    return 100.0 * covered / total if total else 0.0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Merge llvm-cov lcov fragments (max hit count per line)."
    )
    parser.add_argument("fragments", nargs="+", help="lcov fragment files")
    parser.add_argument(
        "-o", "--output", default="-", help="merged lcov output path ('-' = stdout)"
    )
    parser.add_argument(
        "--root",
        default=os.getcwd(),
        help="checkout root used to make absolute SF: paths relative (default: cwd)",
    )
    parser.add_argument(
        "--expect",
        action="append",
        default=[],
        metavar="NAME",
        help="fragment basename that must be present (repeatable); "
        "guards against a shard whose artifact never arrived",
    )
    parser.add_argument(
        "--quiet", action="store_true", help="only print the merged summary"
    )
    args = parser.parse_args(argv)

    # De-duplicate while keeping the caller's order, so `frag-*.info` globs
    # cannot merge the same shard twice.
    paths: list[str] = []
    seen: set[str] = set()
    for path in args.fragments:
        real = os.path.realpath(path)
        if real not in seen:
            seen.add(real)
            paths.append(path)

    missing = [name for name in args.expect if not any(
        os.path.basename(p) == name for p in paths
    )]
    if missing:
        print(
            "::error::missing lcov fragment(s) %s — a coverage shard did not "
            "upload its artifact; the merged result would under-report coverage. "
            "Found: %s"
            % (", ".join(missing), ", ".join(sorted(os.path.basename(p) for p in paths)) or "none")
        )
        return 1

    root = os.path.abspath(args.root)
    warn: list[str] = []
    fragments: list[Fragment] = []
    for path in paths:
        fragment = Fragment(path)
        parse(fragment, root, warn)
        if fragment.lines == 0:
            print(
                f"::error::{path} contains no DA: lines — an empty coverage "
                "fragment means the shard produced no profile (build or test "
                "failure), so the merge would silently lose its coverage"
            )
            return 1
        fragments.append(fragment)

    merged = merge(fragments)

    overlaps = 0
    for source, lines in merged.items():
        hits_in = sum(
            1
            for number in lines
            if sum(1 for f in fragments if f.files.get(source, {}).get(number, 0) > 0)
            > 1
        )
        overlaps += hits_in

    if not args.quiet:
        print("lcov fragments (max-merged, not summed):")
        for fragment in fragments:
            print(
                "  %-32s files=%3d lines=%6d covered=%6d (%.2f%% of its own lines)"
                % (
                    os.path.basename(fragment.path),
                    len(fragment.files),
                    fragment.lines,
                    fragment.covered,
                    pct(fragment.covered, fragment.lines),
                )
            )

    total = sum(len(v) for v in merged.values())
    covered = sum(1 for v in merged.values() for hits in v.values() if hits > 0)

    if args.output == "-":
        write(sys.stdout, merged)
    else:
        with open(args.output, "w", encoding="utf-8") as handle:
            write(handle, merged)

    print(
        "merged: files=%d lines=%d covered=%d (%.4f%%)"
        % (len(merged), total, covered, pct(covered, total)),
        file=sys.stderr if args.output == "-" else sys.stdout,
    )
    if not args.quiet:
        print(
            "  %d line(s) were executed by more than one fragment "
            "(kept their max count)" % overlaps
        )
    if warn:
        print(
            "::warning::%d source path(s) are absolute but outside --root=%s; "
            "they were kept verbatim: %s" % (len(warn), root, sorted(set(warn))[:5])
        )
    return 0


def write(handle, merged: "OrderedDict[str, dict[int, int]]") -> None:
    for source in sorted(merged):
        lines = merged[source]
        handle.write(f"SF:{source}\n")
        for number in sorted(lines):
            handle.write(f"DA:{number},{lines[number]}\n")
        handle.write(f"LF:{len(lines)}\n")
        handle.write(f"LH:{sum(1 for hits in lines.values() if hits > 0)}\n")
        handle.write("end_of_record\n")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
