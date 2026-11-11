# Rulebook

- **G1 - No cargo, no crates.** Gateway: one `rustc` invocation plus the vendored Mbed TLS C archive; `std` plus raw FFI only.
- **G2 - Only sanctioned runtime libs.** `libc` and glibc support libraries; vendored Mbed TLS is statically linked. `libssl`/`libcrypto` are not sanctioned.
- **G3 - Bounded memory.** 64 MiB body, 64 KiB headers, 256 connections, 256 KiB stacks. Limits are per request/connection; total peak scales with concurrency and request copies, not a global byte budget.
- **G4 - Small binary.** Stripped release target <= 1.5 MiB; the build gate warns on regression.
- **G5 - Stream, do not hoard.** SSE is forwarded chunk by chunk; no full-response buffering.
- **G6 - Loopback by default.** `127.0.0.1:8899` unless config explicitly says otherwise.
- **G7 - Edge compatibility.** OpenAI shape at `/v1`; backend quirks normalized per handler.

`./build/paig --rules` prints the current scorecard.
