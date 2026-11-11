# Bob aliases — measured screening (2026-11-10)

**Status: complete.** 12 aliases × (9 correctness + 10 stability) = 228
measured requests, all HTTP 200, no bridge/upstream errors.

`bob/ultra` is excluded: upstream returns 403
("access to the ultra model is forbidden"), same as the old `fake-ultra` probe.
Bob also still rejects the `cwd` field with HTTP 422, so every request was sent
with `--no-cwd`.

## Result

**Best overall: `bob/dev-sec-ops-premium`** — 9/9 suite, 10/10 stability, tool
call PASS, suite median 1.724 s. `bob/dev-sec-ops-premium-west` has the same
9/9 matrix and is slightly slower.

**Best value: the `gpt-oss` family** (`background`, `security`,
`openai/gpt-oss-20b`, `gpt-oss-20b`) — 8/9, 10/10 stability, **0 credits** in
this run. The only failure is the Python mutable-default trap.

**Fastest: `bob/explorer` and `bob/fast`** — 1.27–1.74 s suite median, 7/9,
about 0.0126 credits each.

**Worst: `bob/premium`** — 6/9; it failed modular exponentiation, Python stdout,
and Indonesian JSON.

| alias | suite | stability | format | suite median | suite p90 | tokens med | credits |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `bob/dev-sec-ops-premium` | **9/9** | 10/10 | 10/10 | 1.724 s | 2.792 s | 112 | 0.043662 |
| `bob/dev-sec-ops-premium-west` | **9/9** | 10/10 | 10/10 | 2.626 s | 5.755 s | 131 | 0.043902 |
| `bob/premium-ide` | 8/9 | 10/10 | 10/10 | 2.174 s | 4.202 s | 78 | 0.030512 |
| `bob/premium-shell` | 8/9 | 10/10 | 10/10 | 2.214 s | 4.239 s | 78 | 0.030388 |
| `bob/wxO-model` | 8/9 | 10/10 | 10/10 | 2.262 s | 3.637 s | 78 | 0.029950 |
| `bob/background` | 8/9 | 10/10 | 10/10 | 2.354 s | 6.705 s | 229 | 0 |
| `bob/security` | 8/9 | 10/10 | 10/10 | 1.863 s | 3.638 s | 229 | 0 |
| `bob/openai/gpt-oss-20b` | 8/9 | 10/10 | 10/10 | 2.228 s | 3.795 s | 226 | 0 |
| `bob/gpt-oss-20b` | 8/9 | 10/10 | 10/10 | 1.794 s | 3.582 s | 162 | 0 |
| `bob/fast` | 7/9 | 10/10 | 10/10 | 1.743 s | 2.265 s | 124 | 0.012614 |
| `bob/explorer` | 7/9 | 10/10 | 10/10 | 1.266 s | 2.174 s | 124 | 0.012693 |
| `bob/premium` | 6/9 | 10/10 | 10/10 | 5.264 s | 7.532 s | 126 | 0.030652 |

## Task matrix

| task | premium | premium-ide | premium-shell | wxO | dev-sec-ops | dev-sec-ops-west | fast | explorer | background | security | openai/gpt-oss | gpt-oss |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| math | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| modpow | **FAIL** | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| python-output | **FAIL** | PASS | PASS | PASS | PASS | PASS | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** | **FAIL** |
| json-extract | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| logic | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| sql | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| indonesian-json | **FAIL** | **FAIL** | **FAIL** | **FAIL** | PASS | PASS | **FAIL** | **FAIL** | PASS | PASS | PASS | PASS |
| needle | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| tool-call | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |

## Behavior groups

The measured matrix has five groups, matching the 2026-11-05 run:

1. `bob/premium` — 6/9 (modpow + Python + Indonesian).
2. `bob/premium-ide`, `bob/premium-shell`, `bob/wxO-model` — 8/9
   (Indonesian only).
3. `bob/dev-sec-ops-premium`, `bob/dev-sec-ops-premium-west` — 9/9.
4. `bob/fast`, `bob/explorer` — 7/9 (Python + Indonesian).
5. `bob/background`, `bob/security`, `bob/openai/gpt-oss-20b`,
   `bob/gpt-oss-20b` — 8/9 (Python only; zero credits).

Within a group the task matrices and stability results are identical. That is
evidence they behave like the same backend/role, not proof of the underlying
vendor model.

## Budget

The JSONL `usage.credits` sums to **0.234373 credits** for 228 requests. The
zero-credit gpt-oss family accounts for a large share of the run; the
dev-sec-ops pair is the most expensive (~0.044 each). No overage or 429 was
observed.

## Caveats

- One sample per alias per task. A 9/9 or 8/9 is a screening result, not a
  guarantee.
- Aliases may share a backend; identical matrices are grouped, not treated as
  independent models without evidence.
- The trial plan can change or stop; this is a 2026-11-10 snapshot.
- The needle is ~8k prompt tokens, not a long-context stress test.
- The tool-call task is one function call per alias.
- All benchmark traffic went through PAIG's `bob/` backend; no direct Bob
  upstream request was used.
- `cwd` must be stripped for Bob. A request with `cwd` still gets HTTP 422
  (`unexpected property body.cwd`).

## Raw evidence

- Measured JSONL: `runs/bob-20261110-191108.jsonl` (228 records)
  SHA-256 `e15acba9f16e029eb41d8d925305a284249f646962e0831a7999aae84a83237a`
- Summary: `runs/bob-20261110-191108-summary.json`
  SHA-256 `5e1258c61cf6495c59972db22828fc35d84f73ae1284d71b228ed9a04b7bb563`
- Analysis: `runs/bob-analysis.json`
  SHA-256 `c28cce61bdf16ea3ca7c6be6f45290c8194ea96a99a69fe631c57f25f87ddb1e`

Reproduce:

```sh
python3 benchmark/harness/bench.py --tag bob \
  --out-dir benchmark/bob/runs --no-cwd \
  --models bob/premium bob/premium-ide bob/premium-shell \
  bob/dev-sec-ops-premium bob/dev-sec-ops-premium-west bob/fast bob/explorer \
  bob/wxO-model bob/background bob/security bob/openai/gpt-oss-20b bob/gpt-oss-20b
python3 benchmark/harness/analyze_bob.py \
  benchmark/bob/runs/bob-20261110-191108.jsonl \
  --out benchmark/bob/runs/bob-analysis.json
```
