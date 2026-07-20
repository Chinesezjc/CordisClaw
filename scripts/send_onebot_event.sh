#!/usr/bin/env bash
# 模拟 OneBot v11 群消息事件 POST 到 qq_serve 的 /onebot/event (任务 D).
#
# 用合法的 HMAC-SHA1 X-Signature 签名请求体（P0-23），无需真实 QQ 客户端
# 即可把一条群消息推进 CordisClaw 的 webhook 入口。脚本可重复执行：
# 每次调用递增 message_id，避免被去重逻辑丢弃。
#
# 用法:
#   scripts/send_onebot_event.sh [-p PORT] [-t TOKEN] [-g GROUP_ID] [-m MSG] [-i MSG_ID] [--bad-sig] [--no-sig]
#
# 依赖: curl, openssl (计算 HMAC-SHA1)
set -euo pipefail

PORT=8099
TOKEN="1145141919810"
GROUP_ID=123456
MSG="bot 你好，报一下你的状态"
MSG_ID="$(date +%s)"      # 默认用时间戳当 message_id，保证每次不同
USER_ID=10001
BAD_SIG=0
NO_SIG=0

while [ $# -gt 0 ]; do
  case "$1" in
    -p|--port) PORT="$2"; shift 2;;
    -t|--token) TOKEN="$2"; shift 2;;
    -g|--group) GROUP_ID="$2"; shift 2;;
    -m|--message) MSG="$2"; shift 2;;
    -i|--msg-id) MSG_ID="$2"; shift 2;;
    -u|--user) USER_ID="$2"; shift 2;;
    --bad-sig) BAD_SIG=1; shift;;
    --no-sig) NO_SIG=1; shift;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done

# OneBot v11 群消息事件（结构化 message segments）。
# 用 printf 生成紧凑单行 JSON —— 签名必须针对"实际发送的字节"计算，
# 所以这里的 BODY 变量就是发送体本身。
BODY=$(printf '{"post_type":"message","message_type":"group","sub_type":"normal","message_id":%s,"group_id":%s,"user_id":%s,"raw_message":"%s","message":[{"type":"text","data":{"text":"%s"}}],"sender":{"user_id":%s,"nickname":"tester"}}' \
  "$MSG_ID" "$GROUP_ID" "$USER_ID" "$MSG" "$MSG" "$USER_ID")

# HMAC-SHA1(body, key=access_token) → hex，OneBot 头格式为 "sha1=<hex>"。
SIG_HEX=$(printf '%s' "$BODY" | openssl dgst -sha1 -hmac "$TOKEN" | sed 's/^.*= //')
if [ "$BAD_SIG" = "1" ]; then
  SIG_HEX="deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
fi

URL="http://127.0.0.1:${PORT}/onebot/event"
echo "[event] POST $URL group=$GROUP_ID msg_id=$MSG_ID sig=sha1=${SIG_HEX:0:12}... bad_sig=$BAD_SIG no_sig=$NO_SIG"

if [ "$NO_SIG" = "1" ]; then
  HTTP_CODE=$(curl -s -o /tmp/onebot_resp.txt -w '%{http_code}' \
    -H "Content-Type: application/json" \
    --data-binary "$BODY" "$URL")
else
  HTTP_CODE=$(curl -s -o /tmp/onebot_resp.txt -w '%{http_code}' \
    -H "Content-Type: application/json" \
    -H "X-Signature: sha1=${SIG_HEX}" \
    --data-binary "$BODY" "$URL")
fi

echo "[event] HTTP $HTTP_CODE body=$(cat /tmp/onebot_resp.txt)"
# 退出码：2xx → 0，否则非零，方便脚本断言。
case "$HTTP_CODE" in
  2*) exit 0;;
  *) exit 1;;
esac
