# Kilo — skipped (no credential)

> **2026-11-14 update:** Kilo's `:free` ids are served keyless; PAIG exposes
> them as `kilo-free/<id>`. The 2026-11-10 run below used the keyed harness and
> remains a skip. A keyless sweep over `kilo-free/...` has not been run, and the
> ids below are a historical snapshot, not today's catalogue.

**Status: not measured.** `KILO_API_KEY` is not set, `kilo.apiKey` is empty in
`~/.paig/config.json`, and no keyed Kilo request can be authenticated.

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

To run it later, either use the keyless free prefix or provide a key for the
keyed prefix:

```sh
./build/paig   # keyless
python3 benchmark/harness/bench.py --tag kilo \
  --out-dir benchmark/kilo/runs \
  --models kilo-free/nvidia/nemotron-3.5-super-vl:free ... --no-cwd

KILO_API_KEY=... ./build/paig
python3 benchmark/harness/bench.py --tag kilo \
  --out-dir benchmark/kilo/runs \
  --models kilo/nvidia/nemotron-3.5-super-vl:free ... --no-cwd
```
