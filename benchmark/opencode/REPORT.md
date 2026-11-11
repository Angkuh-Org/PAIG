# OpenCode — free wall + Go models (2026-11-10)

**Status: free matrix blocked; Go models complete.** The ten `opencode/*` free
models returned no usable answer: nine were HTTP 429 (`FreeUsageLimitError`) on
all 19 requests, `crouton-alpha-free` was HTTP 410 (`ModelDeprecated`) on all
19, and `pulse-alpha-free` was HTTP 400 ("Model is unavailable") on all 19. The
free run is capacity-limited and has no quality ranking.

The `opencode-go/*` route is complete: `beam-free` scored 8/9 and
`iceman-contributor-free` 6/9, both with 10/10 stability. `crouton-alpha-free`
on the Go route was excluded after a 410 deprecation probe.

## Go results

| model | suite | stability | 3-bullet | suite median | stability median | comp tokens med |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `opencode-go/beam-free` | **8/9** | 10/10 | 10/10 | 1.903 s | 2.064 s | 141 |
| `opencode-go/iceman-contributor-free` | 6/9 | 10/10 | 10/10 | 3.167 s | 9.805 s | 127 |

### Task matrix

| task | beam-free | iceman |
| --- | --- | --- |
| math | PASS | PASS |
| modpow | PASS | **FAIL** |
| python-output | **FAIL** | PASS |
| json-extract | PASS | PASS |
| logic | PASS | PASS |
| sql | PASS | PASS |
| indonesian-json | PASS | **FAIL** |
| needle | PASS | PASS |
| tool-call | PASS | **FAIL** |

`beam-free` has only the Python mutable-default trap. `iceman` passed Python
but failed modpow, the Indonesian counting task, and the tool-call task
(no valid `get_exchange_rate(USD, 2026-03-14)` call).

## Free wall

| model | status | count | error |
| --- | --- | ---: | --- |
| `opencode/beam-free` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/big-pickle` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/deepseek-v4.1-flash-free` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/flint-40b-free` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/glm-5.3-flash-free` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/mimo-v2.6-flash-free` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/nemotron-3.5-lightning-free` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/nemotron-3.5-super-vl-free` | 429 | 19 | `FreeUsageLimitError` |
| `opencode/crouton-alpha-free` | 410 | 19 | `ModelDeprecated` |
| `opencode/pulse-alpha-free` | 400 | 19 | "Model is unavailable" |

The 2026-11-05 run saw roughly 25 successful free requests with
`OPENCODE_API_KEY`; on 2026-11-10 the bucket was already exhausted before the
first measured request. The API key was present (`OPENCODE_API_KEY` from
`~/.qwen/settings.json` env `OPENCODE_GO`) and it unlocks the Go route, but it
does **not** remove the free-tier limit. The summary marks every free model
`graded: 0` and its ranking is empty; the `0/9` cells are attempted requests,
not nine graded model answers.

## Caveats

- The free numbers are a quota wall, not a model-quality result. Do not rank
  the ten free models from this run.
- `opencode-go/*` is the only complete OpenCode data in this snapshot.
- The key comes from the user's own `~/.qwen/settings.json`; no key is stored
  in this repository.
- Latency is end-to-end through PAIG. `iceman` runs through OpenCode Go and is
  much slower on stability (9.805 s median) than `beam-free` (2.064 s).
- The tool-call task is one function call per model; one pass is not a robust
  multi-tool claim.

## Raw evidence

- Go run: `runs/opencode-go-20261110-194120.jsonl` (38 records)
  SHA-256 `d1fc563bef704ff9bec6947cafed7ad1ab397c808082feb5d3ec6d651f461a4e`
- Go summary: `runs/opencode-go-20261110-194120-summary.json`
  SHA-256 `d07ddb3a33f9c41617db9d34dc370964a5a4e37d2a0148fc6bc251c61e635bfd`
- Free run: `runs/opencode-free-20261110-194500.jsonl` (190 records)
  SHA-256 `1bc9d7bf60d83f1af71921f679eb7ccf305b1a8ed79c66497286377821bc9098`
- Free summary: `runs/opencode-free-20261110-194500-summary.json`
  SHA-256 `953ada48072ca98101c09053139db66081f5038214dc03d289865004266e8f99`

Reproduce:

```sh
OPENCODE_API_KEY=... ./build/paig
python3 benchmark/harness/bench.py --tag opencode-go \
  --out-dir benchmark/opencode/runs --no-cwd \
  --models opencode-go/iceman-contributor-free opencode-go/beam-free
python3 benchmark/harness/bench.py --tag opencode-free \
  --out-dir benchmark/opencode/runs --no-cwd \
  --models opencode/beam-free opencode/big-pickle opencode/crouton-alpha-free \
  opencode/deepseek-v4.1-flash-free opencode/flint-40b-free opencode/glm-5.3-flash-free \
  opencode/mimo-v2.6-flash-free opencode/nemotron-3.5-lightning-free \
  opencode/nemotron-3.5-super-vl-free opencode/pulse-alpha-free
```
