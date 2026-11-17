# PAIG - (Probably AI Gateway)

a local gateway that lets OpenAI-shaped clients
reach several model backends through one endpoint. SDKs, CLIs, and agents
point at `http://127.0.0.1:8899/v1`; PAIG picks the backend from the model id
prefix, forwards the request, and normalizes the reply to the OpenAI `/v1`
shape.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Angkuh-Org/PAIG/main/install.sh | sh
```

The installer fetches the `x86_64` or `aarch64` release binary, verifies its
SHA-256, and installs `paig` to `~/.local/bin` (or `/usr/local/bin` as root):

```text
  ✔ verify    paig v0.1.1
```

Release binaries need glibc 2.35+. Set `PAIG_INSTALL_DIR` to change the target
directory. If `~/.local/bin` is not on `PATH`, run the `export` line the
installer prints. Manual downloads, checksums, and provenance verification are
in [`docs/install.md`](docs/install.md).

## Build from source

The binary is built from `std` plus raw FFI over vendored Mbed TLS: no cargo,
no crates. TLS and the CA bundle are linked into the binary.

## Requirements

- Linux with glibc
- `rustc` 1.85+ with edition 2024 support
- `cc` and `ar` for the vendored Mbed TLS build and the in-process OpenCode free transport
- POSIX `sh` for `build.sh` and `tests/*.sh`
- A reasonably correct system clock

G2's sanctioned set is `libc` and glibc support libraries (`libm`,
`libpthread`, `libdl`, `librt`, `libgcc_s`). The build gate checks `ldd` after
linking and fails on anything else.

## Quick start

Build the gateway:
```sh
./build.sh                   # rustc + Mbed TLS build -> build/paig
```

`opencode/*` needs no extra binary: the C TLS/HTTP transport is compiled into
`paig`. The ClientHello parity harness is test-only and runs inside
`./tests/test.sh`; it is not installed or released.

See [`docs/opencode-free.md`](docs/opencode-free.md) for the full mechanism and
the user-facing requirements.

Create the config:
```sh
./build/paig --init          # write ~/.paig/config.json
```

`--init` writes `~/.paig/config.json` with mode `0600` and prints the
credential state of every backend. Missing keys do not fail the command: each
backend reads its key from the environment or its `apiKey`/`apiKeyFile` fields
in `config.json`.

Start the server:
```sh
./build/paig                 # serve on 127.0.0.1:8899
```

Send a request:
```sh
curl -s http://127.0.0.1:8899/health
curl -s http://127.0.0.1:8899/v1/models
curl -s http://127.0.0.1:8899/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "opencode/beam-free",
    "messages": [{"role": "user", "content": "hello"}]
  }'
```

Point any OpenAI SDK at `http://127.0.0.1:8899/v1`. The gateway does not
authenticate inbound requests: keep it on loopback unless an authenticating
proxy sits in front.

Background control (`paig.pid` and `paig.log` live in `~/.paig/`):
```sh
./build/paig --start      # detached start; write ~/.paig/paig.pid, append ~/.paig/paig.log
./build/paig --status
./build/paig --stop
./build/paig --restart
```

## Performance

v0.1.1 cuts per-request JSON/SSE work without changing the wire format.
Callgrind on hash-verified release twins, same synthetic workload:

![PAIG v0.1.0 vs v0.1.1 benchmark](docs/images/profile-v0.1.0-vs-v0.1.1.png)

- streaming: 78% fewer instructions, 4.51× less CPU work
- non-streaming: 47% fewer instructions, 1.90× less CPU work
- health: 17% fewer instructions, 1.21× less CPU work
- D1 write misses on health: 98% fewer; allocator cost on streaming: 89% less
- binary: 1.245 MB → 1.248 MB, still under the current 2 MiB gate

These are loopback instruction-count ratios, not wall-clock latency. Method and
profiling recipe: [`docs/build.md`](docs/build.md#profiling).

### v0.2.0 model catalogue

The capability path no longer clones the 9,136-entry models.dev catalogue on
every `/v1/models` request: the cache is shared behind `Arc`, lookups borrow
`provider`/`id` instead of formatting a key, and only the five routed providers
are converted. Callgrind on the same 264-model workload:

![PAIG v0.2.0 pre-optimization vs release benchmark](docs/images/profile-v0.2.0-pre-vs-release.png)

- warm `/v1/models`: 49% fewer instructions, 1.97× less CPU work
- cold `/v1/models` (6 MB catalogue parse): 4.7% fewer instructions, 1.05× less
- `aggregate_sse` static disassembly: 7.3% fewer instructions, 30% fewer stack refs
- release binary: 1,297,720 → 1,300,080 B, still under the current 2 MiB gate

## Model routing

The model prefix selects the backend; the prefix is stripped before the
request is forwarded. First matching prefix wins. Unprefixed ids are rejected
with HTTP 400 (`model_unrouted`); there is no default backend.

| Model id | Backend | Credential |
| --- | --- | --- |
| `opencode/<id>`, `oc/<id>` (also `opencode:<id>`) | OpenCode free | none |
| `opencode-go/<id>` | OpenCode Go | `OPENCODE_API_KEY` / `OPENCODE_API_KEY_FILE` / `opencode auth export` |
| `kilo-free/<id>` | Kilo free | none; only ids ending in `:free` |
| `kilo/<id>`, `kilocode/<id>` | Kilo keyed | `KILO_API_KEY` (required) |
| `bob/<id>` | IBM Bob | `PAIG_BOB_API_KEY` / `PAIG_BOB_API_KEY_FILE` |
| `mistral/<id>` | Mistral | `PAIG_MISTRAL_API_KEY` / `PAIG_MISTRAL_API_KEY_FILE` |
| `cc/<id>`, `commandcode/<id>` | Command Code `/alpha/generate` | `CC_API_KEY` (required) |

Examples: `opencode/beam-free`, `opencode-go/iceman-contributor-free`,
`kilo-free/nvidia/nemotron-3.5-super-vl:free`, `kilo/kilo-auto/efficient`, `bob/<model>`,
`mistral/mistral-large-4`, `cc/deepseek/deepseek-v4.1-flash`.

Backend details: [OpenCode free](docs/opencode-free.md) · [Kilo](docs/kilo.md).

## Configuration

Precedence, lowest to highest:
1. built-in defaults
2. `~/.paig/config.json`, then `./paig.config.json`
3. `~/.paig/.env`, then `./.env`
4. process environment

Files in the same layer merge; later files and higher layers win.
`PAIG_CONFIG=<file>` replaces config-file discovery with one file, and
`PAIG_ENV_FILE=<file>` does the same for env-file discovery. Empty string
values for string keys in env files do not erase a value from JSON.

Copy the examples:
```sh
mkdir -p ~/.paig
cp paig.config.example.json ~/.paig/config.json
cp .env.example .env
```

Inspect the result:
```sh
./build/paig --check          # sources, key status, warnings, errors
./build/paig --print          # resolved config; secret values masked
./build/paig --get server.port
./build/paig --init --force   # rewrite ~/.paig/config.json
```

`--check` exits non-zero on validation errors. Config and env files that are
group/world readable produce a `chmod 600` warning.

## Key setup

One import flag covers every backend; the prefix selects it
(`opencode`/`oc`, `kilo`, `bob`, `mistral`, `commandCode`/`cc`):
```sh
./build/paig --init \
  --from-auth opencode:~/.paig/opencode.key \
  --from-auth kilo:~/.paig/kilo.key \
  --from-auth bob:~/.paig/bob.key \
  --from-auth mistral:~/.paig/mistral.key \
  --from-auth cc:~/.paig/cc.key
```

Key files accept JSON (`apiKey`/`api_key`/`key`/`token`) or a bare key, and
`~` is expanded. Imported keys land in the matching `apiKey` field of
`~/.paig/config.json`.

`--init` writes only keys passed with `--from-auth`; keys from each backend's
env var or `apiKey`/`apiKeyFile` in `config.json` apply at runtime and are not
written. `apiKey` wins if both are set.

SSE, response shape, and error semantics: [docs/api.md](docs/api.md).

Environment variables and model capabilities: [docs/config.md](docs/config.md).

## CLI

```text
paig                                   run the server
paig --init [--force] [--from-auth [backend:]file]  create ~/.paig/config.json
paig --print                           print resolved config (secrets masked)
paig --check                           validate config and show sources
paig --get <dotted.key>                print one resolved value
paig --rules                           print the game rulebook
paig --start                           start in the background (pid/log in ~/.paig/)
paig --stop                            stop the background server
paig --restart                         stop, then start in the background
paig --status                          report running / not running (exit 1 when not running)
```

`--init` refuses to overwrite an existing config unless `--force` is given;
with `--force` it keeps existing values and fills in missing ones. The file is
written atomically with mode `0600`, and missing credentials are reported per
backend without failing the command.

## Development

Build flags, `build.sh` commands, tests, and benchmark: [docs/build.md](docs/build.md).
Rules G1-G7 and the scorecard: [docs/rules.md](docs/rules.md).

## License

GPL-3.0-only. Bundled third-party components: Mbed TLS 3.6.7 (Apache-2.0 OR
GPL-2.0-or-later) and the Mozilla CA bundle converted by curl.se (MPL-2.0).
See `vendor/mbedtls/LICENSE`, `vendor/mbedtls/ca-bundle.LICENSE`, and
`vendor/mbedtls/README.paig.md`.
