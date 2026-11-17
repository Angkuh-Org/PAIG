# zen-client-c

The C transport body for PAIG's OpenCode free route. It is compiled into the
`paig` binary; there is no standalone `zen-client` executable, no `build.sh
zen` target, and no install/release artifact.

## Layout

| file | role |
|---|---|
| `src/tls.c` | TLS 1.3 client: Bun 1.4.4 ClientHello, X25519MLKEM768, record layer, certificate verification |
| `src/x25519.c` | RFC 7748 X25519 on five 51-bit limbs; no MPI, no heap |
| `src/http.c` | HTTP/1.1 request builder and chunked response decoder |
| `src/json.c`, `src/url.c` | metadata parser and URL parser |
| `src/util.c` | buffers, thread-local errors, timeout/poll helpers, cancellation fd |
| `src/main.c` | `zen_shim_run`: metadata/body in, status/body out, over a socketpair |
| `src/zen.h`, `src/zen_shim.h` | C API |
| `mlkem/` | PQClean ML-KEM-768 (CC0) |

`tools/build.rs` compiles these sources into `build/zen/libzen.a` and links
that archive before `build/mbedtls/libmbedtls.a`. The gateway supplies its
existing Rust `include_bytes!` CA bundle through
`src/transport/zen_inprocess/ca_bundle.h` and `paig_zen_set_ca`; this tree
does not embed a second 200 KiB PEM copy.

## Runtime contract

PAIG creates a socketpair, writes one JSON metadata line followed by the raw
request body, and runs `zen_shim_run(in_fd, out_fd, opts)` on a 256 KiB worker
thread. The shim writes one JSON status line followed by the decoded response
body. `opts` carries the loopback-only test switches (`insecure`, `sni`,
`dial`) and a cancel fd; shutting down the cancel socket wakes a blocked
`zen_wait_fd`.

The gateway never spawns a process. The old standalone CLI (`shim`, `spec`,
`live` modes) has been removed.

## Tests

`tests/test.sh` builds three test-only executables from the same archive:

- `tests/zen_cancel_test.c` — proves `zen_wait_fd` observes the cancel fd;
- `tests/zen_hello_test.c` — writes the exact ClientHello record, then
  `tests/zen_hello_parse.py` asserts the 17-cipher / 13-extension /
  X25519MLKEM768 shape;
- `tests/zen_x25519_test.c` — pins RFC 7748 §5.2 / §6.1 and cross-checks
  random key generation and shared secrets against Mbed TLS, because the
  parity harness checks key-share lengths, not values.

They are not installed or released. The integration suite uses
`tests/zen_mock.py` for the full in-process request path.

## TLS gate

The free endpoint only accepts Bun 1.4.4 `fetch()`'s ClientHello. Mbed TLS's
built-in TLS 1.3 handshake cannot be configured to that shape: its record
version, extension order and one-key-share writer are hardcoded, and the
vendored 3.6.7 tree has no ML-KEM group. This module owns the handshake and
uses Mbed TLS only for crypto primitives and X.509 verification. The two
X25519 key shares and the shared secret use the local five-limb ladder in
`src/x25519.c`, not Mbed TLS ECP. Certificate verification is on by default;
the insecure switch is refused unless both the URL host and dial host are
loopback.
