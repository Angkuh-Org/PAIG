#!/bin/sh
# Unit tests, then the local mock-upstream integration test.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d /tmp/paig-tests.XXXXXX)
trap 'rm -rf "$TMP"' EXIT INT TERM
"$ROOT/build.sh" >/dev/null
"$ROOT/build/paig-build" test "$@"

# The cancellation path is C-side: a socketpair read end is polled alongside
# the upstream fd, so a downstream disconnect wakes a blocked worker.
"${CC:-cc}" -O2 -Wl,--gc-sections -I "$ROOT/tools/zen-client-c/src" \
    "$ROOT/tests/zen_cancel_test.c" \
    "$ROOT/build/zen/libzen.a" "$ROOT/build/mbedtls/libmbedtls.a" \
    -lpthread -o "$ROOT/build/zen-cancel-test"
"$ROOT/build/zen-cancel-test"

# Test-only ClientHello parity: build the record from the same libzen.a and
# let the Python parser assert the 17-cipher / 13-extension Bun shape. This is
# a test harness, not an installable helper.
"${CC:-cc}" -O2 -Wl,--gc-sections -I "$ROOT/tools/zen-client-c/src" \
    "$ROOT/tests/zen_hello_test.c" \
    "$ROOT/build/zen/libzen.a" "$ROOT/build/mbedtls/libmbedtls.a" \
    -lpthread -o "$ROOT/build/zen-hello-test"
"$ROOT/build/zen-hello-test" >"$TMP/hello.bin"
python3 "$ROOT/tests/zen_hello_parse.py" "$TMP/hello.bin"

# Value-level X25519 gate. The parity harness above checks only the shape of
# the key shares; this pins RFC 7748 and cross-checks the local ladder against
# Mbed TLS, so a wrong public key or shared secret cannot pass CI.
"${CC:-cc}" -O2 -Wl,--gc-sections \
    -I "$ROOT/tools/zen-client-c/src" \
    -DMBEDTLS_CONFIG_FILE='"paig_config.h"' \
    -I "$ROOT/vendor/mbedtls/include" -I "$ROOT/vendor/mbedtls" \
    "$ROOT/tests/zen_x25519_test.c" \
    "$ROOT/build/zen/libzen.a" "$ROOT/build/mbedtls/libmbedtls.a" \
    -lpthread -o "$ROOT/build/zen-x25519-test"
"$ROOT/build/zen-x25519-test"
