# Environment variables and model capabilities

## Environment variables

| Variable | Default | Purpose |
| --- | --- | --- |
| `PAIG_HOST` | `127.0.0.1` | Bind address (G6: loopback by default) |
| `PAIG_PORT` | `8899` | Bind port, 1-65535 |
| `PAIG_DEBUG` | `0` | Verbose request logging |
| `PAIG_LEDGER` | `~/.paig/paig-ledger.jsonl` | JSONL usage ledger, mode `0600` |
| `CC_API_KEY` | empty | Command Code key; required for `cc/` and `commandcode/` models |
| `CC_API_KEY_FILE` | empty | JSON or plain-text key file |
| `CC_MODELS` | unset | Comma list of bare ids; used when the live catalogue is unavailable |
| `PAIG_EXCLUDED_MODELS` | built-in list | Replaces the exclusion list; empty disables |
| `CC_FETCH_RETRIES` | `2` | Retries for transient upstream failures |
| `CC_FETCH_BACKOFF_MS` | `500` | Linear backoff base |
| `CC_USER_AGENT` | `cli` | `user-agent` identity header |
| `CC_CLI_VERSION` | `1.88.0` | `x-command-code-version` header |
| `CC_CLI_ENVIRONMENT` | `production` | `x-cli-environment` header |
| `CC_TASTE_LEARNING` | `true` | `x-taste-learning` header |
| `OPENCODE_API_KEY` | empty | OpenCode Go credential |
| `OPENCODE_API_KEY_FILE` | empty | OpenCode key file |
| `OPENCODE_BIN` | empty | `opencode` binary used for `opencode-go/*` `auth export`; falls back to `~/.opencode/bin/opencode` |
| `OPENCODE_VERSION` | `2.3.0` | `opencode/latest/<version>/cli` User-Agent for the free path |
| `OPENCODE_MODELS` | built-in free list | Comma list of free model ids |
| `OPENCODE_GO_MODELS` | built-in Go list | Comma list of Go model ids |
| `OPENCODE_TIMEOUT_MS` | `180000` | OpenCode upstream timeout |
| `KILO_API_KEY` | empty | Required for keyed `kilo/` models; `kilo-free/` is keyless |
| `KILO_ALL_MODELS` | `0` | With a key, expose the full paid catalogue under `kilo/`; free models always live under `kilo-free/` |
| `PAIG_BOB_API_KEY` | empty | Bob credential |
| `PAIG_BOB_API_KEY_FILE` | empty | Bob key file, e.g. `~/.paig/bob.key` |
| `PAIG_BOB_TEAM_ID` | empty | Bob team id |
| `PAIG_BOB_USER_AGENT` | `bob-shell/2.0.9` | Bob user agent |
| `PAIG_MISTRAL_API_KEY` | empty | Mistral credential |
| `PAIG_MISTRAL_API_KEY_FILE` | empty | Mistral key file, e.g. `~/.paig/mistral.key` |
| `PAIG_MISTRAL_MODELS` | unset | Curated ids; unset lists every chat model |
| `PAIG_CONFIG` | unset | Use one config file instead of discovery |
| `PAIG_ENV_FILE` | unset | Use one env file instead of discovery |

## Model capabilities

Every `/v1/models` entry carries a `capabilities` object built from the backend's own
metadata (Bob `model_info`, Mistral `capabilities`, Kilo modalities/parameters, Command Code
`context_length`). Fields a backend does not report are filled from
[models.dev](https://models.dev) as estimates; upstream values always win. The models.dev
catalogue refreshes in the background and is cached at `~/.paig/models-dev.json` for 24 hours,
so a cold request is not blocked by the download.

Set `modelsDev.url` to `""` in `config.json` to disable the fallback and the outbound request.
`modelsDev.blocking` is a test-only switch.
