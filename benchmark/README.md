# PAIG benchmark (2026-11-10)

Model screening through the current PAIG gateway. Methodology and decision
rules: [PRE-REGISTRATION.md](PRE-REGISTRATION.md).

## Layout

```
benchmark/
├── PRE-REGISTRATION.md
├── harness/
│   ├── bench.py          # 9-task suite + 10 stability rounds
│   └── posthoc.py        # optional 10x modpow stability probe
├── commandcode/runs/     # cc/* (currently one free model)
├── bob/runs/             # 12 aliases
├── opencode/runs/        # free + Go models
├── mistral/runs/         # 7 configured models
└── kilo/                 # skipped: no KILO_API_KEY
```

## Run

Build and start PAIG first, then run the harness against `127.0.0.1:8899`:

```sh
./build.sh
./build/paig                     # or ./build/paig --start

python3 benchmark/harness/bench.py \
  --tag cc-glyph \
  --out-dir benchmark/commandcode/runs \
  --models cc/stealth/glyph-cluster:free \
  --no-cwd
```

Bob and Mistral reject the `cwd` field, so pass `--no-cwd` for them. OpenCode
free hits a shared quota; `--max-consecutive-429 N` can stop a pure-429 run,
but interleaved 410/400 responses reset that counter.

Each run writes `<tag>-<timestamp>.jsonl` and
`<tag>-<timestamp>-summary.json`. The summary includes the JSONL SHA-256.

## Results

Cross-backend snapshot: [SUMMARY.md](SUMMARY.md). Per-backend detail:
[Command Code](commandcode/REPORT.md), [Bob](bob/REPORT.md),
[OpenCode](opencode/REPORT.md), [Mistral](mistral/REPORT.md),
[Kilo](kilo/SKIPPED.md).

Mistral needs `--top-p 1` with `temperature=0`; the harness exposes that flag.
The harness flags two degraded modes:

- `capacity_limited`: at least 20% of requests are 429/5xx.
- `adapter_suspect`: at least 20% of responses are HTTP 200 with empty content
  (a gateway that hides upstream errors as empty replies).

Kilo is skipped because `KILO_API_KEY` is absent; no Kilo request was measured.
