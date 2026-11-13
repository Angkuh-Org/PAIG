# Build, test, and benchmark

## Build

```sh
./build.sh          # incremental rustc + Mbed TLS build -> build/paig
./build.sh --check  # type-check only -> build/paig.rmeta
./build.sh run      # build, then run in the foreground
./build.sh clean    # remove build artifacts, including C objects
```

Build orchestration lives in `tools/build.rs`: `build.sh` bootstraps it with
one rustc invocation when missing or stale, then the orchestrator builds the
vendored Mbed TLS archive and runs the gateway compile plus the G1/G2/G4 gates.
The gateway itself is still one rustc invocation.

The gateway uses these rustc flags:
```text
--edition 2024 -C opt-level=3 -C lto=fat -C codegen-units=1
-C panic=abort -C strip=symbols -C target-cpu=native
-C link-arg=-Wl,--gc-sections -C link-arg=-Wl,--as-needed
-C link-arg=-fuse-ld=lld
```

The vendored Mbed TLS sources use `cc` with `-O2 -march=native -fPIC
-ffunction-sections -fdata-sections -fvisibility=hidden -fno-unwind-tables
-fno-asynchronous-unwind-tables -DNDEBUG`, then `ar` archives them in
`build/mbedtls/libmbedtls.a`. Objects are cached in `build/mbedtls/obj` and
rebuilt when a source, header, config, source list, or compiler flag changes.
incremental builds reuse the C objects and
only relink when the archive changes. 

`PAIG_TARGET_CPU` and `PAIG_MARCH` replace the native CPU baseline for rustc
and the vendored C sources; local builds default to `native`. Release builds set
them to `x86-64-v2`/`x86-64-v2` and `generic`/`armv8-a` so a binary runs on
machines other than the builder. `PAIG_VERSION` stamps the `--version` output.
The stripped binary targets 1.5 MiB (1,572,864 bytes); the build gate warns
above it but never fails the build (G4). The embedded CA bundle is part of the
binary, so there is no runtime `libssl`/`libcrypto` or system CA file.

`cc`, `ar`, and `lld` are the only additions to the build toolchain; rustc
uses `cc` as its linker driver, and the build passes `-fuse-ld=lld` through.
Mbed TLS 3.6.7 needs no CMake, Perl, Python, or
generated build files. Vendor provenance and the update procedure are in
[`vendor/mbedtls/README.paig.md`](../vendor/mbedtls/README.paig.md).

## Profiling

`build/paig` is stripped. For disassembly or callgrind, build a symbol-bearing
twin with the same flags and the source path spelled exactly as the build does
(the path is embedded, so a different argv shifts PC-relative addresses):

```sh
ROOT=$PWD
PAIG_VERSION=dev rustc --edition 2024 -C opt-level=3 -C lto=fat \
  -C codegen-units=1 -C panic=abort -C debug-assertions=off \
  -C overflow-checks=off -C target-cpu=generic -C strip=none \
  -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,--as-needed \
  -C link-arg=-fuse-ld=lld \
  -C link-arg=$ROOT/build/mbedtls/libmbedtls.a -C link-arg=-lpthread \
  -o $ROOT/build/paig.nostrip $ROOT/src/main.rs
objcopy -O binary --only-section=.text build/paig /tmp/rel.text
objcopy -O binary --only-section=.text build/paig.nostrip /tmp/sym.text
sha256sum /tmp/rel.text /tmp/sym.text   # must match before trusting symbols
```

`build/paig.nostrip` keeps the symbol table but not debug info, so the code is
identical to the release. Build it with the same CPU/march flags as the
artifact under test (`generic`/`armv8-a` for a release, `native` for a local
build), or the hash will not match. Profile it with `valgrind --tool=callgrind
--cache-sim=yes --branch-sim=yes --dump-instr=yes`; the code section hash, not
the file name, is what makes the profile trustworthy.

The v0.1.0 vs v0.1.1 comparison chart is
[`images/profile-v0.1.0-vs-v0.1.1.png`](images/profile-v0.1.0-vs-v0.1.1.png).
The v0.2.0 models.dev capability-path chart is
[`images/profile-v0.2.0-pre-vs-release.png`](images/profile-v0.2.0-pre-vs-release.png).

## Commands

| Command | Action |
| --- | --- |
| *(none)* | incremental build through `build/paig-build` (G1/G2/G4) |
| `--check` | type-check only, no link |
| `run` | build incrementally, then run in the foreground (extra args go to the binary) |
| `clean` | remove `build/paig`, `paig.rmeta`, `paig-test`, `paig.stamp`, `build/mbedtls` |
| `bench` | build, then run `tests/bench.sh` |

Tests use their own entrypoint: `./tests/test.sh`.

## CI and releases

`.github/workflows/ci.yml` builds and tests `x86_64` and `aarch64` on Ubuntu
22.04. Pushing a `v*` tag runs `.github/workflows/release.yml`, which tests,
stamps the tag into `paig --version`, attests provenance, and publishes
`paig-linux-x86_64` and `paig-linux-aarch64` with matching `.sha256` files.
[`install.sh`](../install.sh) consumes those assets; usage and verification are
in [install.md](install.md).

## Test and benchmark

```sh
./tests/test.sh      # 61 unit tests + real-binary integration test
./build.sh bench     # binary size, startup, idle RSS, health/chat latency
```

`tests/test.sh` delegates unit and integration testing to
`build/paig-build test`; `tests/bench.sh` owns the binary/RSS/latency perf
check. The model screening benchmark (JSONL + per-backend reports) lives in
`benchmark/`.

The integration test starts a local Python mock upstream (no external network)
and exercises Command Code translation, OpenCode free + ledger, Kilo, Bob,
Mistral, streaming, model rewriting, the excluded-model guard, and TLS
certificate-error reporting. The unit suite also parses the embedded CA bundle
and checks the SHA-256/RNG primitives. Tests need `python3`, `curl`, and the
`openssl` CLI; the gateway itself does not.
