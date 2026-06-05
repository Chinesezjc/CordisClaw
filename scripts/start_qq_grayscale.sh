#!/bin/bash
# CordisClaw QQ 灰度测试 — 一键启动
#
# 启动顺序:
#   1. CordisClaw runtime (qq_serve HTTP server)
#   2. LunaBot (OneBot 客户端)
#
# 前置条件:
#   - OneBot 客户端 (NapCat/LLOneBot) 已运行
#   - LLM API key 已在 fixtures/config.yaml 中配置
#   - LunaBot 已安装依赖 (pip install -r requirements.txt)
#
# 使用方法:
#   ./scripts/start_qq_grayscale.sh [--with-llm]

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
Lunabot_DIR="$PROJECT_DIR/../PJSKBot/LunaBot"

CORDISCLAW_BIN="$PROJECT_DIR/target/debug/cordis-runtime"
FIXTURES_ROOT="$PROJECT_DIR/fixtures"
QQ_SERVE_PORT="${QQ_SERVE_PORT:-8080}"
ONEBOT_URL="${ONEBOT_URL:-http://127.0.0.1:5700}"
ALLOW_GROUPS="${ALLOW_GROUPS:-}"

USE_LLM=""
if [ "$1" = "--with-llm" ]; then
    USE_LLM="--enable-llm"
fi

echo "========================================"
echo " CordisClaw QQ 灰度测试"
echo "========================================"
echo " OneBot URL:  $ONEBOT_URL"
echo " QQ Serve:    :$QQ_SERVE_PORT"
echo " Allow:       ${ALLOW_GROUPS:-all groups}"
echo " LLM:         ${USE_LLM:-disabled (pass-through)}"
echo "========================================"

# ── 1. Build CordisClaw (if needed) ────────────────────────────────────────
if [ ! -x "$CORDISCLAW_BIN" ]; then
    echo "[build] Compiling CordisClaw..."
    cd "$PROJECT_DIR" && cargo build
fi

# ── 2. Start CordisClaw QQ Agent ───────────────────────────────────────────
echo "[cordisclaw] Starting QQ agent..."
cd "$PROJECT_DIR"

python3 scripts/qq_agent.py \
    --cordisclaw-bin "$CORDISCLAW_BIN" \
    --fixtures-root "$FIXTURES_ROOT" \
    --port "$QQ_SERVE_PORT" \
    --onebot-url "$ONEBOT_URL" \
    --allow-groups "$ALLOW_GROUPS" \
    $USE_LLM &

CORDISCLAW_PID=$!
echo "[cordisclaw] PID=$CORDISCLAW_PID"

# Wait for CordisClaw to be ready (health check).
echo "[cordisclaw] Waiting for qq_serve..."
for i in $(seq 1 30); do
    if curl -s "http://127.0.0.1:$QQ_SERVE_PORT/health" > /dev/null 2>&1; then
        echo "[cordisclaw] qq_serve is ready on :$QQ_SERVE_PORT"
        break
    fi
    sleep 1
done

# ── 3. Start LunaBot ───────────────────────────────────────────────────────
echo "[lunabot] Starting..."
cd "$Lunabot_DIR"

if [ -f "venv/bin/activate" ]; then
    source venv/bin/activate
fi

# Start LunaBot with nb run.
python3 -m nb_cli run &
LUNABOT_PID=$!
echo "[lunabot] PID=$LUNABOT_PID"

# ── 4. Wait ────────────────────────────────────────────────────────────────
cleanup() {
    echo ""
    echo "[shutdown] Stopping..."
    kill $LUNABOT_PID 2>/dev/null || true
    kill $CORDISCLAW_PID 2>/dev/null || true
    wait
    echo "[shutdown] Done."
}
trap cleanup INT TERM

echo ""
echo "========================================"
echo " Both systems running."
echo " CordisClaw PID: $CORDISCLAW_PID"
echo " LunaBot PID:    $LUNABOT_PID"
echo " Press Ctrl+C to stop."
echo "========================================"

wait
