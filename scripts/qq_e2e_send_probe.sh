#!/usr/bin/env bash
# QQ 链路"出口段"验证 —— 消耗 DeepSeek token，只跑一次 (任务 D).
#
# 完整链路: webhook → 去重/队列 → start_agent_poller → agent_trigger →
#   runtime inbox session 路由 → agent_send(DeepSeek) → agent 决定动作 →
#   host.invoke(qq, qq_send) → OneBot send_group_msg → mock sink 落盘。
#
# 关键: 用**真实 fixtures 根**运行 serve，这样 discover_config_dir 能找到
# 同级的 config/llm_api.yaml（含 DeepSeek key）。真实 fixtures/startup_invoke.json
# 的 onebot_url 已指向 127.0.0.1:5700，正好是本脚本的 mock sink —— 因此出站
# 的 send_group_msg 会被 sink 拦截落盘，无需真实 QQ 服务端。
#
# 通过检查 sink dump 是否出现 send_group_msg 记录，断言回复动作到达 HTTP 出口。
# 因消耗 token，需显式确认。
#
# 用法: CONFIRM_TOKEN_SPEND=1 scripts/qq_e2e_send_probe.sh
set -uo pipefail

if [ "${CONFIRM_TOKEN_SPEND:-0}" != "1" ]; then
  echo "此脚本会调用 DeepSeek（消耗 token）。确认后用: CONFIRM_TOKEN_SPEND=1 $0" >&2
  exit 3
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BIN="$PROJECT_DIR/target/debug/cordis-runtime"
FIXTURES="$PROJECT_DIR/fixtures"
PORT="${QQ_SERVE_PORT:-8099}"     # 需与 fixtures/startup_invoke.json 的 port 一致
SINK_PORT="${SINK_PORT:-5700}"    # 需与 startup_invoke.json 的 onebot_url 端口一致
TOKEN="1145141919810"             # 需与 startup_invoke.json 的 access_token 一致
GROUP=123456
SINK_DUMP="/tmp/qq_sink_probe_$$.jsonl"
SERVE_LOG="/tmp/qq_serve_probe_$$.log"

cleanup() {
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null
  [ -n "${SINK_PID:-}" ] && kill "$SINK_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT INT TERM

if ss -tlnp 2>/dev/null | grep -q ":$PORT "; then
  echo "端口 $PORT 被占用；设 QQ_SERVE_PORT 换端口（并同步改 startup_invoke.json）" >&2; exit 2
fi

echo "=== 启动 mock sink :$SINK_PORT (拦截 qq_send 出站) ==="
python3 "$SCRIPT_DIR/mock_onebot_sink.py" --port "$SINK_PORT" --dump "$SINK_DUMP" >/tmp/qq_sink_probe_$$.log 2>&1 &
SINK_PID=$!
sleep 1

echo "=== 启动 serve --runtime-only（真实 fixtures，含 LLM 配置）:$PORT ==="
"$BIN" serve "$FIXTURES" --runtime-only >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!
for i in $(seq 1 30); do curl -s "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break; sleep 1; done

echo "=== 发一条明确 @bot 的群消息，触发 agent 回复 ==="
"$SCRIPT_DIR/send_onebot_event.sh" -p "$PORT" -t "$TOKEN" -g "$GROUP" \
  -m "bot 请只回复两个字：收到" -i "$(date +%s)" || true

echo "=== 等待 poller(2s) + agent + qq_send 出站（最多 180s）==="
HIT=0
for i in $(seq 1 90); do
  if grep -q "send_group_msg\|send_private_msg" "$SINK_DUMP" 2>/dev/null; then HIT=1; break; fi
  sleep 2
done

echo ""
if [ "$HIT" = "1" ]; then
  echo "PASS: qq_send 出站到达 mock sink —— 完整链路打通"
  echo "--- sink 收到的出站请求 ---"
  cat "$SINK_DUMP"
else
  echo "FAIL: 180s 内未见出站；检查 LLM key / agent 是否 suspend"
  echo "--- serve 日志尾部 ---"
  tail -25 "$SERVE_LOG"
fi

echo ""
echo "--- serve inbox 痕迹 ---"
grep -E "inbox|\[qq\]" "$SERVE_LOG" | grep -v "registered-net" | head -30 || true
[ "$HIT" = "1" ]
