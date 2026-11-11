# Vendored Mbed TLS

PAIG vendors Mbed TLS **3.6.7 LTS** for the TLS client. The upstream source is
the GitHub tag archive
<https://github.com/Mbed-TLS/mbedtls/archive/refs/tags/v3.6.7.tar.gz>,
SHA-256 `7312b70b067b6a271961c8d36c3b8f9ba3e86fe6b26f18af13cd70430ee52ed1`.

## License

Mbed TLS is dual-licensed **Apache-2.0 OR GPL-2.0-or-later**. PAIG is
GPL-3.0-only; either branch of the Mbed TLS dual license can be combined with
it. The full text is in `LICENSE`.

`ca-bundle.pem` is the Mozilla CA bundle converted by curl.se
(<https://curl.se/ca/cacert.pem>, fetched 2026-11-10, dated 2026-11-05,
SHA-256 `427c064216761a1e72a8b30f0d954d08cb6e4435a82091fb865afd20c8497285`)
and is covered by MPL-2.0; see `ca-bundle.LICENSE`.

## What is vendored

- `include/` and `library/*.h` — upstream headers, unchanged.
- `library/*.c` — only the modules in `sources.txt`. The list started from the linker map of a standalone TLS probe and was trimmed for the Rust shim, not copied wholesale from the library.
- `paig_config.h` — PAIG's module configuration.
- `ca-bundle.pem` — embedded trust store.

No upstream file is modified.

## Build

`tools/build.rs` compiles `sources.txt` with `cc`, archives the objects in `build/mbedtls/libmbedtls.a`, and `paig_config.h` and `sources.txt` are PAIG files.
links that archive into the gateway. A clean build needs the C compiler that
rustc already uses as its linker, plus `ar`.

The C flags are kept in `tools/build.rs`; they include `-O2`, `-fPIC`,
`-ffunction-sections`, `-fdata-sections`, `-march=native`, and
`-fvisibility=hidden`.

## Updating

1. Replace `include/` and `library/` from the new tag and record the archive
   SHA-256 above.
2. Regenerate the object list from a standalone TLS probe or add
   `-C link-arg=-Wl,-Map=build/paig.map` to `tools/build.rs` temporarily;
   update `sources.txt` with the objects actually pulled in.
3. Keep `MBEDTLS_HAVE_TIME_DATE`, `MBEDTLS_X509_RSASSA_PSS_SUPPORT`,
   `MBEDTLS_HKDF_C`, `MBEDTLS_SHA384_C`, and the P-384/P-521 curves. Without
   them CA parsing or TLS 1.3 silently loses coverage; `tests/test.sh` must
   prove the embedded bundle parses with zero failures.
4. Refresh `ca-bundle.pem` from <https://curl.se/ca/cacert.pem>.
5. Run `./tests/test.sh` and a real handshake against every upstream.

Mbed TLS 3.6 LTS receives security fixes until roughly March 2027. Plan the
upgrade to a 4.x LTS before then; 4.x moves PSA Crypto into TF-PSA-Crypto and
generates driver wrappers with Python, so it needs a separate vendoring pass.
