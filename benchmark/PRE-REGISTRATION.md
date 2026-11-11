# PAIG benchmark — pre-registration (2026-11-10)

The initial rules were written before the measured runs. Post-hoc changes are
recorded in "Deviations and post-hoc amendments" at the end; they are not
silently folded into the original rules.

## Scope

- Gateway: the current PAIG binary at `http://127.0.0.1:8899/v1`.
- Transport: non-streaming OpenAI chat completions, `temperature=0`, explicit
  `max_tokens` (2048 correctness, 1024 stability, 2048 post-hoc), no retries.
- No `cwd` field: PAIG Rust ignores it for Command Code, while Bob and Mistral
  reject it with HTTP 422. Sending it would make backends incomparable.
- One request at a time, 1 s after a successful request. A 429 gets a 10 s
  backoff by default. The harness can stop after `--max-consecutive-429 N`; the
  OpenCode free run was a quota-wall probe and completed all 190 requests because
  interleaved 410/400 responses reset the consecutive counter.
- Mistral receives `top_p=1` because its API rejects `temperature=0` greedy
  sampling with the default `top_p`. The Mistral run uses a 2 s cadence and a
  20 s 429 backoff after the first attempt hit `rate_limited`. These are
  post-hoc amendments; see "Deviations and post-hoc amendments".

## Model sets

### Command Code (`cc/`)

- `cc/stealth/glyph-cluster:free` — the only CC model verified with
  `provider-metadata.gateway.cost == 0` on 2026-11-10.
- `cc/stealth/crouton-alpha` is retired: upstream returns 404 "testing period
  ended" and points at `meta/muse-ember`.
- `reflection/beam:free` is absent from the current catalog.
- Paid models (`meta/muse-ember`, `meta/muse-spark-open`, `deepseek/*`) are not
  part of the free-model score. A deepseek probe may be used only as a control
  outside the pre-registered table.

### Bob (`bob/`)

12 aliases, excluding `bob/ultra` because upstream returns 403
("access to the ultra model is forbidden"):

`premium`, `premium-ide`, `premium-shell`, `dev-sec-ops-premium`,
`dev-sec-ops-premium-west`, `fast`, `explorer`, `wxO-model`, `background`,
`security`, `openai/gpt-oss-20b`, `gpt-oss-20b`.

### OpenCode (`opencode/`, `opencode-go/`)

- Free: `beam-free`, `big-pickle`, `crouton-alpha-free`,
  `deepseek-v4.1-flash-free`, `flint-40b-free`, `glm-5.3-flash-free`,
  `mimo-v2.6-flash-free`, `nemotron-3.5-lightning-free`,
  `nemotron-3.5-super-vl-free`, `pulse-alpha-free`.
- Go: `beam-free`, `crouton-alpha-free`, `iceman-contributor-free` (requires
  `OPENCODE_API_KEY`; the 2026-11-05 run only completed `iceman`).

The free bucket was already returning `FreeUsageLimitError` (HTTP 429) during
the probes. The full free matrix is attempted; if it hits the quota wall, the
run is labeled capacity-limited and only the Go models provide complete data.

### Mistral (`mistral/`)

The seven ids configured in `~/.paig/config.json`: `mistral-large-4`,
`codestral-latest`, `mistral-code-latest`, `ministral-14b-latest`,
`ministral-8b-latest`, `ministral-3b-latest`, `voxtral-small-latest`.

### Kilo (`kilo/`)

Skipped: `KILO_API_KEY` is not configured. The catalog advertises eight
`:free` ids, but no request can be authenticated. This is recorded as a skip,
not as a result.

## Correctness suite (9 tasks, one request per model per task)

1. Multi-step arithmetic word problem (integer answer).
2. Modular exponentiation: `7^123 mod 1000`.
3. Python 3 stdout prediction with a mutable default argument.
4. Structured extraction to an exact JSON object.
5. Row-seating logic puzzle (exact name order).
6. SQLite query generation, executed against a fixed in-memory dataset.
7. Indonesian instruction following, returning an exact JSON object.
8. Needle-in-a-haystack retrieval from a ~400-line document.
9. Tool call with exact arguments (`get_exchange_rate`, USD, 2026-03-14).

Answers are graded mechanically: exact integer / normalized string / parsed JSON
/ executed SQL rows / parsed tool-call arguments. A model that emits the right
value in prose but not in the requested `ANSWER:` line or JSON shape fails that
task. Order is rotated per task so no model always runs first.

## Stability probe (10 rounds per model)

Prompt asks for three short bullet points; each round includes a different run
number. Order is rotated per round.

`transport_ok` = HTTP 200.
`usable` = HTTP 200 **and** non-empty `content` **and** `finish_reason == "stop"`.
`format_ok` = at least three bullet/numbered lines (reported separately, not part
of the stability gate).

## Post-hoc reasoning probe

The same modular-exponentiation prompt is repeated 10 times per model,
interleaved and rotated, at 2048 max tokens. This is exploratory and does not
change the pre-registered score.

## Decision rules

- **Stable** if `usable >= 9/10`. A model with `usable <= 8/10` is labeled
  unstable regardless of its correctness score.
- **Best** = highest correctness score. Tie-break: higher `usable`, then lower
  median latency over successful requests.
- Correctness bands: `>= 7/9` candidate for a primary role; `5-6/9` light tasks
  only; `<= 4/9` not recommended.
- If any model has `>= 20%` requests answered with 429/5xx, the run is labeled
  capacity-limited and the ranking is not reported as a quality ranking.
- If any model has `>= 20%` HTTP 200 responses with empty content, the run is
  labeled adapter-suspect. This catches a gateway that drops upstream content
  and returns 200 with `content: null` instead of surfacing the error.
- A model that returns empty content on more than 2/10 stability rounds is
  unstable even if every correctness task passed.

## Claims this benchmark will NOT support

- It does not prove the models stay free; the catalogs can change or disappear.
- Latency is end-to-end through PAIG on this machine. For Command Code, upstream
  injects a ~9.2k-token system prompt (`x-system-prompt-breakdown`); PAIG Rust
  does not inject its own system prompt. This is not a direct-upstream latency
  figure.
- A 9-task suite is a screening test, not a capability ceiling.
- The SQL task executes generated SQL in an in-memory SQLite database with
  progress interruption and forbidden-keyword screening; it does not execute
  arbitrary Python.
- The 2026-11-05 and 2026-11-10 numbers are not directly comparable: the model
  sets, the gateway, and the Command Code stream framing changed.

## Deviations and post-hoc amendments (2026-11-10)

Recorded after the runs; these are not part of the original pre-registered
rules.

1. **Mistral `top_p` and pacing.** The first Mistral attempt sent
   `temperature=0` without `top_p`; `mistral-large-4` returned HTTP 400
   (`top_p must be 1 when using greedy sampling`), and `codestral` /
   `mistral-code` hit `rate_limited`. The harness gained `--top-p`, and the
   Mistral run was repeated with `top_p=1`, a 2 s cadence, and a 20 s 429
   backoff. The 9-record first attempt was removed and is not part of the
   evidence tree; its transcript remains in the session log.
2. **`adapter_suspect` narrowed.** The gate now requires HTTP 200,
   `finish_reason == "stop"`, empty content, and a non-tool-call task. The
   original broader count flagged `mistral-large-4`'s budget-exhaustion
   `finish=length` records as a possible adapter bug; the narrowed gate does
   not.
3. **`capacity_limited` denominator.** The gate uses the actual number of
   records per model, not the planned 19, so a run that stops early can still
   be labeled capacity-limited.
4. **OpenCode free counter.** The free run was intended to stop after 10
   consecutive 429s, but interleaved 410 (`crouton-alpha-free`) and 400
   (`pulse-alpha-free`) responses reset the counter, so it completed all 190
   requests. The result is still a quota wall, not a quality matrix.
5. **`mistral-large-4` 4096 post-hoc.** After observing 0/10 stability at 1024
   tokens, a 3-round probe at 4096 tokens was run. It was usable 3/3 with 3
   bullets. This is exploratory and does not change the pre-registered score.
6. **Command Code free-set scope.** "Only Glyph is free" is based on a
   name-filtered candidate probe plus `provider-metadata.gateway.cost == 0` on
   that model, not a cost sweep of all 97 CC models. `deepseek` reported
   `cost: null`, which means unknown, not free.

## Raw evidence

The harness writes one JSONL record per request (timestamp, model, task, HTTP
status, latency, usage, finish reason, content, grade) plus a SHA-256 of the
JSONL file. The summary is derived only from that file.
