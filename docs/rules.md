# Rulebook

- **G1 - No cargo, no crates.** Gateway: one `rustc` invocation plus the vendored Mbed TLS and in-process free-transport C archives; `std` plus raw FFI only.
- **G2 - Only sanctioned runtime libs.** `libc` and glibc support libraries; vendored Mbed TLS is statically linked. `libssl`/`libcrypto` are not sanctioned.
- **G3 - Bounded memory.** 64 MiB body, 64 KiB headers, 256 connections, 256 KiB stacks. Limits are per request/connection; total peak scales with concurrency and request copies, not a global byte budget.
- **G4 - Small binary.** Stripped release target <= 2 MiB; the build gate warns on regression.
- **G1/G4 scope.** The OpenCode free transport is compiled into `paig`: the C TLS/HTTP module runs in-process over a socketpair, so there is no helper binary to install. The ClientHello parity harness is a test-only C executable over the same `libzen.a`; it is not a runtime dependency. The vendored Mbed TLS TLS 1.3 client cannot be configured to produce the Bun-shaped ClientHello the free endpoint requires, so that handshake is implemented in the C module and reuses Mbed TLS only for crypto primitives.
- **G5 - Stream, do not hoard.** SSE is forwarded chunk by chunk; no full-response buffering.
- **G6 - Loopback by default.** `127.0.0.1:8899` unless config explicitly says otherwise.
- **G7 - Edge compatibility.** OpenAI shape at `/v1`; backend quirks normalized per handler.

`./build/paig --rules` prints the current scorecard.
