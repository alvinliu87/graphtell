#!/usr/bin/env python3
"""Run the eval set case by case through the real `graphtell recall` pipeline, counting hit@5/10/20.

Difference from recall_eval.py: no resident serve needed, it invokes the CLI directly (cold-loads bge ~4s each time,
~2 minutes for 30 cases). The hit criterion is identical: a hit node name (lowercased) containing any target counts.
Usage:
    python3 tools/recall_cli_eval.py
    python3 tools/recall_cli_eval.py --project 15
    python3 tools/recall_cli_eval.py --workers 4   # run concurrently; saves time with many cases
"""
import argparse
import json
import os
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "graphtell")
KS = (5, 10, 20)
HIT_RE = re.compile(r"^\s*\d+\.\s+\S+\s+(.+?)\s+[\d.]+\s+跳数")


def recall(project, query, limit=20, timeout=600):
    try:
        r = subprocess.run(
            [BIN, "recall", "--project", str(project), "--query", query,
             "--limit", str(limit)],
            capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return None
    names = []
    for line in r.stdout.splitlines():
        m = HIT_RE.match(line)
        if m:
            names.append(m.group(1).strip())
    return names


def hit_at(names, targets, k):
    low = [n.lower() for n in names[:k]]
    return any(t.lower() in n for n in low for t in targets)


def run_case(c):
    names = recall(c["project"], c["query"])
    return {
        "project": c["project"], "query": c["query"],
        "lang": c.get("lang", "zh"), "batch": c.get("batch", 1),
        "targets": c["targets"],
        "hit": {k: hit_at(names, c["targets"], k) for k in KS} if names is not None
        else {k: False for k in KS},
        "top3": (names or [])[:3],
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--project")
    ap.add_argument("--workers", type=int, default=1,
                    help="并发进程数（默认 1：顺序执行，结果与 CPU 竞争无关）")
    ap.add_argument("--batch", help="只跑某批（1 / 2）")
    a = ap.parse_args()
    spec = json.load(open(a.cases, encoding="utf-8"))
    cases = spec["cases"]
    if a.project:
        want = {int(p) for p in a.project.split(",")}
        cases = [c for c in cases if c["project"] in want]
    if a.batch:
        cases = [c for c in cases if c.get("batch", 1) == int(a.batch)]
    projects = spec.get("projects", {})

    results = []
    if a.workers > 1:
        with ThreadPoolExecutor(max_workers=a.workers) as ex:
            for r in ex.map(run_case, cases):
                if not r["top3"] and not any(r["hit"].values()):
                    print(f"#{r['project']} {r['query']}: 超时", file=sys.stderr)
                results.append(r)
    else:
        for c in cases:
            r = run_case(c)
            if not r["top3"] and not any(r["hit"].values()):
                print(f"#{c['project']} {c['query']}: 超时", file=sys.stderr)
            results.append(r)

    print(f"{'批':<4}{'工程':<6}{'语':<4}{'查询':<26}{'@5':<6}{'@10':<6}{'@20':<6} 命中前3")
    print("-" * 106)
    for r in results:
        mk = lambda b: "✓" if b else "·"
        print(f"{r['batch']:<4}#{r['project']:<5}{r['lang']:<4}{r['query'][:24]:<26}"
              f"{mk(r['hit'][5]):<6}{mk(r['hit'][10]):<6}{mk(r['hit'][20]):<6} {r['top3']}")

    print("\n=== 汇总（真实流水线）===")
    for group, keyfn in (
        ("按批次", lambda r: f"batch{r['batch']}"),
        ("按工程", lambda r: f"#{r['project']} {projects.get(str(r['project']),'')}"),
        ("按语言", lambda r: "中文" if r["lang"] == "zh" else "英文(对照)"),
    ):
        print(f"  {group}:")
        buckets = {}
        for r in results:
            buckets.setdefault(keyfn(r), []).append(r)
        for g, rs in sorted(buckets.items()):
            line = "   " + " ".join(
                f"@{k}={sum(1 for r in rs if r['hit'][k])}/{len(rs)}" for k in KS)
            print(f"    {g[:44]:<46}{line}")
    total = len(results)
    print("  总体: " + " ".join(
        f"@{k}={sum(1 for r in results if r['hit'][k])}/{total}" for k in KS))


if __name__ == "__main__":
    main()
