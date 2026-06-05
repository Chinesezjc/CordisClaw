#!/usr/bin/env python3
"""CordisClaw QQ Agent — drives the serve REPL to handle QQ messages.

Architecture:
  CordisClaw serve REPL (stdin/stdout pipe)
       │
       ▼
  qq_agent.py
       │
       ├── Boot: invoke qq_serve to start HTTP server
       │
       └── Poll loop (every 5s):
            ├── invoke qq_fetch_messages
            ├── for each message:
            │     ├── send to LLM (CordisClaw agent session)
            │     └── if not IGNORE: invoke qq_send to reply
            └── sleep

Usage:
  python3 scripts/qq_agent.py [--cordisclaw-bin ./target/debug/cordis-runtime]
                              [--fixtures-root fixtures]
                              [--port 8080]
                              [--onebot-url http://127.0.0.1:5700]
                              [--enable-llm]   # send to LLM agent (needs API key)
"""

import subprocess
import sys
import os
import json
import time
import argparse
import re
import signal
from datetime import datetime

# ── Config ────────────────────────────────────────────────────────────────

parser = argparse.ArgumentParser(description="CordisClaw QQ Agent")
parser.add_argument("--cordisclaw-bin", default="./target/debug/cordis-runtime")
parser.add_argument("--fixtures-root", default="fixtures")
parser.add_argument("--port", type=int, default=8080)
parser.add_argument("--onebot-url", default="http://127.0.0.1:5700")
parser.add_argument("--allow-groups", default="", help="Comma-separated group IDs")
parser.add_argument("--poll-interval", type=int, default=5, help="Seconds between polls")
parser.add_argument("--enable-llm", action="store_true", help="Process messages through LLM agent")
args = parser.parse_args()

ALLOW_GROUPS = [g.strip() for g in args.allow_groups.split(",") if g.strip()]

# ── REPL Driver ────────────────────────────────────────────────────────────

class CordisClawRepl:
    """Drives the CordisClaw serve REPL via stdin/stdout."""

    def __init__(self, bin_path, fixtures_root):
        self.proc = subprocess.Popen(
            [bin_path, "serve", fixtures_root, "--runtime-only"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        self.buffer = ""
        self._wait_ready()

    def _wait_ready(self):
        """Wait until the REPL prints 'serve ready'."""
        deadline = time.time() + 30
        while time.time() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                time.sleep(0.1)
                continue
            print(f"[cordisclaw] {line.rstrip()}", file=sys.stderr)
            if "serve ready" in line:
                return
        raise TimeoutError("CordisClaw did not become ready within 30s")

    def _read_response(self, timeout=10):
        """Read the response to a command. Returns the JSON or text output."""
        lines = []
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                time.sleep(0.05)
                continue
            line = line.rstrip()
            # The REPL echoes the command prompt; skip it.
            if line.startswith("⚙") or line.startswith(">") or line.startswith(">>"):
                continue
            if line:
                lines.append(line)
                # If we got a JSON-like response, return it.
                if line.startswith("{") or line.startswith("invoke"):
                    break
        return "\n".join(lines)

    def invoke(self, plugin_path, node_id, payload):
        """Send an invoke command to the REPL.

        The REPL uses '>' prefix for command mode.
        invoke syntax: invoke <plugin_path> <node_id> --payload-json=<json>
        """
        payload_str = json.dumps(payload)
        cmd = f">invoke {plugin_path} {node_id} --payload-json='{payload_str}'\n"
        self.proc.stdin.write(cmd)
        self.proc.stdin.flush()
        return self._read_response()

    def agent_send(self, message):
        """Send a message to the agent session in the REPL.

        The REPL uses '>>' prefix for agent mode.
        """
        self.proc.stdin.write(f"{message}\n")
        self.proc.stdin.flush()
        return self._read_response(timeout=60)

    def send_command(self, cmd_text):
        """Send a raw command line (with '>' prefix)."""
        self.proc.stdin.write(f">{cmd_text}\n")
        self.proc.stdin.flush()
        return self._read_response()

    def stop(self):
        self.proc.terminate()
        self.proc.wait(timeout=5)


# ── Main ───────────────────────────────────────────────────────────────────

def parse_invoke_response(raw: str) -> dict:
    """Try to extract JSON from invoke response."""
    # The output looks like: "invoke ok=true exit_code=null message=..."
    # or it's a JSON blob.
    result = {"ok": False, "messages": [], "message": "", "data": None}

    if not raw:
        return result

    # Try plain JSON first.
    try:
        parsed = json.loads(raw)
        return parsed
    except json.JSONDecodeError:
        pass

    # Try the key=value format.
    for part in raw.split():
        if "=" in part:
            key, value = part.split("=", 1)
            result[key.strip()] = value.strip()

    result["ok"] = result.get("ok") == "true"
    return result


def main():
    print(f"[qq_agent] Starting CordisClaw...", file=sys.stderr)

    repl = CordisClawRepl(args.cordisclaw_bin, args.fixtures_root)

    # Step 1: Start qq_serve.
    print(f"[qq_agent] Starting qq_serve on port {args.port}...", file=sys.stderr)
    serve_payload = {
        "node_id": "qq_serve",
        "payload": {
            "port": args.port,
            "onebot_url": args.onebot_url,
            "allow_groups": ALLOW_GROUPS,
        },
    }
    resp = repl.invoke("qq", "qq_serve", serve_payload)
    print(f"[qq_agent] qq_serve: {resp}", file=sys.stderr)

    # Step 2: Enter polling loop.
    print(f"[qq_agent] Entering poll loop (interval={args.poll_interval}s)...", file=sys.stderr)

    processed_ids = set()  # Track processed message_ids to avoid duplicates

    while True:
        try:
            # Poll for messages.
            resp = repl.invoke("qq", "qq_fetch_messages", {"node_id": "qq_fetch_messages"})
            result = parse_invoke_response(resp)

            messages = result.get("messages", [])
            if not messages:
                time.sleep(args.poll_interval)
                continue

            if isinstance(messages, str):
                try:
                    messages = json.loads(messages)
                except json.JSONDecodeError:
                    messages = []

            for msg in messages:
                msg_id = msg.get("raw_event", {}).get("message_id", "")
                if msg_id and msg_id in processed_ids:
                    continue
                if msg_id:
                    processed_ids.add(msg_id)
                    if len(processed_ids) > 1000:
                        processed_ids = set(list(processed_ids)[-500:])

                text = msg.get("message", "")
                sender_id = msg.get("sender_id", "")
                msg_type = msg.get("message_type", "group")

                timestamp = datetime.now().strftime("%H:%M:%S")
                print(f"\n[{timestamp}] [{msg_type}:{sender_id}] {text}", file=sys.stderr)

                if args.enable_llm:
                    # Send to agent for processing.
                    agent_input = (
                        f"[QQ {msg_type} message from {sender_id}]: {text}\n\n"
                        f"Only respond if this message is directed at you. "
                        f"If not, reply with exactly IGNORE."
                    )
                    agent_resp = repl.agent_send(agent_input)
                    print(f"[qq_agent] agent: {agent_resp[:200]}...", file=sys.stderr)

                    # If agent responded with something other than IGNORE, send to QQ.
                    if "IGNORE" not in agent_resp[:50]:
                        target = f"group:{sender_id}" if msg_type == "group" else f"private:{sender_id}"
                        reply_payload = {
                            "node_id": "qq_send",
                            "target": target,
                            "message": agent_resp.strip(),
                        }
                        send_resp = repl.invoke("qq", "qq_send", reply_payload)
                        print(f"[qq_agent] sent reply: {send_resp}", file=sys.stderr)
                else:
                    # Pass-through mode: echo received messages (for testing).
                    print(f"  → message received (LLM disabled)", file=sys.stderr)

        except KeyboardInterrupt:
            print("\n[qq_agent] Shutting down...", file=sys.stderr)
            break
        except Exception as e:
            print(f"[qq_agent] Error: {e}", file=sys.stderr)
            time.sleep(args.poll_interval)

    repl.stop()
    print("[qq_agent] Done.", file=sys.stderr)


if __name__ == "__main__":
    main()
