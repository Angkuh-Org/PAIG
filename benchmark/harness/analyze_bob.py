#!/usr/bin/env python3
"""Analyze a Bob alias benchmark JSONL: per-alias scores, families, latency, credits."""
from __future__ import annotations

import argparse
import collections
import json
import statistics
from pathlib import Path


def p90(values: list[float]) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    return round(ordered[min(len(ordered) - 1, max(0, int(round(0.9 * (len(ordered) - 1)))))], 3)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("jsonl")
    parser.add_argument("--out", default="")
    args = parser.parse_args()
    rows = [json.loads(line) for line in Path(args.jsonl).read_text(encoding="utf-8").splitlines() if line.strip()]
    models = list(dict.fromkeys(r["model"] for r in rows))
    result = {}
    for model in models:
        correctness = [r for r in rows if r["model"] == model and r["phase"] == "correctness"]
        stability = [r for r in rows if r["model"] == model and r["phase"] == "stability"]
        tasks = {r["task"]: bool(r.get("passed")) for r in correctness}
        c_ok = [r for r in correctness if r.get("status") == 200]
        s_ok = [r for r in stability if r.get("status") == 200 and (r.get("content") or "").strip()]
        lat = [r["latency_s"] for r in c_ok]
        stab_lat = [r["latency_s"] for r in s_ok]
        tokens = [r.get("usage", {}).get("completion_tokens", 0) for r in c_ok if r.get("usage")]
        credits = [r.get("usage", {}).get("credits", 0) for r in correctness + stability if r.get("usage")]
        errors = collections.Counter(
            str(r.get("status")) for r in correctness + stability if r.get("status") != 200
        )
        result[model] = {
            "score": f"{sum(tasks.values())}/{len(tasks)}",
            "score_int": sum(tasks.values()),
            "tasks": tasks,
            "stability": f"{sum(1 for r in stability if r.get('passed'))}/{len(stability)}",
            "stability_int": sum(1 for r in stability if r.get("passed")),
            "format": f"{sum(1 for r in stability if (r.get('bullets') or 0) >= 3)}/{len(stability)}",
            "suite_median_s": round(statistics.median(lat), 3) if lat else None,
            "suite_p90_s": p90(lat),
            "stability_median_s": round(statistics.median(stab_lat), 3) if stab_lat else None,
            "completion_tokens_median": round(statistics.median(tokens), 1) if tokens else None,
            "credits_total": round(sum(credits), 8),
            "credits_median": round(statistics.median(credits), 8) if credits else None,
            "tool_call": tasks.get("tool-call"),
            "errors": dict(errors),
        }

    print("| alias | suite | stability | format | median lat | p90 lat | tokens med | credits total |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- |")
    for model in models:
        item = result[model]
        print(
            f"| {model} | {item['score']} | {item['stability']} | {item['format']} | "
            f"{item['suite_median_s']} | {item['suite_p90_s']} | "
            f"{item['completion_tokens_median']} | {item['credits_total']} |"
        )

    print("\n| task | " + " | ".join(models) + " |")
    print("| --- | " + " | ".join("---" for _ in models) + " |")
    for task in tasks:
        print(f"| {task} | " + " | ".join("PASS" if result[m]["tasks"].get(task) else "FAIL" for m in models) + " |")

    families: dict[tuple, list[str]] = collections.defaultdict(list)
    for model in models:
        item = result[model]
        signature = (
            tuple(sorted(item["tasks"].items())),
            item["stability_int"],
            item["tool_call"],
        )
        families[signature].append(model)
    print("\nIdentical-result groups (same task matrix + stability + tool result):")
    for group in families.values():
        print("- " + ", ".join(group))

    ranking = sorted(models, key=lambda m: (-result[m]["score_int"], -result[m]["stability_int"], result[m]["suite_median_s"] or 1e9))
    print("\nRanking (suite, stability, latency):")
    for index, model in enumerate(ranking, 1):
        print(f"{index}. {model} — {result[model]['score']} suite, {result[model]['stability']} stability")

    if args.out:
        Path(args.out).write_text(json.dumps({"models": result, "ranking": ranking, "families": [list(v) for v in families.values()]}, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
