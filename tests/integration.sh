#!/bin/sh
# End-to-end smoke test: real rustc binary + mock upstream.
# TODO(paig): optional credentialed smoke test against real upstreams; keep the
# default suite deterministic and mock-only.
# TODO(paig): cover conflicting Content-Length, IPv6 Host on the wire, POST
# no-transport-retry, pooled timeout reset, auth-export timeout/overflow,
# OpenCode 3xx mapping, and stream_error_status.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PORT=${PAIG_TEST_PORT:-18899}
MOCK_PORT=${PAIG_MOCK_PORT:-19090}
TLS_PORT=${PAIG_TLS_PORT:-19443}
TLS_PAIG_PORT=${PAIG_TLS_PAIG_PORT:-18901}
TLS_EXPIRED_PORT=${PAIG_TLS_EXPIRED_PORT:-19444}
TLS_EXPIRED_PAIG_PORT=${PAIG_TLS_EXPIRED_PAIG_PORT:-18904}
TMP=$(mktemp -d /tmp/paig-it.XXXXXX)
PAIG_PID=
MOCK_PID=
TLS_PAIG_PID=
TLS_PID=
TLS_EXPIRED_PAIG_PID=
TLS_EXPIRED_PID=
FOREIGN_PID=
DAEMON_HOME=
cleanup() {
    if [ -n "$DAEMON_HOME" ] && [ -f "$DAEMON_HOME/.paig/paig.pid" ]; then
        kill "$(cat "$DAEMON_HOME/.paig/paig.pid" 2>/dev/null)" 2>/dev/null || true
    fi
    [ -n "$FOREIGN_PID" ] && kill "$FOREIGN_PID" 2>/dev/null || true
    [ -n "$TLS_EXPIRED_PAIG_PID" ] && kill "$TLS_EXPIRED_PAIG_PID" 2>/dev/null || true
    [ -n "$TLS_EXPIRED_PID" ] && kill "$TLS_EXPIRED_PID" 2>/dev/null || true
    [ -n "$TLS_PAIG_PID" ] && kill "$TLS_PAIG_PID" 2>/dev/null || true
    [ -n "$TLS_PID" ] && kill "$TLS_PID" 2>/dev/null || true
    [ -n "$PAIG_PID" ] && kill "$PAIG_PID" 2>/dev/null || true
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
    [ -n "$TLS_EXPIRED_PAIG_PID" ] && wait "$TLS_EXPIRED_PAIG_PID" 2>/dev/null || true
    [ -n "$TLS_EXPIRED_PID" ] && wait "$TLS_EXPIRED_PID" 2>/dev/null || true
    [ -n "$TLS_PAIG_PID" ] && wait "$TLS_PAIG_PID" 2>/dev/null || true
    [ -n "$TLS_PID" ] && wait "$TLS_PID" 2>/dev/null || true
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
  },
  "opencode": { "freeBaseUrl": "http://127.0.0.1:$MOCK_PORT/v1" },
  "kilo": { "baseUrl": "http://127.0.0.1:$MOCK_PORT/v1" },
  "bob": {
    "baseUrl": "http://127.0.0.1:$MOCK_PORT/inference/v1",
    "apiKey": "dummy",
    "teamId": "team1"
  },
  "mistral": {
    "baseUrl": "http://127.0.0.1:$MOCK_PORT/v1",
    "apiKey": "dummy",
    "models": ["mistral-model"]
  }
}
EOF
: > "$TMP/empty.env"

"$ROOT/build.sh" >/dev/null
python3 "$ROOT/tests/mock_upstream.py" "$MOCK_PORT" >"$TMP/mock.log" 2>&1 &
MOCK_PID=$!
wait_ready "http://127.0.0.1:$MOCK_PORT/provider/v1/models" "$TMP/mock.log"
PAIG_CONFIG="$TMP/config.json" PAIG_ENV_FILE="$TMP/empty.env" PAIG_PORT="$PORT" CC_API_KEY=dummy \
    "$ROOT/build/paig" >"$TMP/paig.log" 2>&1 &
PAIG_PID=$!
wait_ready "http://127.0.0.1:$PORT/health" "$TMP/paig.log"

fail() {
    echo "FAIL: $*" >&2
    echo "--- paig.log ---" >&2
    cat "$TMP/paig.log" >&2 || true
    exit 1
}

health=$(curl -fsS --max-time 5 "http://127.0.0.1:$PORT/health")
echo "$health" | grep -q '"ok":true' || fail "health"

models=$(curl -fsS --max-time 10 "http://127.0.0.1:$PORT/v1/models")
echo "$models" | grep -q '"cc/test-model"' || fail "models missing CC model"
if echo "$models" | grep -q '"id":"test-model"'; then fail "bare CC model still listed"; fi
echo "$models" | grep -q '"opencode/beam-free"' || fail "models missing OpenCode model"
echo "$models" | grep -q '"bob/bob-model"' || fail "models missing Bob model"
echo "$models" | grep -q '"mistral/mistral-model"' || fail "models missing Mistral model"

stream=$(curl -fsS -N --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"cc/test-model","messages":[{"role":"user","content":"hi"}],"stream":true}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$stream" | grep -q 'hello' || fail "CC stream text"
echo "$stream" | grep -q '"model":"cc/test-model"' || fail "CC stream model rewrite"
echo "$stream" | grep -q '\[DONE\]' || fail "CC stream terminator"

nonstream=$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"cc/test-model","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$nonstream" | grep -q '"content":"hello world"' || fail "CC non-stream text"
echo "$nonstream" | grep -q '"model":"cc/test-model"' || fail "CC model rewrite"

cc_prefix_stream=$(curl -fsS -N --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"commandcode/test-model","messages":[{"role":"user","content":"hi"}],"stream":true}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$cc_prefix_stream" | grep -q '"model":"commandcode/test-model"' \
    || fail "CC prefixed stream model rewrite"

oc_stream=$(curl -fsS -N --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"opencode/beam-free","messages":[{"role":"user","content":"hi"}],"stream":true}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$oc_stream" | grep -q '"content":"open"' || fail "OpenCode stream text"
echo "$oc_stream" | grep -q '\[DONE\]' || fail "OpenCode stream terminator"

empty_status=$(curl -sS -o "$TMP/empty.json" -w '%{http_code}' --max-time 5 \
    -H 'content-type: application/json' \
    -d '{"model":"cc/empty-model","messages":[{"role":"user","content":"hi"}],"stream":true}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
[ "$empty_status" = "502" ] || fail "empty upstream status $empty_status"
grep -q 'no body' "$TMP/empty.json" || fail "empty upstream error"

notmod_status=$(curl -sS -o "$TMP/notmod.json" -w '%{http_code}' --max-time 5 \
    -H 'content-type: application/json' \
    -d '{"model":"cc/not-modified-model","messages":[{"role":"user","content":"hi"}],"stream":true}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
[ "$notmod_status" = "502" ] || fail "304 upstream status $notmod_status"
grep -q 'no body' "$TMP/notmod.json" || fail "304 upstream error"

nodone=$(curl -fsS -N --max-time 5 -H 'content-type: application/json' \
    -d '{"model":"cc/no-done-model","messages":[{"role":"user","content":"hi"}],"stream":true}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$nodone" | grep -q 'hello' || fail "no-DONE stream text"
echo "$nodone" | grep -q '\[DONE\]' || fail "no-DONE stream terminator"

oc_json=$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"opencode/beam-free","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$oc_json" | grep -q '"content":"open"' || fail "OpenCode non-stream text"
echo "$oc_json" | grep -q '"model":"opencode/beam-free"' || fail "OpenCode model rewrite"
grep -q '"model":"opencode/beam-free"' "$TMP/ledger.jsonl" || fail "OpenCode ledger entry"

kilo_json=$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"kilo/some/model:free","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$kilo_json" | grep -q '"content":"open"' || fail "Kilo non-stream text"
echo "$kilo_json" | grep -q '"model":"kilo/some/model:free"' || fail "Kilo model rewrite"

bob_json=$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"bob/bob-model","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$bob_json" | grep -q '"content":"open"' || fail "Bob non-stream text"
echo "$bob_json" | grep -q '"model":"bob/bob-model"' || fail "Bob model rewrite"

mistral_json=$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"mistral/mistral-model","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$mistral_json" | grep -q '"content":"open"' || fail "Mistral non-stream text"
echo "$mistral_json" | grep -q '"model":"mistral/mistral-model"' || fail "Mistral model rewrite"

mistral_stream=$(curl -fsS -N --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"mistral/mistral-model","messages":[{"role":"user","content":"hi"}],"stream":true}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
echo "$mistral_stream" | grep -q '"content":"open"' || fail "Mistral stream text"
echo "$mistral_stream" | grep -q '\[DONE\]' || fail "Mistral stream terminator"

unrouted_status=$(curl -sS -o "$TMP/unrouted.json" -w '%{http_code}' --max-time 5 \
    -H 'content-type: application/json' \
    -d '{"model":"test-model","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
[ "$unrouted_status" = "400" ] || fail "unrouted model status $unrouted_status"
grep -q 'model_unrouted' "$TMP/unrouted.json" || fail "unrouted model code"

excluded_status=$(curl -sS -o "$TMP/excluded.json" -w '%{http_code}' --max-time 5 \
    -H 'content-type: application/json' \
    -d '{"model":"cc/claude-opus-5","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$PORT/v1/chat/completions")
[ "$excluded_status" = "404" ] || fail "excluded model status $excluded_status"
grep -q 'model_excluded' "$TMP/excluded.json" || fail "excluded model code"

unknown_status=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$PORT/nope")
[ "$unknown_status" = "404" ] || fail "unknown route status $unknown_status"

# TLS diagnosis: a self-signed cert must surface the handshake failure and the
# readable verify code, not a masked connect error from another address.
command -v openssl >/dev/null 2>&1 || fail "openssl CLI is required for the TLS test"
openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$TMP/tls-key.pem" -out "$TMP/tls-cert.pem" \
    -days 1 -subj '/CN=localhost' -addext 'subjectAltName=DNS:localhost' \
    >"$TMP/tls-cert.log" 2>&1 || fail "cannot generate TLS certificate"
openssl s_server -quiet -accept "127.0.0.1:$TLS_PORT" \
    -cert "$TMP/tls-cert.pem" -key "$TMP/tls-key.pem" \
    >"$TMP/tls-server.log" 2>&1 &
TLS_PID=$!
sleep 0.2

cat > "$TMP/tls-config.json" <<EOF
{
  "server": { "host": "127.0.0.1", "port": $TLS_PAIG_PORT },
  "ledgerPath": "$TMP/tls-ledger.jsonl",
  "opencode": { "freeBaseUrl": "https://localhost:$TLS_PORT/v1" }
}
EOF

PAIG_CONFIG="$TMP/tls-config.json" PAIG_ENV_FILE="$TMP/empty.env" PAIG_PORT="$TLS_PAIG_PORT" \
    "$ROOT/build/paig" >"$TMP/tls-paig.log" 2>&1 &
TLS_PAIG_PID=$!
wait_ready "http://127.0.0.1:$TLS_PAIG_PORT/health" "$TMP/tls-paig.log"

tls_error=$(curl -sS --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"opencode/beam-free","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$TLS_PAIG_PORT/v1/chat/completions")
echo "$tls_error" | grep -q 'TLS handshake failed' || fail "TLS handshake error missing: $tls_error"
echo "$tls_error" | grep -q 'self-signed or untrusted certificate' || fail "TLS verify text missing: $tls_error"
echo "$tls_error" | grep -q 'verify=0x8' || fail "TLS verify flag missing: $tls_error"
if echo "$tls_error" | grep -q 'Cannot assign requested address'; then
    fail "TLS error masked by connect error: $tls_error"
fi

kill "$TLS_PAIG_PID" "$TLS_PID" 2>/dev/null || true
wait "$TLS_PAIG_PID" "$TLS_PID" 2>/dev/null || true
TLS_PAIG_PID=
TLS_PID=

# Expired certificate: date validation must fail closed too. `openssl req`
# only gained -not_before/-not_after after 3.0, so sign through `openssl ca`,
# whose -startdate/-enddate work across versions.
mkdir -p "$TMP/ca/newcerts"
: > "$TMP/ca/index.txt"
echo 01 > "$TMP/ca/serial"
cat > "$TMP/ca/openssl.cnf" <<EOF
[ ca ]
default_ca = CA_default
[ CA_default ]
dir = $TMP/ca
database = \$dir/index.txt
new_certs_dir = \$dir/newcerts
serial = \$dir/serial
default_md = sha256
policy = policy_any
x509_extensions = v3_ca
[ policy_any ]
commonName = supplied
[ v3_ca ]
subjectAltName = DNS:localhost
basicConstraints = critical,CA:TRUE
EOF
openssl req -new -newkey rsa:2048 -nodes \
    -keyout "$TMP/tls-expired-key.pem" -out "$TMP/tls-expired.csr" \
    -subj '/CN=localhost' \
    >"$TMP/tls-expired-csr.log" 2>&1 || fail "cannot generate expired TLS key"
openssl ca -batch -selfsign -config "$TMP/ca/openssl.cnf" \
    -keyfile "$TMP/tls-expired-key.pem" -in "$TMP/tls-expired.csr" \
    -out "$TMP/tls-expired-cert.pem" \
    -startdate 20200101000000Z -enddate 20200102000000Z -extensions v3_ca \
    >"$TMP/tls-expired-cert.log" 2>&1 || fail "cannot generate expired TLS certificate"
openssl s_server -quiet -accept "127.0.0.1:$TLS_EXPIRED_PORT" \
    -cert "$TMP/tls-expired-cert.pem" -key "$TMP/tls-expired-key.pem" \
    >"$TMP/tls-expired-server.log" 2>&1 &
TLS_EXPIRED_PID=$!
sleep 0.2

cat > "$TMP/tls-expired-config.json" <<EOF
{
  "server": { "host": "127.0.0.1", "port": $TLS_EXPIRED_PAIG_PORT },
  "ledgerPath": "$TMP/tls-expired-ledger.jsonl",
  "opencode": { "freeBaseUrl": "https://localhost:$TLS_EXPIRED_PORT/v1" }
}
EOF

PAIG_CONFIG="$TMP/tls-expired-config.json" PAIG_ENV_FILE="$TMP/empty.env" PAIG_PORT="$TLS_EXPIRED_PAIG_PORT" \
    "$ROOT/build/paig" >"$TMP/tls-expired-paig.log" 2>&1 &
TLS_EXPIRED_PAIG_PID=$!
wait_ready "http://127.0.0.1:$TLS_EXPIRED_PAIG_PORT/health" "$TMP/tls-expired-paig.log"

expired_error=$(curl -sS --max-time 10 -H 'content-type: application/json' \
    -d '{"model":"opencode/beam-free","messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$TLS_EXPIRED_PAIG_PORT/v1/chat/completions")
echo "$expired_error" | grep -q 'TLS handshake failed' || fail "expired TLS error missing: $expired_error"
echo "$expired_error" | grep -q 'certificate has expired' || fail "expired TLS text missing: $expired_error"
echo "$expired_error" | grep -q 'verify=0x9' || fail "expired TLS verify flag missing: $expired_error"

kill "$TLS_EXPIRED_PAIG_PID" "$TLS_EXPIRED_PID" 2>/dev/null || true
wait "$TLS_EXPIRED_PAIG_PID" "$TLS_EXPIRED_PID" 2>/dev/null || true
TLS_EXPIRED_PAIG_PID=
TLS_EXPIRED_PID=

# Background lifecycle: the binary owns ~/.paig/pid/log, detachment, and stop.
DAEMON_PORT=${PAIG_DAEMON_PORT:-18902}
DAEMON_HOME="$TMP/daemon-home"
mkdir -p "$TMP/bg" "$DAEMON_HOME"
cp "$ROOT/build/paig" "$TMP/bg/paig"
HOME="$DAEMON_HOME" PAIG_CONFIG="$TMP/config.json" PAIG_ENV_FILE="$TMP/empty.env" PAIG_PORT="$DAEMON_PORT" CC_API_KEY=dummy \
    "$TMP/bg/paig" --start >"$TMP/bg-start.txt" || fail "daemon --start"
grep -q 'paig started' "$TMP/bg-start.txt" || fail "daemon start output"
HOME="$DAEMON_HOME" "$TMP/bg/paig" --status | grep -q 'paig running' || fail "daemon --status"
wait_ready "http://127.0.0.1:$DAEMON_PORT/health" "$DAEMON_HOME/.paig/paig.log"

HOME="$DAEMON_HOME" PAIG_CONFIG="$TMP/config.json" PAIG_ENV_FILE="$TMP/empty.env" PAIG_PORT="$DAEMON_PORT" CC_API_KEY=dummy \
    "$TMP/bg/paig" --restart >"$TMP/bg-restart.txt" || fail "daemon --restart"
grep -q 'paig stopped' "$TMP/bg-restart.txt" || fail "daemon restart stop output"
grep -q 'paig started' "$TMP/bg-restart.txt" || fail "daemon restart start output"
wait_ready "http://127.0.0.1:$DAEMON_PORT/health" "$DAEMON_HOME/.paig/paig.log"

HOME="$DAEMON_HOME" "$TMP/bg/paig" --stop >/dev/null || fail "daemon --stop"
if HOME="$DAEMON_HOME" "$TMP/bg/paig" --status >/dev/null 2>&1; then
    fail "daemon still running after --stop"
fi
[ ! -f "$DAEMON_HOME/.paig/paig.pid" ] || fail "daemon pidfile not removed"

# A stale pidfile must never signal a foreign process.
sleep 60 &
FOREIGN_PID=$!
printf '%s\n' "$FOREIGN_PID" > "$DAEMON_HOME/.paig/paig.pid"
if HOME="$DAEMON_HOME" "$TMP/bg/paig" --status >/dev/null 2>&1; then
    fail "foreign pid reported running"
fi
printf '%s\n' "$FOREIGN_PID" > "$DAEMON_HOME/.paig/paig.pid"
HOME="$DAEMON_HOME" "$TMP/bg/paig" --stop >"$TMP/bg-foreign.txt" || fail "daemon --stop with foreign pid"
grep -q 'paig not running' "$TMP/bg-foreign.txt" || fail "foreign pid stop output"
kill -0 "$FOREIGN_PID" 2>/dev/null || fail "foreign process was killed"
kill "$FOREIGN_PID" 2>/dev/null || true
wait "$FOREIGN_PID" 2>/dev/null || true
FOREIGN_PID=

# One --init accepts a --from-auth per backend.
printf 'cc-key\n' > "$TMP/cc.key"
printf 'oc-key\n' > "$TMP/oc.key"
printf 'kilo-key\n' > "$TMP/kilo.key"
printf 'bob-key\n' > "$TMP/bob.key"
printf 'mistral-key\n' > "$TMP/mistral.key"
PAIG_CONFIG="$TMP/multi.json" "$ROOT/build/paig" --init \
    --from-auth cc:"$TMP/cc.key" \
    --from-auth opencode:"$TMP/oc.key" \
    --from-auth kilo:"$TMP/kilo.key" \
    --from-auth bob:"$TMP/bob.key" \
    --from-auth mistral:"$TMP/mistral.key" >"$TMP/multi-init.txt" 2>&1 \
    || fail "multi-backend --init"
for key in cc-key oc-key kilo-key bob-key mistral-key; do
    grep -q "$key" "$TMP/multi.json" || fail "missing imported $key"
done

echo "integration: ok (port $PORT, mock $MOCK_PORT)"
