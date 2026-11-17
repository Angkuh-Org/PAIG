# OpenCode free models (`opencode/*`)

`opencode/<id>` is PAIG's keyless backend for OpenCode's free inference
endpoint. This page is the user-facing explanation: what PAIG does on that
route, why each part exists, and what you need to run it.

## What you send

Nothing special. Point any OpenAI client at PAIG:

```sh
curl -s http://127.0.0.1:8899/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "opencode/beam-free",
    "messages": [{"role": "user", "content": "hello"}]
  }'
```

No API key is needed. PAIG strips the `opencode/` prefix and forwards the bare
model id to the free endpoint.

## What PAIG does

```
client
  │  POST /v1/chat/completions  {model:"opencode/beam-free"}
  ▼
PAIG :8899
  │  1. route + strip prefix, force stream=true / include_usage
  │  2. flatten history, insert title marker + correction, wrap user turn
  │  3. mint a local ses_ id, build the free headers
  │  4. run the in-process C transport over a socketpair
  ▼
C transport (inside the paig binary)
  │  5. TLS 1.3 handshake with Bun 1.4.4's exact ClientHello
  │     X25519MLKEM768 + X25519, ALPN http/1.1, verify opencode.ai
  │  6. HTTP/1.1 POST /inference/openai/v1/chat/completions
  ▼
opencode.ai  ──►  SSE chunks  ──►  C transport  ──►  PAIG  ──►  client
```

There is **one binary**. The C transport is compiled into `paig`; PAIG does not
spawn `zen-client`, the `opencode` CLI, Bun, or any helper process on the free
route. If the in-process transport fails, the request fails — there is no
fallback.

### 1. Route and payload

PAIG matches the `opencode/` prefix, checks the bare id against
`opencode.models`, and rewrites the payload:

- `model` becomes the bare id (for example `beam-free`);
- `stream` is forced to `true`;
- `stream_options.include_usage` is forced to `true`.

### 2. Title-shaped body

The free endpoint serves the CLI's **title** request, not a general chat
request. PAIG therefore builds the same shape:

- a `system` message containing the exact 1806-character title prompt
  (`FREE_MARKER`) plus a 142-character correction that tells the model to
  ignore the title instruction and answer the user;
- one `user` message containing:

  ```text
  Here is the first message of the conversation:

  <message>
  {the conversation, flattened to text}
  </message>

  Reply with only the title.
  ```

Multi-turn history is flattened into one user message; images and tool calls
are rejected rather than silently dropped.

### 3. Local session and headers

`ses_...` is **not signed** and is not minted by the CLI. PAIG generates it
locally:

- `now_ms * 4096 + counter`, inverted and truncated to 48 bits, as 12 lowercase
  hex characters;
- plus 14 random base62 characters;
- one id is cached per process; a 401/403/429 rotates it once behind a 60 s
  cooldown.

The C transport rewrites the headers into the proven Bun order:

```text
Authorization, Content-Type, User-Agent, x-opencode-client,
x-opencode-session, x-opencode-session-id, x-session-affinity,
x-session-id, Connection, Accept, Host, Accept-Encoding, Content-Length
```

`User-Agent` is `opencode/latest/<version>/cli`, default version `2.3.0`.

### 4. The in-process transport

PAIG creates a `socketpair`, writes one JSON metadata line plus the raw body to
one end, and runs the C shim on a worker thread against the other end:

```json
{"url":"https://opencode.ai/inference/openai/v1/chat/completions",
 "headers":{"Authorization":"Bearer public","...":"..."},
 "timeout_ms":180000}
```

The C shim reads the metadata and body until EOF, builds the exact HTTP/1.1
request, performs the TLS handshake, and writes one JSON status line followed
by the decoded body. PAIG streams that body to the client.

The ClientHello parity harness is test-only: `tests/zen_hello_test.c` links the
same `libzen.a`, writes the exact record, and `tests/zen_hello_parse.py`
asserts the 17-cipher/13-extension shape. It is built and run inside
`./tests/test.sh`, not installed, and not released. There is no helper binary.

### 5. Why a custom TLS client exists: the gate

The free endpoint only accepts a client whose TLS ClientHello looks like Bun
1.4.4's `fetch()`. Mbed TLS's built-in TLS 1.3 handshake cannot be configured
to produce that shape: it hardcodes the record version, extension order and
key-share count, has no ML-KEM group, and writes its own cipher list. The C
transport therefore implements the handshake itself and reuses Mbed TLS only
for crypto primitives and X.509 verification. It reproduces:

- record version `0x0301`, legacy `0x0303`, 32-byte session id;
- 17 ciphers in BoringSSL order;
- 13 extensions in BoringSSL order, ALPN `http/1.1`;
- key shares `X25519MLKEM768` (1216 B) and `X25519` (32 B);
- ML-KEM-768 from PQClean (CC0) + X25519, shared secret
  `ML-KEM || X25519`;
- TLS 1.3 suites 0x1301, 0x1302 and 0x1303 (the live endpoint negotiates
  0x1301; 0x1302/0x1303 were verified against local OpenSSL peers);
- certificate chain, hostname, `CertificateVerify` and `Finished` verified
  against PAIG's embedded 118-certificate CA bundle (about 200 KB).

The module links only against `libc`, `pthread` and PAIG's static Mbed TLS
archive. It needs no Go, CGO, cargo, CMake, or runtime CA file.

### 6. Streaming

The HTTP layer is HTTP/1.1 (Bun's `fetch` offers only `http/1.1`). The C shim
decodes `Transfer-Encoding: chunked` incrementally and writes each piece to the
socketpair; PAIG forwards it as SSE. No full-response buffering.

### 7. Retries

A 401/403/429 on the free route rotates the local session once and replays the
request. A second 429 inside 60 seconds is returned as-is so a real quota wall
does not churn sessions.

## Requirements

### Build time

- Linux with glibc.
- `rustc` 1.85+ with edition 2024 support.
- `cc` and `ar` (Mbed TLS and the in-process C transport).
- POSIX `sh` for `build.sh` and the tests.
- No Go, cargo, CMake, or Perl required for the gateway build. `python3` runs
  the test-only ClientHello parser (`tests/zen_hello_parse.py`).

### Runtime

- `paig` only. No `zen-client`, Bun, or `opencode` binary is needed for
  `opencode/*`.
- Outbound DNS + TCP 443 to `opencode.ai`.
- A reasonably correct system clock (TLS and `ses_` timestamps).
- No `ca-certificates` package: the CA bundle is embedded in `paig`.

### Config and environment

| Setting | Default | Meaning |
|---|---|---|
| `opencode.freeBaseUrl` | `https://opencode.ai/inference/openai/v1` | free upstream |
| `opencode.models` | built-in beam-free list | ids allowed under `opencode/` |
| `opencode.version` / `OPENCODE_VERSION` | `2.3.0` | `opencode/latest/<v>/cli` UA |
| `opencode.bin` / `OPENCODE_BIN` | empty | `opencode` binary used only to export the `opencode-go` credential |

`opencode.zenClient` / `OPENCODE_ZEN_CLIENT` is gone. If an old config still
sets it, PAIG warns that the key is ignored.

### Security

Certificate verification is on by default and covers the chain, hostname,
`CertificateVerify` and `Finished`. Test-only loopback switches can disable it
for a local TLS mock, but they apply only when the URL host is loopback and are
never consulted for a remote host: `PAIG_ZEN_TEST_FREE=1` forces the free
shape for a loopback `freeBaseUrl`; `PAIG_ZEN_SNI`, `PAIG_ZEN_DIAL` and
`PAIG_ZEN_INSECURE=1` are read only for a loopback URL. None of them is a
production fallback.

## Build and install

```sh
./build.sh          # build/paig, including the free transport

install -m 0755 build/paig /usr/local/bin/paig
```

Or use the installer, which downloads the single `paig-linux-<arch>` asset and
verifies its checksum:

```sh
curl -fsSL https://raw.githubusercontent.com/Angkuh-Org/PAIG/main/install.sh | sh
```

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `free transport status read failed` | The in-process C transport did not start or timed out; check the clock, DNS, and TCP 443. There is no helper/CLI fallback. |
| 429 `FreeUsageLimitError` | Shape mismatch or quota. Check the full marker + envelope; the session rotates once. `Retry-After` counts to UTC midnight, not a retry window. |
| Certificate verification failed | Check the clock and that the test-only insecure switch is not enabled against a remote host. |
| Output is short/title-like | The free route is a title generator being coerced; this is best-effort chat, not a chat-quality guarantee. |
| `opencode-go/*` asks for a credential | The Go tier still uses `opencode auth export`; only the free path is CLI-free. |

## Scope and limits

- **Free only.** `opencode/*` needs no CLI, helper, or Go toolchain.
  `opencode-go/*` still uses the CLI/`auth export` and a stored credential.
- **No fallback.** The free route is in-process only. A failed handshake,
  transport, or stream is returned as an error.
- **Best-effort replies.** Every observed free response is title-shaped; the
  correction makes it answer the user, but wording varies.
- **Quota.** The bucket is shared with the CLI; `Retry-After` is time to UTC
  midnight and does not distinguish a rejected shape from a real quota wall.
- **Thread cost.** One worker thread plus a request socketpair and a cancel
  socketpair per free request. The C transport is small and exits after the
  response; there is no child process and no Bun runtime. A downstream
  disconnect wakes the cancel fd, so an abandoned stream does not pin the
  worker for the full upstream timeout.
