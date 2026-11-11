# Mistral — measured screening (2026-11-10)

**Status: complete but capacity-limited.** 7 models × (9 correctness + 10
stability) = 133 measured requests. All models answered, but
`mistral/codestral-latest` and `mistral/mistral-code-latest` each hit 5 HTTP 429
(`rate_limited`) responses. The ranking is therefore **not** a quality ranking
for those two.

Two backend requirements shaped the run:

- Mistral rejects `temperature=0` with the default `top_p`
  (`top_p must be 1 when using greedy sampling`). The harness sends `top_p=1`.
- Mistral rejects the `cwd` field with HTTP 422; every request used `--no-cwd`.

## Result

**Best correctness: `mistral/mistral-large-4` — 8/9.** Only the Python
mutable-default task failed. Its stability score is **0/10 at 1024 max_tokens**
because it spends the whole budget on reasoning and returns `finish_reason:
length` with empty content; a 3-round post-hoc at 4096 tokens was usable 3/3
with 3 bullets, so this is a budget limit, not an empty-answer defect.

**Best stable small model: `mistral/voxtral-small-latest` — 5/9, 10/10
stability.** The three `ministral` models are 4/9 with 10/10 stability.
`codestral` (4/9) and `mistral-code` (3/9) are capacity-limited by 429s.

| model | suite | stability | suite median | suite p90 | comp tokens med | 429 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `mistral/mistral-large-4` | **8/9** | 0/10* | 3.665 s | — | 318 | 0 |
| `mistral/voxtral-small-latest` | 5/9 | 10/10 | 0.990 s | 1.655 s | 60 | 0 |
| `mistral/ministral-14b-latest` | 4/9 | 10/10 | 1.732 s | 2.404 s | 28 | 0 |
| `mistral/ministral-8b-latest` | 4/9 | 10/10 | 1.022 s | 1.281 s | 28 | 0 |
| `mistral/ministral-3b-latest` | 4/9 | 10/10 | 0.819 s | 1.001 s | 63 | 0 |
| `mistral/codestral-latest` | 4/9 | 7/10 | 1.574 s | 2.363 s | 71 | 5 |
| `mistral/mistral-code-latest` | 3/9 | 7/10 | 1.007 s | 1.404 s | 71 | 5 |

\* `mistral-large-4` stability is budget-limited at 1024 tokens; see post-hoc
below. It is not a 0/10 quality result.

## Task matrix

| task | large-4 | voxtral | ministral-14b | ministral-8b | ministral-3b | codestral | mistral-code |
| --- | --- | --- | --- | --- | --- | --- | --- |
| math | PASS | PASS | **FAIL** | **FAIL** | PASS | **FAIL** | **FAIL** |
| modpow | PASS | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** |
| python-output | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** |
| json-extract | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| logic | PASS | **FAIL** | PASS | **FAIL** | **FAIL** | **FAIL** | **FAIL** |
| sql | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| indonesian-json | PASS | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** |
| needle | PASS | PASS | **FAIL** | PASS | **FAIL** | PASS | **FAIL** |
| tool-call | PASS | PASS | PASS | PASS | PASS | PASS | PASS |

**All seven returned a valid `get_exchange_rate(USD, 2026-03-14)` tool call.**

## Shared failures

- **Python mutable-default trap:** all seven answered
  `[1] [1, 2] [1, 2, 3]` instead of `[1, 2, 3] [1, 2, 3] [1, 2, 3]`.
- **Modpow:** only `mistral-large-4` passed `7^123 mod 1000`; the other six
  failed, mostly by not emitting the requested `ANSWER:` line or by a wrong
  value.
- **Indonesian counting:** only `mistral-large-4` returned `kode: 2`; the rest
  failed the exact JSON task.
- **Tool call:** all seven passed.

## `mistral-large-4` stability post-hoc (4096 tokens)

| round | finish | bullets | latency |
| --- | --- | ---: | ---: |
| 1 | stop | 3 | 21.334 s |
| 2 | stop | 3 | 38.913 s |
| 3 | stop | 3 | 29.735 s |

At the pre-registered 1024-token stability budget the same model returned
`finish=length` and no content on 10/10 rounds. The evidence is explicit: every
one of the 10 records has `completion_tokens=1024`,
`reasoning_tokens=1023`, and `reasoning_chars` between 3,560 and 4,340. The
model used the entire budget on reasoning before emitting any content. The
4096-token probe is exploratory and does not change the pre-registered score; it
shows the failure is budget exhaustion, not an adapter bug.

## Caveats

- The run is capacity-limited: `codestral` and `mistral-code` each have 5/19
  requests answered with 429. Their correctness/stability numbers are lower
  bounds, not quality rankings.
- `mistral-large-4` is a reasoning model; the 1024-token stability phase is too
  small for it. Do not read 0/10 as instability.
- One sample per task per model. A 8/9 or 5/9 is a screening result.
- Latency is end-to-end through PAIG on this machine; it includes Mistral's
  upstream reasoning time. The 4096-token post-hoc is much slower by design.
- The SQL task executes generated SQL in an in-memory SQLite database with
  forbidden-keyword screening.
- Mistral is a paid backend; this report does not include a cost figure because
  the API response exposes tokens, not a per-request credit amount.

## Raw evidence

- Main run: `runs/mistral-20261110-192742.jsonl` (133 records)
  SHA-256 `906fc7d43196b515ee2b49c960efd079ac1b4a2e764d404d3a97adb7a5e4b0ff`
- Summary: `runs/mistral-20261110-192742-summary.json`
  SHA-256 `99f5243ef149f9517629d4877a8c5adeaf94a31e3fb57551abb66e0e9d281600`
- Large-4 4096 post-hoc: `runs/mistral-large-stability-4096-20261110-194218.jsonl`
  SHA-256 `0788926a65c7af815bbfb2c19d7bebb2214b783978b9a32c58718efe871f6437`

Reproduce:

```sh
python3 benchmark/harness/bench.py --tag mistral \
  --out-dir benchmark/mistral/runs --no-cwd --top-p 1 \
  --delay 2 --rate-limit-delay 20 \
  --models mistral/mistral-large-4 mistral/codestral-latest \
  mistral/mistral-code-latest mistral/ministral-14b-latest \
  mistral/ministral-8b-latest mistral/ministral-3b-latest \
  mistral/voxtral-small-latest
```
