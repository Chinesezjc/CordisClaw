#!/usr/bin/env python3
"""Rewrite stored agent sessions from the retired JSON protocol to the DSML dialect.

The model imitates history, so a snapshot that still says

    {"action": "respond", "message": "……"}
    <|DSML|tool_calls>{"node_id": "qq_send", "payload_json": {...}}</|DSML|tool_calls>

keeps teaching the next turn a protocol the harness no longer speaks. This
script rewrites those entries into the dialect `crates/cordis-agent/src/dsml.rs`
parses and renders, and compacts the handful of sessions that grew past
`--keep` messages.

It only touches `history`; `transcript` is the record of what actually happened
and stays as it is.

Usage:
    python3 scripts/compact_sessions.py --dry-run
    python3 scripts/compact_sessions.py
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
import time

SESSIONS = "data/sessions"
DEFAULT_KEEP = 400

ACTION_RESPOND = re.compile(r'\{\s*"action"\s*:\s*"respond"\s*,\s*"message"\s*:\s*"((?:[^"\\]|\\.)*)"\s*\}', re.S)
ACTION_SUSPEND = re.compile(r'\{\s*"action"\s*:\s*"suspend"\s*\}')
NODE_SUSPEND = re.compile(r'\{\s*"node_id"\s*:\s*"suspend"\s*\}')
DSML_BLOCK = re.compile(r"<[|｜]{1,2}DSML[|｜]{1,2}\s*tool_calls\s*>(.*?)</[|｜]{1,2}DSML[|｜]{1,2}\s*tool_calls\s*>", re.S)


def json_objects(text: str):
    """Every top-level JSON object in `text`, tolerating trailing junk."""
    values, depth, start, in_string, escaped = [], 0, None, False, False
    for index, char in enumerate(text):
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            continue
        if char == '"':
            in_string = True
        elif char == "{":
            if depth == 0:
                start = index
            depth += 1
        elif char == "}":
            if depth:
                depth -= 1
                if depth == 0 and start is not None:
                    chunk = text[start : index + 1]
                    try:
                        values.append(json.loads(chunk))
                    except json.JSONDecodeError:
                        # A bare newline inside a string is the common breakage;
                        # escape it and try once more.
                        repaired = chunk.replace("\n", "\\n").replace("\r", "")
                        try:
                            values.append(json.loads(repaired))
                        except json.JSONDecodeError:
                            pass
                    start = None
    return values


def render_call(name: str, arguments) -> str:
    lines = [f'<|DSML|invoke name="{name}">']
    if isinstance(arguments, dict):
        for key, value in arguments.items():
            if isinstance(value, str):
                lines.append(
                    f'<|DSML|parameter name="{key}" string="true">{value}</|DSML|parameter>'
                )
            else:
                lines.append(
                    f'<|DSML|parameter name="{key}">{json.dumps(value, ensure_ascii=False)}</|DSML|parameter>'
                )
    lines.append("</|DSML|invoke>")
    return "\n".join(lines)


def render_block(calls) -> str:
    return "<|DSML|calls>\n" + "\n".join(render_call(name, args) for name, args in calls) + "\n</|DSML|calls>"


def legacy_calls(content: str):
    """Calls encoded in the retired JSON form, in order."""
    calls = []
    for match in DSML_BLOCK.finditer(content):
        for value in json_objects(match.group(1)):
            if not isinstance(value, dict):
                continue
            node = value.get("node_id")
            if not isinstance(node, str) or not node:
                continue
            payload = value.get("payload_json")
            if not isinstance(payload, dict):
                payload = {k: v for k, v in value.items() if k not in ("node_id", "plugin_path")}
            calls.append((node, payload))
    return calls


def rewrite(content: str):
    """`content` in the DSML dialect, or None when it carried no protocol."""
    if not isinstance(content, str):
        return None
    changed = False

    block = DSML_BLOCK.search(content)
    if block:
        calls = legacy_calls(content)
        if calls:
            content = (content[: block.start()] + "\n" + render_block(calls) + "\n" + content[block.end() :]).strip()
            changed = True

    # Old batch wrappers embed the retired instruction as prose; rewriting it
    # into a call block would put markup inside the user's own message.
    if "CURRENT_INCOMING_BATCH_START" in content:
        updated = NODE_SUSPEND.sub("end the turn by calling the suspend tool", content)
        updated = ACTION_SUSPEND.sub("end the turn by calling the suspend tool", updated)
        return updated if updated != content else None

    if NODE_SUSPEND.search(content):
        content = NODE_SUSPEND.sub(render_block([("suspend", {})]), content)
        changed = True

    if ACTION_SUSPEND.search(content):
        content = ACTION_SUSPEND.sub(render_block([("suspend", {})]), content)
        changed = True

    def respond(match):
        raw = match.group(1)
        try:
            return json.loads('"' + raw + '"')
        except json.JSONDecodeError:
            return raw

    if ACTION_RESPOND.search(content):
        content = ACTION_RESPOND.sub(respond, content)
        changed = True

    return content if changed else None


def compact(history, keep: int):
    """Keep the newest `keep` messages, summarising the rest into one line."""
    if len(history) <= keep:
        return history, 0
    dropped = history[: len(history) - keep]
    kept = history[len(history) - keep :]
    summary = (
        f"[compact] {len(dropped)} earlier messages were compacted out of this "
        f"conversation. They predate the current tool protocol; nothing from them "
        f"is needed to continue."
    )
    return [{"role": "assistant", "content": summary}] + kept, len(dropped)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--keep", type=int, default=DEFAULT_KEEP)
    parser.add_argument("--root", default=SESSIONS)
    args = parser.parse_args()

    files = sorted(
        path
        for path in os.listdir(args.root)
        if path.endswith(".json") and path != "index.json"
    )
    if not files:
        print(f"no sessions under {args.root}", file=sys.stderr)
        return 1

    backup = f"{args.root}.backup-{time.strftime('%Y%m%d-%H%M%S')}"
    if not args.dry_run:
        shutil.copytree(args.root, backup)
        print(f"backup: {backup}")

    rewritten = compacted = sessions_changed = 0
    for name in files:
        path = os.path.join(args.root, name)
        with open(path, encoding="utf-8") as handle:
            try:
                snapshot = json.load(handle)
            except json.JSONDecodeError as error:
                print(f"skip {name}: {error}", file=sys.stderr)
                continue
        history = snapshot.get("history") or []
        changed = False
        for entry in history:
            new = rewrite(entry.get("content"))
            if new is not None and new != entry.get("content"):
                entry["content"] = new
                rewritten += 1
                changed = True
        history, dropped = compact(history, args.keep)
        if dropped:
            snapshot["history"] = history
            compacted += dropped
            changed = True
        if changed:
            sessions_changed += 1
            if not args.dry_run:
                tmp = f"{path}.tmp"
                with open(tmp, "w", encoding="utf-8") as handle:
                    json.dump(snapshot, handle, ensure_ascii=False)
                os.replace(tmp, path)

    verb = "would rewrite" if args.dry_run else "rewrote"
    print(f"{verb} {rewritten} entries across {sessions_changed} sessions; compacted {compacted} messages")
    return 0


if __name__ == "__main__":
    sys.exit(main())
