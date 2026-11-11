# PAIG benchmark — 2026-11-10

Screening through the current PAIG gateway. Method and decision rules:
[PRE-REGISTRATION.md](PRE-REGISTRATION.md). Per-backend detail:
[Command Code](commandcode/REPORT.md), [Bob](bob/REPORT.md),
[OpenCode](opencode/REPORT.md), [Mistral](mistral/REPORT.md),
[Kilo](kilo/SKIPPED.md).

## Headline

- **Bob is the only backend with a complete, high-quality grid.**
  `bob/dev-sec-ops-premium` is 9/9 with 10/10 stability; the zero-credit
  `gpt-oss` family is 8/9 with 10/10 stability. `bob/premium` is the weakest at
  6/9.
- **OpenCode Go is usable; OpenCode free is blocked.** `opencode-go/beam-free`
  is 8/9, `iceman` 6/9, both 10/10 stable. All ten `opencode/*` free models hit
  non-200 walls (429 quota, 410 deprecated, 400 unavailable), so there is no
  free-model quality result.
- **Command Code’s free catalog shrank to one verified model.** Glyph Cluster is
  7/9 and 10/10 stable; Crouton Alpha is retired (404), Beam is gone. Glyph is
  the only candidate verified at `cost == 0`; this was a name-filtered probe,
  not a cost sweep of all 97 CC models. Glyph also passed the 10× modpow
  post-hoc 10/10.
- **Mistral is correctness-limited and rate-limited.** `mistral-large-4` is
  8/9; its 0/10 stability at 1024 tokens is a reasoning-budget limit (3/3 at
  4096). `voxtral-small` is 5/9; the small `ministral` models are 4/9. Two code
  models hit 429 and are capacity-limited.
- **Kilo was not measured:** no `KILO_API_KEY`.

## Cross-backend snapshot

| backend | model | suite | stability | note |
| --- | --- | ---: | ---: | --- |
| Bob | `bob/dev-sec-ops-premium` | **9/9** | 10/10 | best complete result |
| Bob | `bob/gpt-oss-20b` | 8/9 | 10/10 | zero credits |
| Bob | `bob/premium` | 6/9 | 10/10 | weakest Bob alias |
| OpenCode Go | `opencode-go/beam-free` | 8/9 | 10/10 | complete |
| OpenCode Go | `opencode-go/iceman-contributor-free` | 6/9 | 10/10 | complete |
| Command Code | `cc/stealth/glyph-cluster:free` | 7/9 | 10/10 | only verified cost-0 CC model |
| Mistral | `mistral/mistral-large-4` | 8/9 | 0/10* | *4096 post-hoc 3/3 |
| Mistral | `mistral/voxtral-small-latest` | 5/9 | 10/10 | complete |
| OpenCode free | all ten | 0 graded | n/a | 429/410/400 wall |
| Kilo | — | — | — | skipped, no key |

## What changed vs 2026-11-05

- The CC free set changed from three models to one. Crouton Alpha is retired and
  points at `meta/muse-ember`; Beam is absent. Only Glyph reports
  `provider-metadata.gateway.cost == 0`.
- Bob still rejects `cwd` with 422; `bob/ultra` still returns 403. The measured
  set remains the 12 allowed aliases.
- OpenCode free quota was already exhausted; the key unlocks `opencode-go/*`
  but not the free bucket. `crouton-alpha-free` is deprecated on the Go route.
- Mistral needs `top_p=1` with `temperature=0` and rejects `cwd`.

## Caveats

- These are screening runs, not general model rankings. One sample per task per
  model; 10 stability rounds.
- The 2026-11-05 and 2026-11-10 numbers are not directly comparable: model
  catalogs, gateway implementation, and Command Code framing changed.
- Mistral’s codestral/mistral-code results are capacity-limited by 429s.
- Bob’s zero-credit gpt-oss family and the paid aliases can change without
  notice; this is a 2026-11-10 snapshot.
- `cwd` was omitted for every new run (Bob/Mistral reject it; CC ignores it).
- No retries; a 429/5xx is recorded as-is.
