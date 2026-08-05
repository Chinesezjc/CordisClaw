#!/usr/bin/env bash
# 通过 WebSocket 向 qq_ws_serve 推一条 OneBot v11 群消息事件 (WS 链路 e2e).
#
# qq_ws_serve 是一个原生 tungstenite WS 服务端：客户端连上后直接发送
# OneBot v11 事件 JSON 文本帧（与 HTTP /onebot/event 同格式），服务端
# 解析后按 message_id 去重并入队。WS 路径不做签名校验，因此本脚本
# 不需要 openssl，也没有 --bad-sig / --no-sig 分支。
#
# 依赖仅 python3 标准库：内嵌一个最小 WS 客户端（RFC6455 握手 + 单个
# masked 文本帧发送 + 干净的 close 帧）。不引入 websocket-client 等外部包。
# 服务端一次只处理一个连接，且 handle_ws_connection 会阻塞读到 Close/错误
# 才返回 accept()，因此本脚本每次都发送 close 帧，避免占死连接。
#
# 用法:
#   scripts/send_onebot_ws_event.sh [-p PORT] [-g GROUP_ID] [-m MSG] [-i MSG_ID] [-u USER_ID] [--probe]
#
# --probe: 只做 WS 握手（不发事件），用于就绪/端口连通探测；握手成功退出 0。
set -euo pipefail

PORT=8002
GROUP_ID=123456
MSG="bot 你好，报一下你的状态"
MSG_ID="$(date +%s)"      # 默认用时间戳当 message_id，保证每次不同
USER_ID=10001
PROBE=0

while [ $# -gt 0 ]; do
  case "$1" in
    -p|--port) PORT="$2"; shift 2;;
    -g|--group) GROUP_ID="$2"; shift 2;;
    -m|--message) MSG="$2"; shift 2;;
    -i|--msg-id) MSG_ID="$2"; shift 2;;
    -u|--user) USER_ID="$2"; shift 2;;
    --probe) PROBE=1; shift;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done

# OneBot v11 群消息事件（结构化 message segments），与 HTTP 路径同格式。
BODY=$(printf '{"post_type":"message","message_type":"group","sub_type":"normal","message_id":%s,"group_id":%s,"user_id":%s,"raw_message":"%s","message":[{"type":"text","data":{"text":"%s"}}],"sender":{"user_id":%s,"nickname":"tester"}}' \
  "$MSG_ID" "$GROUP_ID" "$USER_ID" "$MSG" "$MSG" "$USER_ID")

if [ "$PROBE" = "1" ]; then
  echo "[ws] PROBE handshake -> ws://127.0.0.1:${PORT}/"
else
  echo "[ws] SEND event -> ws://127.0.0.1:${PORT}/ group=$GROUP_ID msg_id=$MSG_ID"
fi

# 最小 WS 客户端：环境变量传参，避免 shell 引号转义问题。
WS_HOST=127.0.0.1 WS_PORT="$PORT" WS_PROBE="$PROBE" WS_BODY="$BODY" python3 - <<'PY'
import base64
import os
import socket
import struct
import sys

host = os.environ["WS_HOST"]
port = int(os.environ["WS_PORT"])
probe = os.environ.get("WS_PROBE", "0") == "1"
body = os.environ.get("WS_BODY", "")


def ws_handshake(sock):
    """发送 RFC6455 客户端握手，校验 101 响应。"""
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        "GET / HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "\r\n"
    )
    sock.sendall(req.encode("ascii"))
    resp = b""
    while b"\r\n\r\n" not in resp:
        chunk = sock.recv(4096)
        if not chunk:
            break
        resp += chunk
    status = resp.split(b"\r\n", 1)[0].decode("ascii", "replace")
    if "101" not in status:
        raise RuntimeError(f"handshake failed: {status!r}")


def ws_send_text(sock, text):
    """发送单个 masked 文本帧（客户端帧必须 mask）。"""
    payload = text.encode("utf-8")
    header = bytearray([0x81])  # FIN=1 + opcode=0x1 (text)
    n = len(payload)
    if n < 126:
        header.append(0x80 | n)
    elif n < 65536:
        header.append(0x80 | 126)
        header += struct.pack(">H", n)
    else:
        header.append(0x80 | 127)
        header += struct.pack(">Q", n)
    mask = os.urandom(4)
    header += mask
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(bytes(header) + masked)


def ws_close(sock):
    """发送 masked close 帧，让服务端 handle_ws_connection 干净返回 accept()。"""
    try:
        mask = os.urandom(4)
        sock.sendall(bytes([0x88, 0x80]) + mask)  # opcode=0x8 close, len=0, masked
    except OSError:
        pass


try:
    sock = socket.create_connection((host, port), timeout=5)
except OSError as e:
    print(f"[ws] connect failed: {e}", file=sys.stderr)
    sys.exit(1)

try:
    sock.settimeout(5)
    ws_handshake(sock)
    if not probe:
        ws_send_text(sock, body)
    ws_close(sock)
except Exception as e:  # noqa: BLE001 — 探测脚本，任何失败都以非零退出反馈
    print(f"[ws] error: {e}", file=sys.stderr)
    sys.exit(1)
finally:
    try:
        sock.close()
    except OSError:
        pass

print("[ws] ok")
PY
