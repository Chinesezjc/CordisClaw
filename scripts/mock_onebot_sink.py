#!/usr/bin/env python3
"""Mock OneBot v11 HTTP API sink for CordisClaw QQ 链路验证 (任务 D).

拦截 qq_send / qq_system_notify 的出站调用（send_group_msg /
send_private_msg 等），把请求体落盘到 --dump 指定文件（每行一个 JSON），
并返回 OneBot 成功响应，从而无需真实 QQ 服务端即可断言"回复动作构造正确"。

用法:
    python3 scripts/mock_onebot_sink.py --port 5700 --dump /tmp/qq_sink.jsonl
"""
import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

DUMP_PATH = None


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass  # 静音默认访问日志

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length).decode("utf-8", "replace") if length else ""
        endpoint = self.path.lstrip("/")
        try:
            params = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            params = {"_unparsed": raw}
        record = {"endpoint": endpoint, "params": params,
                  "authorization": self.headers.get("Authorization")}
        if DUMP_PATH:
            with open(DUMP_PATH, "a", encoding="utf-8") as f:
                f.write(json.dumps(record, ensure_ascii=False) + "\n")
        print(f"[sink] {endpoint} <- {json.dumps(params, ensure_ascii=False)}", flush=True)
        # OneBot v11 成功响应：状态 ok，回一个假 message_id。
        body = json.dumps({"status": "ok", "retcode": 0,
                           "data": {"message_id": 424242}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    global DUMP_PATH
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=5700)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--dump", default=None, help="每行一个 JSON 的出站记录文件")
    args = ap.parse_args()
    DUMP_PATH = args.dump
    if DUMP_PATH:
        open(DUMP_PATH, "w").close()  # 清空
    srv = HTTPServer((args.host, args.port), Handler)
    print(f"[sink] mock OneBot listening on {args.host}:{args.port} dump={DUMP_PATH}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        sys.exit(0)


if __name__ == "__main__":
    main()
