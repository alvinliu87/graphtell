#!/usr/bin/env python3
"""用真实 `graphtell recall` 流水线逐个跑评测集，统计 hit@5/10/20。

与 recall_eval.py 的区别：不依赖常驻 serve，直接拉起 CLI（每次冷加载 bge ~4s，
30 用例约 2 分钟）。判定口径完全一致：命中节点名（小写）含任一 target 即算命中。
用法：
    python3 tools/recall_cli_eval.py
    python3 tools/recall_cli_eval.py --project 15
"""
import argparse
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "graphtell")
KS = (5, 10, 20)
HIT_RE = re.compile(r"^\s*\d+\.\s+\S+\s+(.+?)\s+[\d.]+\s+跳数")


def recall(project, query, limit=20, timeout=180):
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--project")
    a = ap.parse_args()
    spec = json.load(open(a.cases, encoding="utf-8"))
    cases = spec["cases"]
    if a.project:
        want = {int(p) for p in a.project.split(",")}
        cases = [c for c in cases if c["project"] in want]
    projects = spec.get("projects", {})

    results = []
    for c in cases:
        names = recall(c["project"], c["query"])
        if names is None:
            print(f"#{c['project']} {c['query']}: 超时", file=sys.stderr)
            continue
        results.append({
            "project": c["project"], "query": c["query"],
            "lang": c.get("lang", "zh"),
            "hit": {k: hit_at(names, c["targets"], k) for k in KS},
            "top3": names[:3],
        })

    print(f"{'工程':<6}{'语':<4}{'查询':<26}{'@5':<6}{'@10':<6}{'@20':<6} 命中前3")
    print("-" * 100)
    for r in results:
        mk = lambda b: "✓" if b else "·"
        print(f"#{r['project']:<5}{r['lang']:<4}{r['query'][:24]:<26}"
              f"{mk(r['hit'][5]):<6}{mk(r['hit'][10]):<6}{mk(r['hit'][20]):<6} {r['top3']}")

    print("\n=== 汇总（真实流水线）===")
    for group, keyfn in (
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
