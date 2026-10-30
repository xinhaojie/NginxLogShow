#!/usr/bin/env bash
# bench.sh —— HTTP 日志服务器性能压测脚本
# 循环并发 POST /api/logs/{system}，统计成功数 / 失败数 / 耗时 / QPS
#
# 用法:
#   ./bench.sh [总请求数] [并发数] [间隔秒]
#   例:
#   ./bench.sh 1000 10          # 1000 请求, 10 并发
#   ./bench.sh 5000 50 0.01     # 5000 请求, 50 并发, 每次间隔 10ms
#
# 环境变量可覆盖:
#   URL   目标地址        默认 http://localhost:80/api/logs/mqtt
#   TOKEN 认证 token      默认使用示例 curl 中的 Bearer token

set -u

URL="${URL:-https://logs.futuremorph.cn/api/logs}"
TOKEN="${TOKEN:-ZnV0dXJlX21vcnBoQGdtYWlsLmNvbTpvMm9pX2xiT3Z0S3prY1htR1R5SEJyQTE0QWdBbmxRNnNSdGs5}"
TOTAL="${1:-1000}"
CONCURRENCY="${2:-10}"
INTERVAL="${3:-0}"

echo ">> 压测目标 : $URL"
echo ">> 总请求数 : $TOTAL   并发数: $CONCURRENCY   间隔: ${INTERVAL}s"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 把总请求数尽量均匀分给每个并发 worker
BASE=$((TOTAL / CONCURRENCY))
REM=$((TOTAL % CONCURRENCY))

START=$(date +%s%3N)

for w in $(seq 1 "$CONCURRENCY"); do
  (
    count=$BASE
    [ "$w" -le "$REM" ] && count=$((count + 1))
    for i in $(seq 1 "$count"); do
      code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
        -X POST "$URL" \
        -H "Authorization: Bearer $TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"level":"debug","message":"bench test"}')
      echo "${code:-000}" >> "$TMP/$w.codes"
      if [ "$INTERVAL" != "0" ]; then
        sleep "$INTERVAL"
      fi
    done
  ) &
done
wait

END=$(date +%s%3N)
ELAPSED_MS=$((END - START))

cat "$TMP"/*.codes > "$TMP/all"
sent=$(wc -l < "$TMP/all" | tr -d ' ')
ok=$(grep -c '^2' "$TMP/all" || true)
fail=$((sent - ok))
qps=$(awk "BEGIN {printf \"%.1f\", $sent / ($ELAPSED_MS / 1000)}")
avg_ms=$(awk "BEGIN {printf \"%.2f\", $ELAPSED_MS / $sent}")

echo ">> 发送     : $sent"
echo ">> 成功(2xx): $ok"
echo ">> 失败     : $fail"
echo ">> 总耗时   : ${ELAPSED_MS}ms"
echo ">> 平均耗时 : ${avg_ms}ms/请求"
echo ">> QPS      : $qps"
