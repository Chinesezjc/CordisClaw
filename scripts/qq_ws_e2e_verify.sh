#!/usr/bin/env bash
# QQ WebSocket 接收链路端到端验证 —— 不消耗 token 的分段 (WS 版, 对照 qq_e2e_verify.sh).
#
# qq_ws_serve 用原生 tungstenite 起一个 WS 服务端，客户端连上后直接发送
# OneBot v11 事件 JSON 文本帧（与 HTTP /onebot/event 同格式），服务端解析
# 后按 message_id 去重并入队，poller 线程消费队列。WS 路径不做 HMAC 签名，
# 所以本脚本只覆盖 HTTP 版的 A/B/E 段等价物，没有签名相关的 C/D 段。
#
# 编排：
#   1. 用一份临时 startup_invoke 启用 qq_ws_serve（随机高位端口）
#   2. 启动 cordis-runtime serve --runtime-only
#   3. 分段验证:
#      A. WS 端口可握手        → serve 起来了且能完成 RFC6455 升级
#      B. 合法事件帧被接收      → serve 日志出现连接/入队痕迹
#      C. 重复 message_id       → 传输层每次都接受，去重由队列层保证（见单测）
#      D. 进程退出后端口释放    → serve 退出后端口不再监听（无 SIGTERM handler，
#                                 靠进程退出回收 fd；优雅 stop 路径由单测覆盖）
#   4. 出站段（消耗 token）沿用 HTTP 版 qq_e2e_send_probe.sh，此脚本只覆盖
#      入口到队列的段（不消耗 token）。
#
# 用法: scripts/qq_ws_e2e_verify.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BIN="$PROJECT_DIR/target/debug/cordis-runtime"
# 随机高位端口，降低与其它进程/并行测试撞端口的概率。
PORT="${QQ_WS_PORT:-$(( (RANDOM % 20000) + 40000 ))}"
SERVE_LOG="/tmp/qq_ws_serve_$$.log"

PASS=0; FAIL=0
ok()   { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

cleanup() {
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null
  wait 2>/dev/null
  [ -n "${TMP_FIX:-}" ] && rm -rf "$TMP_FIX"
}
trap cleanup EXIT INT TERM

# 端口占用检查
if ss -tlnp 2>/dev/null | grep -q ":$PORT "; then
  echo "端口 $PORT 已被占用；设 QQ_WS_PORT 换端口后重试" >&2; exit 2
fi

echo "=== 1. 用临时 startup_invoke 启动 serve (WS :$PORT) ==="
# 临时 fixtures：symlink 复用真实 fixtures 的其它内容，仅覆盖 startup_invoke.json
# 指向 qq_ws_serve + 测试端口。这样其它插件/config 仍走真实路径。
TMP_FIX="/tmp/qq_ws_fix_$$"
mkdir -p "$TMP_FIX"
for f in "$PROJECT_DIR"/fixtures/*; do
  ln -s "$f" "$TMP_FIX/$(basename "$f")" 2>/dev/null
done
rm -f "$TMP_FIX/startup_invoke.json"
cat > "$TMP_FIX/startup_invoke.json" <<JSON
[
  {
    "plugin_path": "qq",
    "node_id": "qq_ws_serve",
    "payload": {
      "node_id": "qq_ws_serve",
      "payload": {
        "port": $PORT
      }
    }
  }
]
JSON

CORDIS_FIXTURES_ROOT="$TMP_FIX" "$BIN" serve "$TMP_FIX" --runtime-only >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!

# 等 WS 服务端起来：WS 无 /health，靠握手探测就绪（最多 30s）。
READY=0
for i in $(seq 1 30); do
  if "$SCRIPT_DIR/send_onebot_ws_event.sh" -p "$PORT" --probe >/dev/null 2>&1; then READY=1; break; fi
  sleep 1
done

echo "=== 2. 分段验证 ==="
# A. WS 端口可握手（就绪探测本身即 A 段断言）
if [ "$READY" = "1" ]; then ok "A 段 WS 端口可完成握手 (:$PORT)"; else bad "A 段 WS 未就绪，见 $SERVE_LOG"; fi

# B. 合法 OneBot 群消息事件帧被接收
#    WS 路径入队本身不打专门日志（handle_onebot_event 静默入队），可观测的
#    接收痕迹是 accept 后的连接日志 "[qq] WebSocket client connected"。
#    就绪探测(A)已产生过一条 connected，故这里比较发送前后的计数增量，
#    把信号绑定到 B 帧自身的连接，而非任意历史连接。
CONN_BEFORE=$(grep -c "WebSocket client connected" "$SERVE_LOG" 2>/dev/null || echo 0)
"$SCRIPT_DIR/send_onebot_ws_event.sh" -p "$PORT" -g 123456 -m "bot hi" -i 900001 >/dev/null 2>&1
RECV=0
for i in $(seq 1 5); do
  CONN_NOW=$(grep -c "WebSocket client connected" "$SERVE_LOG" 2>/dev/null || echo 0)
  if [ "$CONN_NOW" -gt "$CONN_BEFORE" ]; then RECV=1; break; fi
  sleep 1
done
if [ "$RECV" = "1" ]; then ok "B 段 合法事件帧被接收（新增一次 WS 连接受理）"; else bad "B 段 未见新连接受理，见 $SERVE_LOG"; fi

# C. 去重：同一 message_id 连发两次，传输层每次都接受（WS 帧发送成功即退出 0），
#    队列/触发只处理一次 —— 由 Rust 单测断言，此处仅确认重复发送不报错。
"$SCRIPT_DIR/send_onebot_ws_event.sh" -p "$PORT" -g 123456 -m "dup msg" -i 900004 >/dev/null 2>&1
if "$SCRIPT_DIR/send_onebot_ws_event.sh" -p "$PORT" -g 123456 -m "dup msg" -i 900004 >/dev/null 2>&1; then
  ok "C 段 重复 message_id 事件传输层幂等接受（去重由队列层保证，见单测）"
else bad "C 段 重复事件帧发送失败"; fi

# D. 进程退出后端口释放：kill serve，确认端口不再监听且不能再握手。
#    注意：qq_ws_serve 无 SIGTERM signal handler，端口释放依赖进程退出本身
#    （OS 回收 fd），而非优雅 shutdown；显式 stop action 才走 SERVER_SHUTDOWN
#    的 accept-loop break→join→端口释放路径（由 Rust 单测
#    ws_serve_bind_stop_idempotent_lifecycle 覆盖：stop 后 rebind、重复 start
#    幂等、占用端口返 Err 三段）。
kill "$SERVE_PID" 2>/dev/null
SERVE_PID=""
RELEASED=0
for i in $(seq 1 15); do
  if ss -tlnp 2>/dev/null | grep -q ":$PORT "; then sleep 1; continue; fi
  if "$SCRIPT_DIR/send_onebot_ws_event.sh" -p "$PORT" --probe >/dev/null 2>&1; then sleep 1; continue; fi
  RELEASED=1; break
done
if [ "$RELEASED" = "1" ]; then ok "D 段 进程退出后端口 $PORT 已释放"; else bad "D 段 端口 $PORT 未在 15s 内释放"; fi

echo ""
echo "=== serve 日志摘要（WS 处理痕迹）==="
grep -E "\[qq\]|WebSocket|inbox|startup|listening" "$SERVE_LOG" | head -20 || true

echo ""
echo "=== 结果: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" = "0" ]
