# Command Code free model — Glyph Cluster (2026-11-10)

**Status: complete, but the free catalog has shrunk to one model.** The
2026-11-05 benchmark measured three free Command Code models. On 2026-11-10,
`stealth/crouton-alpha` returns 404 ("testing period ended", replaced by
`meta/muse-ember`), `reflection/beam:free` is absent from the catalog, and only
`stealth/glyph-cluster:free` still reports `provider-metadata.gateway.cost == 0`.

This is therefore a single-model screening result, not a ranking.

## Result

`cc/stealth/glyph-cluster:free` passed **7/9** correctness tasks, was usable
**10/10** stability rounds with 3 bullets in all 10, and answered the post-hoc
modular-exponentiation probe **10/10 correct**.

| metric | value |
| --- | ---: |
| correctness | **7/9** |
| stability (usable) | **10/10** |
| 3-bullet format | 10/10 |
| tool call | PASS |
| suite median latency | 3.856 s |
| stability median / p90 | 6.829 s / 7.980 s |
| post-hoc modpow usable / correct | 10/10 / 10/10 |
| post-hoc median latency | 9.454 s |
| HTTP errors | none |

## Task matrix

| task | result | note |
| --- | --- | --- |
| math | PASS | `ANSWER: 882` |
| modpow | PASS | `ANSWER: 343` |
| python-output | **FAIL** | mutable-default trap: `[1] [1, 2] [1, 2, 3]` |
| json-extract | PASS | exact object |
| logic | PASS | Dan, Ana, Ben, Cia |
| sql | PASS | rows `[[12,130],[10,110],[13,100]]` |
| indonesian-json | **FAIL** | counted 3 `a` letters in "analisis" instead of 2 |
| needle | PASS | `Q7X-4291` |
| tool-call | PASS | `get_exchange_rate(USD, 2026-03-14)` |

The single empty `content` in the summary is the tool-call task, which returned
`tool_calls` and no prose. It is not an empty answer.

## Required gateway fix

Command Code now streams bare NDJSON objects
(`{"type":"text-delta","id":"txt-0","text":"OK"}`) instead of SSE
`data: {...}` lines. Before this run, PAIG's CC adapter skipped every non-`data:`
line, so all CC models returned HTTP 200 with `content: null`, and upstream
errors were hidden as empty answers. The parser now accepts both framings in
`src/backend/cc.rs` (`cc_stream_event`), with a unit test. This benchmark was run
against the patched binary.

Verification after the patch:

- `cc/stealth/glyph-cluster:free` returns `"OK"` for a short prompt.
- `cc/stealth/crouton-alpha` surfaces upstream `404` with the retirement message
  instead of a fake 200/null.

## Caveats

- One free model is a snapshot, not a comparison. The 7/9 is a screening result.
- Latency includes Command Code's upstream ~9.2k-token system prompt
  (`x-system-prompt-breakdown: {"systemPrompt":9247}`), not a PAIG-injected
  prompt. It is not comparable to the 2026-11-05 numbers.
- `max_tokens` 2048 correctness / 1024 stability, temperature 0, no retries.
- The Python mutable-default trap remains the shared weakness; the Indonesian
  counting task is the second failure.

## Raw evidence

- Main run: `runs/cc-glyph-20261110-190643.jsonl` (19 records)
  SHA-256 `7953e71e4edccda7552b48e26d6b6e26a90743d8d5caa52bcc9f87068a4f38c3`
- Summary: `runs/cc-glyph-20261110-190643-summary.json`
- Post-hoc: `runs/cc-glyph-posthoc-20261110-190901.jsonl` (10 records)
  SHA-256 `b9893274a6a4b0a2f156ba3c7c70f6faa51a7d4ea55f2a6d53ee2e5125bb4310`

Reproduce:

```sh
python3 benchmark/harness/bench.py --tag cc-glyph \
  --out-dir benchmark/commandcode/runs \
  --models cc/stealth/glyph-cluster:free --no-cwd
python3 benchmark/harness/posthoc.py --tag cc-glyph-posthoc \
  --out-dir benchmark/commandcode/runs \
  --models cc/stealth/glyph-cluster:free --rounds 10 --no-cwd
```
