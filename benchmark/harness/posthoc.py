#!/usr/bin/env python3
"""Post-hoc exploratory probe (NOT part of the pre-registered suite).

Repeats the modular-exponentiation prompt N times per model, interleaved and
rotated, to separate intermittent empty content from a wrong-but-usable answer.

Writes a JSONL next to the main run and prints a compact summary.

Usage:
  python3 benchmark/harness/posthoc.py --tag cc-glyph-posthoc \
      --out-dir benchmark/commandcode/runs --models cc/stealth/glyph-cluster:free --rounds 10
"""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import statistics
import time
import urllib.error
import urllib.request

BASE = os.environ.get("PAIG_CHAT_URL", "http://127.0.0.1:8899/v1/chat/completions")
CWD = "/tmp/paig-bench-empty"
PROMPT = "Compute 7^123 mod 1000.\nEnd with exactly one line: ANSWER: <number>"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def call(body: dict) -> dict:
    request = urllib.request.Request(
        BASE, data=json.dumps(body).encode(), headers={"content-type": "application/json"}
    )
    started = time.monotonic()
    try:
        with OPENER.open(request, timeout=240) as response:
            return {
                "status": response.status,
                "raw": response.read().decode("utf-8", "replace"),
                "latency_s": time.monotonic() - started,
            }
    except urllib.error.HTTPError as error:
        return {
            "status": error.code,
            "raw": error.read().decode("utf-8", "replace"),
            "latency_s": time.monotonic() - started,
        }
    except Exception as error:  # noqa: BLE001
        return {"status": None, "raw": "", "latency_s": time.monotonic() - started,
                "error": f"{type(error).__name__}: {error}"}


def rotated(items: list[str], index: int) -> list[str]:
    k = index % len(items)
    return items[k:] + items[:k]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--models", nargs="+", required=True)
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--out-dir", default=os.path.join(os.path.dirname(__file__), "runs"))
    parser.add_argument("--tag", default="posthoc-modpow")
    parser.add_argument("--no-cwd", action="store_true")
    args = parser.parse_args()
    models = [model.strip() for model in args.models if model.strip()]
    rounds = max(1, args.rounds)

    os.makedirs(CWD, exist_ok=True)
    os.makedirs(args.out_dir, exist_ok=True)
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    path = os.path.join(args.out_dir, f"{args.tag}-{stamp}.jsonl")
    records = []
    with open(path, "w", encoding="utf-8") as sink:
        for round_index in range(rounds):
            for model in rotated(models, round_index):
                body = {
                    "model": model,
                    "messages": [{"role": "user", "content": PROMPT}],
                    "max_tokens": 2048,
                    "temperature": 0,
                }
                if not args.no_cwd:
                    body["cwd"] = CWD
                result = call(body)
                try:
                    payload = json.loads(result["raw"])
                except (json.JSONDecodeError, TypeError):
                    payload = None
                message = {}
                finish = None
                usage = {}
                if payload:
                    try:
                        choice = payload["choices"][0]
                        message = choice.get("message") or {}
                        finish = choice.get("finish_reason")
                    except (KeyError, IndexError, TypeError):
                        pass
                    usage = payload.get("usage") or {}
                content = message.get("content") or ""
                reasoning = message.get("reasoning_content") or ""
                line = None
                matches = re.findall(r"^\s*ANSWER:\s*(.+?)\s*$", content, re.MULTILINE | re.IGNORECASE)
                if matches:
                    line = matches[-1].strip()
                usable = result["status"] == 200 and bool(content.strip()) and finish == "stop"
                correct = bool(line) and "343" in re.findall(r"-?\d+", line or "")
                record = {
                    "ts": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
                    "probe": "posthoc-reasoning-stability",
                    "round": round_index + 1,
                    "model": model,
                    "status": result["status"],
                    "latency_s": round(result["latency_s"], 3),
                    "finish_reason": finish,
                    "content": content,
                    "reasoning_chars": len(reasoning),
                    "usage": usage,
                    "usable": usable,
                    "correct": correct,
                }
                records.append(record)
                sink.write(json.dumps(record, ensure_ascii=False) + "\n")
                sink.flush()
                print(
                    f"[{len(records):02d}/{rounds * len(models)}] {model} "
                    f"HTTP {record['status']} {record['latency_s']:.2f}s "
                    f"usable={int(usable)} correct={int(correct)} "
                    f"chars={len(content)} reasoning={len(reasoning)}",
                    flush=True,
                )
                time.sleep(1.0)

    print(f"\npost-hoc summary ({rounds} repeats, modpow):")
    print("| model | usable | correct | empty content | median lat (s) |")
    print("| --- | --- | --- | --- | --- |")
    for model in models:
        rows = [r for r in records if r["model"] == model]
        usable = sum(1 for r in rows if r["usable"])
        correct = sum(1 for r in rows if r["correct"])
        empty = sum(1 for r in rows if r["status"] == 200 and not r["content"].strip())
        latencies = [r["latency_s"] for r in rows if r["usable"]]
        median = round(statistics.median(latencies), 3) if latencies else None
        print(f"| {model} | {usable}/{rounds} | {correct}/{rounds} | {empty}/{rounds} | {median} |")
    digest = hashlib.sha256(open(path, "rb").read()).hexdigest()
    print(f"\nraw: {path}\nsha256: {digest}")
    print("label: post-hoc exploratory; it does not change the pre-registered score.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
