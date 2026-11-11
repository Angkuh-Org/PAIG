#!/bin/sh
# Small, repeatable benchmark: binary size, idle RSS, edge latency.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != dumb ]; then
    RESET=$(printf '\033[0m')
    GREEN=$(printf '\033[1;32m')
    CYAN=$(printf '\033[1;36m')
    BLUE=$(printf '\033[1;34m')
    MAGENTA=$(printf '\033[1;35m')
    I_BINARY=$(printf '\357\206\262')
    I_START=$(printf '\357\203\247')
    I_RSS=$(printf '\357\210\263')
    I_HEALTH=$(printf '\357\200\204')
    I_CHAT=$(printf '\357\203\202')
else
    RESET= GREEN= CYAN= BLUE= MAGENTA=
    I_BINARY='[B]'
    I_START='[>]'
    I_RSS='[M]'
    I_HEALTH='[H]'
    I_CHAT='[C]'
fi
PORT=${PAIG_BENCH_PORT:-18899}
MOCK_PORT=${PAIG_BENCH_MOCK_PORT:-19090}
TMP=$(mktemp -d /tmp/paig-bench.XXXXXX)
PAIG_PID=
MOCK_PID=
cleanup() {
    [ -n "$PAIG_PID" ] && kill "$PAIG_PID" 2>/dev/null || true
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
    [ -n "$PAIG_PID" ] && wait "$PAIG_PID" 2>/dev/null || true
    [ -n "$MOCK_PID" ] && wait "$MOCK_PID" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

wait_ready() {
    url=$1
    log=$2
    n=0
    while ! curl -fsS --max-time 0.5 "$url" >/dev/null 2>&1; do
        n=$((n + 1))
        if [ "$n" -ge 500 ]; then
            echo "FAIL: readiness timeout for $url" >&2
            cat "$log" >&2 || true
            exit 1
        fi
        sleep 0.01
    done
}

cat > "$TMP/config.json" <<EOF
{
  "server": { "host": "127.0.0.1", "port": $PORT },
  "debug": false,
  "ledgerPath": "$TMP/ledger.jsonl",
  "commandCode": {
    "baseUrl": "http://127.0.0.1:$MOCK_PORT",
    "apiKey": "dummy",
    "models": ["test-model"]
  }
}
EOF
: > "$TMP/empty.env"

"$ROOT/build.sh" >/dev/null
python3 "$ROOT/tests/mock_upstream.py" "$MOCK_PORT" >"$TMP/mock.log" 2>&1 &
MOCK_PID=$!
wait_ready "http://127.0.0.1:$MOCK_PORT/provider/v1/models" "$TMP/mock.log"
START=$(date +%s%N)
PAIG_CONFIG="$TMP/config.json" PAIG_ENV_FILE="$TMP/empty.env" PAIG_PORT="$PORT" CC_API_KEY=dummy \
    "$ROOT/build/paig" >"$TMP/paig.log" 2>&1 &
PAIG_PID=$!
wait_ready "http://127.0.0.1:$PORT/health" "$TMP/paig.log"
READY=$(date +%s%N)
START_MS=$(( (READY - START) / 1000000 ))

RSS_KB=$(awk '/VmRSS/ {print $2}' "/proc/$PAIG_PID/status")
SIZE=$(stat -c%s "$ROOT/build/paig")

REQ=500
URLS=""
i=0
while [ "$i" -lt "$REQ" ]; do
    URLS="$URLS http://127.0.0.1:$PORT/health"
    i=$((i + 1))
done
T0=$(date +%s%N)
curl -sS $URLS >/dev/null
T1=$(date +%s%N)
HEALTH_MS=$(( (T1 - T0) / 1000000 ))
HEALTH_RPS=$(awk -v n="$REQ" -v ms="$HEALTH_MS" 'BEGIN { if (ms <= 0) ms = 1; printf "%.1f", n * 1000 / ms }')

CHAT_REQ=30
CHAT_MS=$(python3 - "$PORT" "$CHAT_REQ" <<'PY'
import http.client, json, sys, time
port = int(sys.argv[1])
n = int(sys.argv[2])
body = json.dumps({"model": "cc/test-model", "messages": [{"role": "user", "content": "hi"}]})
conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
start = time.perf_counter()
for _ in range(n):
    conn.request("POST", "/v1/chat/completions", body, {"content-type": "application/json"})
    response = conn.getresponse()
    response.read()
    if response.status != 200:
        raise SystemExit(f"status {response.status}")
print(int((time.perf_counter() - start) * 1000))
PY
)
CHAT_AVG=$(awk -v n="$CHAT_REQ" -v ms="$CHAT_MS" 'BEGIN { if (n <= 0) n = 1; printf "%.2f", ms / n }')

printf '  %s%s%s  %-13s: %s%s%s bytes\n' "$GREEN" "$I_BINARY" "$RESET" "binary" "$GREEN" "$SIZE" "$RESET"
printf '  %s%s%s  %-13s: %s%s%s ms (process -> /health 200)\n' "$CYAN" "$I_START" "$RESET" "startup" "$GREEN" "$START_MS" "$RESET"
printf '  %s%s%s  %-13s: %s%s%s KiB\n' "$BLUE" "$I_RSS" "$RESET" "idle RSS" "$GREEN" "$RSS_KB" "$RESET"
printf '  %s%s%s  %-13s: %s%s%s req/s (%s requests in %s ms, one curl, keep-alive)\n' "$GREEN" "$I_HEALTH" "$RESET" "health" "$GREEN" "$HEALTH_RPS" "$RESET" "$REQ" "$HEALTH_MS"
printf '  %s%s%s  %-13s: %s%s%s ms/req (%s requests in %s ms, mock upstream, keep-alive)\n' "$MAGENTA" "$I_CHAT" "$RESET" "cc non-stream" "$GREEN" "$CHAT_AVG" "$RESET" "$CHAT_REQ" "$CHAT_MS"
