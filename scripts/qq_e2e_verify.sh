#!/usr/bin/env bash
# QQ 对话链路端到端验证 —— 不消耗 token 的分段 (任务 D).
#
# 编排：
#   1. 启动 mock OneBot sink（拦 qq_send 出站）
#   2. 用一份指向 sink 的 startup_invoke 启动 cordis-runtime serve --runtime-only
#   3. 分段验证:
#      A. /health          → serve 起来了
#      B. 合法签名事件      → HTTP 200（webhook 接收 + 签名校验通过）
#      C. 错误签名事件      → HTTP 401（签名校验拒绝）
#      D. 无签名事件        → HTTP 200（无 token 兜底；本测试配了 token 故应 401）
#      E. 重复 message_id   → 只处理一次（去重）
#   4. 出站验证放在带 token 的 agent 段（qq_e2e_send_probe.sh），此脚本只覆盖
#      入口到队列的段（不消耗 token）。
#
# 用法: scripts/qq_e2e_verify.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BIN="$PROJECT_DIR/target/debug/cordis-runtime"
PORT="${QQ_SERVE_PORT:-8099}"
SINK_PORT="${SINK_PORT:-5700}"
TOKEN="1145141919810"
SINK_DUMP="/tmp/qq_sink_$$.jsonl"
SERVE_LOG="/tmp/qq_serve_$$.log"
SINK_LOG="/tmp/qq_sink_$$.log"

PASS=0; FAIL=0
ok()   { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

cleanup() {
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null
  [ -n "${SINK_PID:-}" ] && kill "$SINK_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT INT TERM

# 端口占用检查
if ss -tlnp 2>/dev/null | grep -q ":$PORT "; then
  echo "端口 $PORT 已被占用；设 QQ_SERVE_PORT 换端口后重试" >&2; exit 2
fi

echo "=== 1. 启动 mock OneBot sink :$SINK_PORT ==="
python3 "$SCRIPT_DIR/mock_onebot_sink.py" --port "$SINK_PORT" --dump "$SINK_DUMP" >"$SINK_LOG" 2>&1 &
SINK_PID=$!
sleep 1

echo "=== 2. 用临时 startup_invoke 启动 serve :$PORT ==="
# 临时 fixtures：复制真实 fixtures，只覆盖 startup_invoke.json 指向本地 sink + 测试端口。
TMP_FIX="/tmp/qq_fix_$$"
mkdir -p "$TMP_FIX"
# 用 symlink 复用真实 fixtures 的其它内容，仅覆盖 startup_invoke.json。
for f in "$PROJECT_DIR"/fixtures/*; do
  ln -s "$f" "$TMP_FIX/$(basename "$f")" 2>/dev/null
done
rm -f "$TMP_FIX/startup_invoke.json"
cat > "$TMP_FIX/startup_invoke.json" <<JSON
[
  {
    "plugin_path": "qq",
    "node_id": "qq_serve",
    "payload": {
      "node_id": "qq_serve",
      "payload": {
        "port": $PORT,
        "onebot_url": "http://127.0.0.1:$SINK_PORT",
        "access_token": "$TOKEN",
        "allow_groups": []
      }
    }
  }
]
JSON

CORDIS_FIXTURES_ROOT="$TMP_FIX" "$BIN" serve "$TMP_FIX" --runtime-only >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!

# 等 serve 起来（health）
READY=0
for i in $(seq 1 30); do
  if curl -s "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then READY=1; break; fi
  sleep 1
done

echo "=== 3. 分段验证 ==="
# A. serve/health
if [ "$READY" = "1" ]; then ok "A 段 webhook server /health 就绪 (:$PORT)"; else bad "A 段 serve 未就绪，见 $SERVE_LOG"; fi

# B. 合法签名 → 200
if "$SCRIPT_DIR/send_onebot_event.sh" -p "$PORT" -t "$TOKEN" -g 123456 -m "bot hi" -i 900001 >/dev/null 2>&1; then
  ok "B 段 合法 X-Signature 事件被接收 (HTTP 200)"
else bad "B 段 合法签名事件未返回 2xx"; fi

# C. 错误签名 → 401（脚本非零退出即通过）
if "$SCRIPT_DIR/send_onebot_event.sh" -p "$PORT" -t "$TOKEN" -g 123456 -m "bot hi" -i 900002 --bad-sig >/dev/null 2>&1; then
  bad "C 段 错误签名竟被接收（应 401）"
else ok "C 段 错误 X-Signature 被拒 (HTTP 401)"; fi

# D. 无签名（配了 token）→ 401
if "$SCRIPT_DIR/send_onebot_event.sh" -p "$PORT" -t "$TOKEN" -g 123456 -m "bot hi" -i 900003 --no-sig >/dev/null 2>&1; then
  bad "D 段 无签名事件竟被接收（配了 token 应 401）"
else ok "D 段 无签名事件被拒 (HTTP 401)"; fi

# E. 去重：同一 message_id 连发两次，均返回 200（HTTP 层不拒重复），
#    但队列/触发只处理一次 —— 由 Rust 单测 chain_tests 断言，此处仅确认
#    HTTP 幂等接受不报错。
"$SCRIPT_DIR/send_onebot_event.sh" -p "$PORT" -t "$TOKEN" -g 123456 -m "dup msg" -i 900004 >/dev/null 2>&1
if "$SCRIPT_DIR/send_onebot_event.sh" -p "$PORT" -t "$TOKEN" -g 123456 -m "dup msg" -i 900004 >/dev/null 2>&1; then
  ok "E 段 重复 message_id 事件 HTTP 幂等接受（去重由队列层保证，见单测）"
else bad "E 段 重复事件返回非 2xx"; fi

echo ""
echo "=== serve 日志摘要（webhook 处理痕迹）==="
grep -E "\[qq\]|signature|startup|listening" "$SERVE_LOG" | head -20 || true

echo ""
echo "=== 结果: PASS=$PASS FAIL=$FAIL ==="
rm -rf "$TMP_FIX"
[ "$FAIL" = "0" ]
