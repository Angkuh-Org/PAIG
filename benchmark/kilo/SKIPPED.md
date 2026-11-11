# Kilo — skipped (no credential)

**Status: not measured.** `KILO_API_KEY` is not set, `kilo.apiKey` is empty in
`~/.paig/config.json`, and no Kilo request can be authenticated.

The current PAIG catalog still advertises eight `:free` ids:

- `kilo/nvidia/nemotron-3.5-super-vl:free`
- `kilo/thinkingmachines/inkling-2-small:free`
- `kilo/liquid/lfm-3-1.7b:free`
- `kilo/dots-studio/dots-3-note-preview:free`
- `kilo/nvidia/nemotron-3.5-lightning:free`
- `kilo/cohere/north-mini-code:free`
- `kilo/nvidia/nemotron-3-ultra-550b-a55b:free`
- `kilo/nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free`

They are listed here only to record the catalog snapshot. No correctness,
stability, or latency number was produced, so this is a skip, not a result.

To run it later, provide a key and use the same harness:

```sh
KILO_API_KEY=... ./build/paig
python3 benchmark/harness/bench.py --tag kilo \
  --out-dir benchmark/kilo/runs \
  --models kilo/nvidia/nemotron-3.5-super-vl:free ... --no-cwd
```
