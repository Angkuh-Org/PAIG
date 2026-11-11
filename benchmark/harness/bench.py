#!/usr/bin/env python3
"""Benchmark model backends through the current PAIG Rust gateway.

Read ../PRE-REGISTRATION.md before running. The harness deliberately does not
retry: a retry would hide exactly the instability the run is meant to measure.

Model ids are sent verbatim, so pass the full PAIG Rust id (`cc/...`, `bob/...`,
`opencode/...`, `opencode-go/...`, `mistral/...`, `kilo/...`).

Usage:
  python3 benchmark/harness/bench.py --tag cc-glyph --models cc/stealth/glyph-cluster:free
  python3 benchmark/harness/bench.py --tag bob --models bob/fast bob/premium --no-cwd
  python3 benchmark/harness/bench.py --tag opencode --models opencode/beam-free --max-consecutive-429 5
"""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import sqlite3
import statistics
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("PAIG_CHAT_URL", "http://127.0.0.1:8899/v1/chat/completions")
CWD = "/tmp/paig-bench-empty"
MODELS: list[str] = []
STABILITY_ROUNDS = 10
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")


def rotated(items: list[str], index: int) -> list[str]:
    if not items:
        return []
    k = index % len(items)
    return items[k:] + items[:k]


def post_chat(body: dict, timeout: float = 240.0) -> dict:
    data = json.dumps(body).encode("utf-8")
    request = urllib.request.Request(
        BASE, data=data, headers={"content-type": "application/json"}
    )
    started = time.monotonic()
    try:
        with OPENER.open(request, timeout=timeout) as response:
            raw = response.read().decode("utf-8", "replace")
            return {
                "status": response.status,
                "raw": raw,
                "latency_s": time.monotonic() - started,
                "error": None,
            }
    except urllib.error.HTTPError as error:
        raw = error.read().decode("utf-8", "replace")
        return {
            "status": error.code,
            "raw": raw,
            "latency_s": time.monotonic() - started,
            "error": raw[:2000],
        }
    except Exception as error:  # noqa: BLE001 - the record is the evidence
        return {
            "status": None,
            "raw": "",
            "latency_s": time.monotonic() - started,
            "error": f"{type(error).__name__}: {error}",
        }


def parse_payload(result: dict) -> dict | None:
    if not result["raw"]:
        return None
    try:
        payload = json.loads(result["raw"])
    except json.JSONDecodeError:
        return None
    return payload if isinstance(payload, dict) else None


def message_of(payload: dict | None) -> dict:
    if not payload:
        return {}
    try:
        message = payload["choices"][0]["message"]
    except (KeyError, IndexError, TypeError):
        return {}
    return message if isinstance(message, dict) else {}


def finish_reason_of(payload: dict | None) -> str | None:
    if not payload:
        return None
    try:
        return payload["choices"][0].get("finish_reason")
    except (KeyError, IndexError, TypeError):
        return None


def usage_of(payload: dict | None) -> dict:
    if not payload:
        return {}
    usage = payload.get("usage")
    return usage if isinstance(usage, dict) else {}


def answer_line(text: str) -> str | None:
    matches = list(
        re.finditer(r"^\s*ANSWER:\s*(.+?)\s*$", text or "", re.MULTILINE | re.IGNORECASE)
    )
    if not matches:
        return None
    value = matches[-1].group(1).strip().strip("`").strip()
    return value or None


def normalize_text(text: str) -> str:
    value = (text or "").strip().strip("`").strip()
    value = re.sub(r"<[^>]+>", "", value)
    value = value.replace("−", "-").replace("–", "-").replace("—", "-")
    value = re.sub(r"\s+", " ", value)
    return value.strip(" .")


def extract_json(text: str) -> object | None:
    value = (text or "").strip()
    value = re.sub(r"^```(?:json)?\s*", "", value, flags=re.IGNORECASE)
    value = re.sub(r"\s*```$", "", value)
    decoder = json.JSONDecoder()
    for index, char in enumerate(value):
        if char != "{":
            continue
        try:
            parsed, _ = decoder.raw_decode(value[index:])
        except json.JSONDecodeError:
            continue
        if isinstance(parsed, dict):
            return parsed
    return None


def extract_sql(text: str) -> str | None:
    fenced = re.search(r"```(?:sql)?\s*(.*?)```", text or "", re.DOTALL | re.IGNORECASE)
    candidate = fenced.group(1) if fenced else (text or "")
    match = re.search(r"\b(?:select|with)\b", candidate, re.IGNORECASE)
    if not match:
        return None
    return candidate[match.start():].strip()


def grade_answer_int(expected: int, text: str) -> tuple[bool, str]:
    line = answer_line(text)
    if line is None:
        return False, "no ANSWER line"
    numbers = [int(value) for value in re.findall(r"-?\d+", line.replace(",", ""))]
    if expected in numbers:
        return True, line
    return False, f"expected {expected}, got {line!r}"


def grade_python_output(expected: str, text: str) -> tuple[bool, str]:
    line = answer_line(text)
    if line is None:
        return False, "no ANSWER line"
    if normalize_text(line).lower() == expected.lower():
        return True, line
    return False, f"expected {expected!r}, got {line!r}"


def grade_logic(text: str) -> tuple[bool, str]:
    line = answer_line(text)
    if line is None:
        return False, "no ANSWER line"
    names = re.findall(r"\b(dan|ana|ben|cia)\b", line.lower())
    if names[:4] == ["dan", "ana", "ben", "cia"]:
        return True, line
    return False, f"expected Dan, Ana, Ben, Cia; got {line!r}"


def grade_json(expected: dict, text: str) -> tuple[bool, str]:
    parsed = extract_json(text)
    if parsed is None:
        return False, "no valid JSON object"
    if parsed == expected:
        return True, json.dumps(parsed, sort_keys=True)
    return False, f"expected {json.dumps(expected, sort_keys=True)}, got {json.dumps(parsed, sort_keys=True)}"


def grade_sql(text: str) -> tuple[bool, str]:
    query = extract_sql(text)
    if not query:
        return False, "no SQL query"
    lowered = query.lower()
    if re.search(r"\b(attach|detach|pragma|vacuum|insert|update|delete|drop|alter|create|load_extension|readfile|writefile)\b", lowered):
        return False, "forbidden SQL keyword"
    if re.search(r"\breplace\s+into\b", lowered):
        return False, "forbidden SQL keyword"
    if not re.match(r"^\s*(select|with)\b", lowered):
        return False, "not a SELECT/WITH query"

    connection = sqlite3.connect(":memory:")
    try:
        connection.executescript(
            """
            CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                user_id INTEGER,
                kind TEXT,
                amount INTEGER,
                created_at TEXT
            );
            INSERT INTO events VALUES
                (1, 10, 'purchase', 60, '2026-01-05'),
                (2, 10, 'purchase', 50, '2026-02-11'),
                (3, 11, 'purchase', 200, '2025-12-31'),
                (4, 11, 'refund',   30, '2026-03-01'),
                (5, 12, 'purchase', 120, '2026-03-03'),
                (6, 12, 'purchase', 10, '2026-04-01'),
                (7, 13, 'purchase', 99, '2026-05-05'),
                (8, 13, 'purchase', 1, '2026-06-06');
            """
        )
        ticks = {"count": 0}

        def progress() -> int:
            ticks["count"] += 1
            return 1 if ticks["count"] > 3000 else 0

        connection.set_progress_handler(progress, 1000)
        cursor = connection.execute(query)
        rows = [list(row) for row in cursor.fetchall()]
    except Exception as error:  # noqa: BLE001 - model output is untrusted
        return False, f"sqlite error: {type(error).__name__}: {error}"
    finally:
        connection.close()

    expected = [[12, 130], [10, 110], [13, 100]]
    if rows == expected:
        return True, json.dumps(rows)
    return False, f"expected {expected}, got {rows}"


def grade_tool(payload: dict | None) -> tuple[bool, str]:
    message = message_of(payload)
    calls = message.get("tool_calls")
    if not isinstance(calls, list) or not calls:
        content = (message.get("content") or "").strip()
        return False, f"no tool_calls (content={content[:120]!r})"
    first = calls[0] if isinstance(calls[0], dict) else {}
    function = first.get("function") if isinstance(first.get("function"), dict) else {}
    name = function.get("name")
    arguments = function.get("arguments")
    if name != "get_exchange_rate":
        return False, f"wrong function {name!r}"
    try:
        parsed = json.loads(arguments) if isinstance(arguments, str) else arguments
    except json.JSONDecodeError:
        return False, f"arguments are not JSON: {arguments!r}"
    if not isinstance(parsed, dict):
        return False, f"arguments are not an object: {parsed!r}"
    currency = str(parsed.get("currency", "")).upper()
    date = str(parsed.get("date", ""))
    if currency == "USD" and date == "2026-03-14":
        return True, json.dumps(parsed, sort_keys=True)
    return False, f"wrong arguments: {json.dumps(parsed, sort_keys=True)}"


def needle_document() -> str:
    lines = []
    for index in range(1, 401):
        if index == 217:
            lines.append(f"Baris {index}: KODE RAHASIA: Q7X-4291.")
        else:
            lines.append(f"Baris {index}: laporan operasional rutin, tidak ada keputusan penting di sini.")
    return "\n".join(lines)


TOOL = {
    "type": "function",
    "function": {
        "name": "get_exchange_rate",
        "description": "Get the exchange rate for a currency on a specific date.",
        "parameters": {
            "type": "object",
            "properties": {
                "currency": {"type": "string", "description": "ISO 4217 currency code"},
                "date": {"type": "string", "description": "Date in YYYY-MM-DD format"},
            },
            "required": ["currency", "date"],
        },
    },
}

TASKS = [
    {
        "id": "math",
        "kind": "answer_int",
        "expected": 882,
        "max_tokens": 2048,
        "prompt": (
            "A workshop has 7 machines. Each machine normally produces 18 widgets per hour. "
            "3 of the machines were upgraded and now produce 25 widgets per hour; the other 4 "
            "still produce 18. How many widgets do all 7 machines produce in 6 hours?\n"
            "End with exactly one line: ANSWER: <number>"
        ),
    },
    {
        "id": "modpow",
        "kind": "answer_int",
        "expected": 343,
        "max_tokens": 2048,
        "prompt": (
            "Compute 7^123 mod 1000.\n"
            "End with exactly one line: ANSWER: <number>"
        ),
    },
    {
        "id": "python-output",
        "kind": "python_output",
        "expected": "[1, 2, 3] [1, 2, 3] [1, 2, 3]",
        "max_tokens": 2048,
        "prompt": (
            "What is the exact stdout of this Python 3 program?\n\n"
            "def f(x, acc=[]):\n"
            "    acc.append(x)\n"
            "    return acc\n\n"
            "print(f(1), f(2), f(3))\n\n"
            "End with exactly one line: ANSWER: <stdout>"
        ),
    },
    {
        "id": "json-extract",
        "kind": "json",
        "expected": {
            "invoice": "INV-2043",
            "payer": "Naya",
            "date": "2026-03-14",
            "amount_idr": 1250000,
            "account": "acct_88",
        },
        "max_tokens": 1024,
        "prompt": (
            "Extract the fields from the text below and return one JSON object with exactly "
            "these keys: invoice, payer, date, amount_idr, account. amount_idr must be an "
            "integer. Output only the JSON object, no prose.\n\n"
            "Text: Invoice INV-2043 was paid by Naya on 2026-03-14 for IDR 1,250,000. "
            "The account is acct_88."
        ),
    },
    {
        "id": "logic",
        "kind": "logic",
        "max_tokens": 2048,
        "prompt": (
            "Four people (Ana, Ben, Cia, Dan) sit in one row. Ana is not at either end. "
            "Ben sits immediately to the left of Cia. Dan is at the left end. "
            "What is the order from left to right?\n"
            "End with exactly one line: ANSWER: <name>, <name>, <name>, <name>"
        ),
    },
    {
        "id": "sql",
        "kind": "sql",
        "max_tokens": 2048,
        "prompt": (
            "Using SQLite, write one SELECT query over this table:\n\n"
            "CREATE TABLE events (id INTEGER PRIMARY KEY, user_id INTEGER, kind TEXT, "
            "amount INTEGER, created_at TEXT);\n"
            "-- rows: (1,10,'purchase',60,'2026-01-05'), (2,10,'purchase',50,'2026-02-11'), "
            "(3,11,'purchase',200,'2025-12-31'), (4,11,'refund',30,'2026-03-01'), "
            "(5,12,'purchase',120,'2026-03-03'), (6,12,'purchase',10,'2026-04-01'), "
            "(7,13,'purchase',99,'2026-05-05'), (8,13,'purchase',1,'2026-06-06')\n\n"
            "Return user_id and the total purchase amount as total, for kind='purchase' in "
            "year 2026 only, keeping only users whose total is at least 100, ordered by "
            "total descending then user_id ascending. Output only one SQL code block, no prose."
        ),
    },
    {
        "id": "indonesian-json",
        "kind": "json",
        "expected": {"bahasa": "Indonesia", "kode": 2},
        "max_tokens": 1024,
        "prompt": (
            "Jawab dalam bahasa Indonesia. Kembalikan hanya satu objek JSON valid tanpa teks "
            'lain dengan format persis: {"bahasa":"Indonesia","kode":<jumlah huruf a pada kata "analisis">}'
        ),
    },
    {
        "id": "needle",
        "kind": "answer_text",
        "expected": "Q7X-4291",
        "max_tokens": 1024,
        "prompt": (
            "Read the document below and answer the question.\n\n"
            "<document>\n" + needle_document() + "\n</document>\n\n"
            "What is the secret code? End with exactly one line: ANSWER: <code>"
        ),
    },
    {
        "id": "tool-call",
        "kind": "tool",
        "max_tokens": 1024,
        "tools": [TOOL],
        "prompt": (
            "Use the get_exchange_rate tool to fetch the USD exchange rate for 2026-03-14. "
            "Do not write a normal answer; call the tool."
        ),
    },
]


def grade(task: dict, payload: dict | None) -> tuple[bool, str]:
    message = message_of(payload)
    content = message.get("content") or ""
    kind = task["kind"]
    if kind == "answer_int":
        return grade_answer_int(task["expected"], content)
    if kind == "python_output":
        return grade_python_output(task["expected"], content)
    if kind == "answer_text":
        line = answer_line(content)
        if line is None:
            return False, "no ANSWER line"
        if normalize_text(line).lower() == task["expected"].lower():
            return True, line
        return False, f"expected {task['expected']!r}, got {line!r}"
    if kind == "logic":
        return grade_logic(content)
    if kind == "json":
        return grade_json(task["expected"], content)
    if kind == "sql":
        return grade_sql(content)
    if kind == "tool":
        return grade_tool(payload)
    return False, f"unknown task kind {kind!r}"


def make_record(phase: str, model: str, task: dict, result: dict, payload: dict | None,
                passed: bool | None, detail: str, extra: dict | None = None) -> dict:
    message = message_of(payload)
    content = message.get("content") or ""
    reasoning = message.get("reasoning_content") or ""
    record = {
        "ts": now_iso(),
        "phase": phase,
        "model": model,
        "task": task["id"],
        "status": result["status"],
        "latency_s": round(result["latency_s"], 3),
        "finish_reason": finish_reason_of(payload),
        "usage": usage_of(payload),
        "content": content,
        "reasoning_chars": len(reasoning),
        "passed": passed,
        "detail": detail,
        "error": result["error"],
    }
    if extra:
        record.update(extra)
    return record


def polite_sleep(result: dict, delay: float = 1.0, rate_limit_delay: float = 10.0) -> None:
    status = result["status"]
    if status == 429:
        time.sleep(rate_limit_delay)
    elif status is not None and 500 <= status < 600:
        time.sleep(3.0)
    elif status is None:
        time.sleep(2.0)
    else:
        time.sleep(delay)


def format_bullets(content: str) -> int:
    return len(re.findall(r"^\s*(?:[-*•]|\d+[.)])\s+\S", content or "", re.MULTILINE))


def summarize(records: list[dict], models: list[str], results_path: str) -> dict:
    summary: dict = {
        "results_file": results_path,
        "results_sha256": sha256_file(results_path),
        "generated_at": now_iso(),
        "models": {},
    }
    for model in models:
        correctness = [r for r in records if r["phase"] == "correctness" and r["model"] == model]
        stability = [r for r in records if r["phase"] == "stability" and r["model"] == model]
        passed = [r for r in correctness if r["passed"]]
        usable = [r for r in stability if r["passed"]]
        successes = [r for r in correctness if r["status"] == 200]
        latencies = sorted(r["latency_s"] for r in successes)
        stable_latencies = sorted(r["latency_s"] for r in usable)

        def percentile(values: list[float], fraction: float) -> float | None:
            if not values:
                return None
            index = min(len(values) - 1, max(0, int(round(fraction * (len(values) - 1)))))
            return round(values[index], 3)

        summary["models"][model] = {
            "correctness": {
                "passed": len(passed),
                "total": len(correctness),
                "graded": sum(1 for r in correctness if r["status"] == 200),
                "score": f"{len(passed)}/{len(correctness)}",
                "tasks": {r["task"]: bool(r["passed"]) for r in correctness},
                "latency_median_s": round(statistics.median(latencies), 3) if latencies else None,
            },
            "stability": {
                "usable": len(usable),
                "total": len(stability),
                "ok": f"{len(usable)}/{len(stability)}",
                "format_3_bullets": sum(1 for r in stability if format_bullets(r["content"]) >= 3),
                "latency_median_s": round(statistics.median(stable_latencies), 3) if stable_latencies else None,
                "latency_p90_s": percentile(stable_latencies, 0.9),
                "latency_max_s": max(stable_latencies) if stable_latencies else None,
            },
            "errors": {
                str(status): sum(1 for r in correctness + stability if r["status"] == status)
                for status in {r["status"] for r in correctness + stability}
                if status != 200
            },
            "empty_content": sum(
                1 for r in correctness + stability
                if r["status"] == 200 and not (r["content"] or "").strip()
            ),
            "null_content_200": sum(
                1 for r in correctness + stability
                if r["status"] == 200
                and not (r["content"] or "").strip()
                and r.get("finish_reason") == "stop"
                and r["task"] != "tool-call"
            ),
            "tool_call": next(
                (r["passed"] for r in correctness if r["task"] == "tool-call"), None
            ),
        }

    def rank_key(model: str) -> tuple:
        item = summary["models"][model]
        score = item["correctness"]["passed"]
        usable = item["stability"]["usable"]
        latency = item["correctness"]["latency_median_s"] or 1e9
        return (-score, -usable, latency)

    ranked = [
        model for model in models
        if summary["models"][model]["correctness"]["graded"] > 0
    ]
    summary["ranking"] = sorted(ranked, key=rank_key)
    best = summary["ranking"][0] if summary["ranking"] else None
    summary["verdict"] = {
        "best": best,
        "stable_models": [
            model for model in models
            if summary["models"][model]["stability"]["usable"] >= STABILITY_ROUNDS - 1
        ],
        "capacity_limited": any(
            summary["models"][model]["errors"].get("429", 0)
            + sum(v for k, v in summary["models"][model]["errors"].items() if k.startswith("5"))
            >= 0.2 * max(
                1,
                summary["models"][model]["correctness"]["total"]
                + summary["models"][model]["stability"]["total"],
            )
            for model in models
        ),
        "adapter_suspect": any(
            summary["models"][model]["null_content_200"]
            >= 0.2 * max(
                1,
                summary["models"][model]["correctness"]["total"]
                + summary["models"][model]["stability"]["total"],
            )
            for model in models
        ),
    }
    return summary


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def print_summary(summary: dict, models: list[str]) -> None:
    print("\n== Summary ==")
    print("| model | suite | stable (usable) | 3-bullet format | median lat (s) | p90 lat (s) | errors |")
    print("| --- | --- | --- | --- | --- | --- | --- |")
    for model in models:
        item = summary["models"][model]
        errors = ", ".join(f"{k}:{v}" for k, v in sorted(item["errors"].items())) or "-"
        print(
            f"| {model} | {item['correctness']['score']} | "
            f"{item['stability']['ok']} | "
            f"{item['stability']['format_3_bullets']}/{item['stability']['total']} | "
            f"{item['correctness']['latency_median_s']} | "
            f"{item['stability']['latency_p90_s']} | {errors} |"
        )

    print("\n| task | " + " | ".join(models) + " |")
    print("| --- | " + " | ".join("---" for _ in models) + " |")
    for task in TASKS:
        cells = []
        for model in models:
            value = summary["models"][model]["correctness"]["tasks"].get(task["id"])
            cells.append("PASS" if value else ("FAIL" if value is not None else "—"))
        print(f"| {task['id']} | " + " | ".join(cells) + " |")

    print("\nRanking (pre-registered rule): " + " > ".join(summary["ranking"]))
    print(f"Stable (usable >= {STABILITY_ROUNDS - 1}/{STABILITY_ROUNDS}): "
          + (", ".join(summary["verdict"]["stable_models"]) or "none"))
    if summary["verdict"]["capacity_limited"]:
        print("WARNING: run is capacity-limited (>=20% 429/5xx); do not read as quality ranking.")
    if summary["verdict"]["adapter_suspect"]:
        print("WARNING: adapter-suspect run (>=20% HTTP 200 with empty content); check the gateway.")
    print(f"Raw JSONL: {summary['results_file']}")
    print(f"SHA-256: {summary['results_sha256']}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--models", nargs="+", default=MODELS)
    parser.add_argument("--out-dir", default=os.path.join(os.path.dirname(__file__), "runs"))
    parser.add_argument("--tag", default="bench", help="file name prefix for this run (default: bench)")
    parser.add_argument("--no-cwd", action="store_true", help="omit the cwd field for backends that reject unknown properties (e.g. Bob, Mistral)")
    parser.add_argument("--max-tokens", type=int, default=0, help="override correctness max_tokens (default: per-task)")
    parser.add_argument("--stability-max-tokens", type=int, default=0, help="override stability max_tokens (default: 1024)")
    parser.add_argument("--top-p", type=float, default=None, help="send top_p (Mistral requires 1.0 with temperature 0)")
    parser.add_argument("--delay", type=float, default=1.0, help="sleep after a successful request (default: 1.0)")
    parser.add_argument("--rate-limit-delay", type=float, default=10.0, help="sleep after HTTP 429 (default: 10.0)")
    parser.add_argument("--phase-cooldown", type=float, default=0.0, help="sleep between correctness and stability phases (default: 0)")
    parser.add_argument("--max-consecutive-429", type=int, default=0, help="stop the run after N consecutive 429s (0 = never)")
    args = parser.parse_args()
    models = [model.strip() for model in args.models if model.strip()]
    if not models:
        print("no models selected", file=sys.stderr)
        return 2

    os.makedirs(CWD, exist_ok=True)
    os.makedirs(args.out_dir, exist_ok=True)
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    results_path = os.path.join(args.out_dir, f"{args.tag}-{stamp}.jsonl")
    summary_path = os.path.join(args.out_dir, f"{args.tag}-{stamp}-summary.json")

    records: list[dict] = []
    total_requests = len(models) * (len(TASKS) + STABILITY_ROUNDS)
    done = 0
    consecutive_429 = 0
    stop = False

    def note_status(result: dict) -> None:
        nonlocal consecutive_429, stop
        if result["status"] == 429:
            consecutive_429 += 1
            if args.max_consecutive_429 and consecutive_429 >= args.max_consecutive_429:
                print(
                    f"stopping after {consecutive_429} consecutive 429s",
                    flush=True,
                )
                stop = True
        else:
            consecutive_429 = 0

    with open(results_path, "w", encoding="utf-8") as sink:
        def write(record: dict) -> None:
            nonlocal done
            records.append(record)
            sink.write(json.dumps(record, ensure_ascii=False) + "\n")
            sink.flush()
            done += 1
            tag = "PASS" if record["passed"] else "FAIL"
            if record["phase"] == "stability":
                tag = "ok" if record["passed"] else "FAIL"
            print(
                f"[{done:02d}/{total_requests}] {record['phase'][:4]} "
                f"{record['model']} {record['task']} "
                f"HTTP {record['status']} {record['latency_s']:.2f}s {tag} "
                f"({record['detail'][:100]})",
                flush=True,
            )

        print(f"models: {', '.join(models)}")
        print(f"results: {results_path}")
        print(f"pre-registration: {os.path.join(os.path.dirname(__file__), '..', 'PRE-REGISTRATION.md')}")
        print(f"requests planned: {total_requests}\n")

        for task_index, task in enumerate(TASKS):
            for model in rotated(models, task_index):
                body = {
                    "model": model,
                    "messages": [{"role": "user", "content": task["prompt"]}],
                    "max_tokens": args.max_tokens or task.get("max_tokens", 2048),
                    "temperature": 0,
                }
                if not args.no_cwd:
                    body["cwd"] = CWD
                if args.top_p is not None:
                    body["top_p"] = args.top_p
                if task.get("tools"):
                    body["tools"] = task["tools"]
                result = post_chat(body)
                payload = parse_payload(result)
                passed, detail = grade(task, payload)
                write(make_record("correctness", model, task, result, payload, passed, detail))
                note_status(result)
                if stop:
                    break
                polite_sleep(result, args.delay, args.rate_limit_delay)
            if stop:
                break

        if args.phase_cooldown > 0 and not stop:
            print(f"phase cooldown: {args.phase_cooldown:.0f}s", flush=True)
            time.sleep(args.phase_cooldown)

        for round_index in range(STABILITY_ROUNDS):
            if stop:
                break
            for model in rotated(models, round_index):
                task = {
                    "id": f"stability-{round_index + 1}",
                    "kind": "stability",
                }
                prompt = (
                    f"Stability run #{round_index + 1}. Give exactly 3 bullet points explaining "
                    "why connection pooling reduces tail latency. Each bullet must be one "
                    "sentence of at most 18 words. Do not add a preamble or conclusion."
                )
                stability_body = {
                    "model": model,
                    "messages": [{"role": "user", "content": prompt}],
                    "max_tokens": args.stability_max_tokens or 1024,
                    "temperature": 0,
                }
                if not args.no_cwd:
                    stability_body["cwd"] = CWD
                if args.top_p is not None:
                    stability_body["top_p"] = args.top_p
                result = post_chat(stability_body)
                payload = parse_payload(result)
                message = message_of(payload)
                content = message.get("content") or ""
                finish = finish_reason_of(payload)
                usable = result["status"] == 200 and bool(content.strip()) and finish == "stop"
                detail = f"finish={finish} chars={len(content)} bullets={format_bullets(content)}"
                write(
                    make_record(
                        "stability", model, task, result, payload, usable, detail,
                        extra={"bullets": format_bullets(content)},
                    )
                )
                note_status(result)
                if stop:
                    break
                polite_sleep(result, args.delay, args.rate_limit_delay)

    summary = summarize(records, models, results_path)
    with open(summary_path, "w", encoding="utf-8") as handle:
        json.dump(summary, handle, indent=2, ensure_ascii=False)
        handle.write("\n")
    print_summary(summary, models)
    print(f"Summary JSON: {summary_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
