#!/usr/bin/env python3
"""Turn recorded agent sessions into labelled regression cases.

A *case* is one turn: the batch the model was given, what it produced, and an
optional verdict. Cases come from `data/sessions/*.json`, which the runtime
writes after every turn, so the input is exactly what the model saw rather than
a re-rendering of it.

Usage:
    python3 evals/extract.py [--out evals/cases.jsonl] [--limit 500] [--session GLOB]

The output is JSONL, one case per line. `verdict.label` is left null for a human
to fill in; `flags` carries the failures the harness can prove from the record
(a tool call that errored, the same message sent twice, a turn the harness had to
nudge into speech).
"""

import argparse
import glob
import json
import os
import sys

# Turns the harness visibly had to push. They are not verdicts — they mark turns
# where a model that had nothing to say was asked to try again.
# Flags that mark a malfunction on their own, with no judgement call.
HARD_FLAGS = {"tool_error", "malformed_call", "duplicate_send"}

# A tool result that says the call itself was wrong, not that the action failed.
MALFORMED_MARKERS = (
    "target is required",
    "is not a live agent session",
    "unknown tool",
    "not a function",
)


def load_turns(path):
    """Yield (input, entries) per turn: one user batch plus what followed."""
    with open(path, encoding="utf-8") as handle:
        session = json.load(handle)
    turn = None
    for entry in session.get("transcript", []):
        if entry.get("kind") == "user":
            if turn:
                yield turn
            turn = {"input": entry.get("content", ""), "entries": []}
        elif turn is not None:
            turn["entries"].append(entry)
    if turn:
        yield turn


def flags_for(turn):
    flags = []
    sent = []
    for entry in turn["entries"]:
        if entry.get("kind") != "tool":
            continue
        output = json.dumps(entry.get("output", {}), ensure_ascii=False)
        if entry.get("ok") is False or '"ok":false' in output.replace(" ", ""):
            flags.append("tool_error")
            flat = output.replace(" ", "")
            if any(marker.replace(" ", "") in flat for marker in MALFORMED_MARKERS):
                flags.append("malformed_call")
        arguments = entry.get("arguments") or {}
        payload = arguments.get("payload_json") or {}
        if isinstance(payload, dict) and isinstance(payload.get("message"), str):
            sent.append(payload["message"])
        if isinstance(arguments.get("message"), str):
            sent.append(arguments["message"])
    if len(sent) != len(set(sent)):
        flags.append("duplicate_send")
    if not sent:
        flags.append("no_message_sent")
    # Forced speech is not visible here: the harness keeps its nudges in the
    # live message list only. The journal does record it
    # (`agent_ended_without_action`, `agent_missing_end_tool`), so those turns
    # have to be joined from there — see evals/README.md.
    return sorted(set(flags))


def context_for(previous, depth=6):
    """The turns before this one, oldest first, so a case is self-contained."""
    return [
        {"input": turn["input"], "text": text}
        for turn, text in previous[-depth:]
    ]


def summarise(turn):
    """The model-visible output: text it wrote and the calls it made."""
    calls = []
    text = None
    reasoning_text = None
    for entry in turn["entries"]:
        kind = entry.get("kind")
        if kind == "tool":
            calls.append({
                "name": entry.get("name"),
                "arguments": entry.get("arguments"),
                "ok": entry.get("ok"),
            })
        elif kind == "assistant":
            content = entry.get("content")
            if isinstance(content, str) and content.strip() and content.strip() != '{"action":"suspend"}':
                text = content
            reasoning = entry.get("reasoning")
            if isinstance(reasoning, str) and reasoning.strip():
                reasoning_text = reasoning
    return calls, text, reasoning_text


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", default="evals/cases.jsonl")
    parser.add_argument("--limit", type=int, default=0, help="0 = every turn")
    parser.add_argument("--session", default="data/sessions/*.json")
    parser.add_argument(
        "--only-flagged",
        action="store_true",
        help="write only cases whose flags prove a malfunction",
    )
    args = parser.parse_args()

    paths = sorted(glob.glob(args.session))
    if not paths:
        sys.exit(f"no session files match {args.session}")

    written = 0
    flagged = {}
    with open(args.out, "w", encoding="utf-8") as out:
        for path in paths:
            session_name = os.path.basename(path).removesuffix(".json")
            try:
                turns = list(load_turns(path))
            except (json.JSONDecodeError, OSError):
                continue
            history = []
            for index, turn in enumerate(turns):
                flags = flags_for(turn)
                if args.only_flagged and not (set(flags) & HARD_FLAGS):
                    continue
                for flag in flags:
                    flagged[flag] = flagged.get(flag, 0) + 1
                calls, text, reasoning = summarise(turn)
                context = context_for(history)
                history.append((turn, text))
                out.write(json.dumps({
                    "id": f"{session_name}-{index}",
                    "session": session_name,
                    "context": context,
                    "input": turn["input"],
                    "calls": calls,
                    "text": text,
                    "reasoning": reasoning,
                    "flags": flags,
                    "verdict": {"label": None, "failure": None, "expected": None},
                }, ensure_ascii=False) + "\n")
                written += 1
                if args.limit and written >= args.limit:
                    break
            if args.limit and written >= args.limit:
                break

    print(f"{written} cases -> {args.out}")
    for flag, count in sorted(flagged.items(), key=lambda kv: -kv[1]):
        print(f"  {count:5d}  {flag}")


if __name__ == "__main__":
    main()
