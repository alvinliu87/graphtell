#!/usr/bin/env python3
"""Recall quality eval: run one recall per query in the eval set, counting hit@5 / hit@10 / hit@20.

Usage (needs the resident service first):
    graphtell serve --port 5177
    python3 tools/recall_eval.py                          # evaluate and print
    python3 tools/recall_eval.py --out tools/baseline.json # also save a baseline
    python3 tools/recall_eval.py --compare tools/baseline.json  # compare against a baseline

Criterion: a hit node name (lowercased) containing any target (lowercased) counts as a hit.
Each case also carries an English control (lang=en): Chinese misses but English hits -> a
cross-lingual bridge problem; both miss -> a retrieval / ranking problem.
"""
import argparse
import json
import os
import sys
import urllib.error
import urllib.request

KS = (5, 10, 20)


def recall(base, project, query, limit=20, hops=2):
    req = urllib.request.Request(
        f"{base}/api/projects/{project}/recall",
        data=json.dumps(
            {"query": query, "limit": limit, "hops": hops, "with_snippets": False}
        ).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        d = json.load(r).get("data") or {}
    return [h["name"] for h in d.get("hits", [])]


def hit_at(names, targets, k):
    low = [n.lower() for n in names[:k]]
    return any(t.lower() in n for n in low for t in targets)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:5177")
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--out", help="把本次结果存为基线文件")
    ap.add_argument("--compare", help="与已有基线对比，输出增量")
    ap.add_argument(
        "--project",
        help="只评测指定工程（逗号分隔）。重算期间用它避开尚未预热的工程，"
        "否则首次查询会触发全量编码把服务拖到超时。",
    )
    a = ap.parse_args()

    with open(a.cases, encoding="utf-8") as f:
        spec = json.load(f)
    cases = spec["cases"]
    if a.project:
        want = {int(p) for p in a.project.split(",") if p.strip()}
        cases = [c for c in cases if c["project"] in want]
    projects = spec.get("projects", {})

    results = []
    for c in cases:
        try:
            names = recall(a.base, c["project"], c["query"])
        except urllib.error.URLError as e:
            print(f"无法连接常驻服务 {a.base}：{e}", file=sys.stderr)
            print("请先启动：graphtell serve", file=sys.stderr)
            return 2
        hits = {k: hit_at(names, c["targets"], k) for k in KS}
        # Warmup state: the persisted file exists -> semantic path; otherwise still on the fast (hash) path.
        # The two paths differ a lot in quality, so they must be viewed separately, never mixed in one summary.
        warm = os.path.exists(f"data/embeddings/{c['project']}.json")
        results.append(
            {
                "project": c["project"],
                "query": c["query"],
                "lang": c.get("lang", "zh"),
                "warm": warm,
                "hit": hits,
                "top5": names[:5],
            }
        )

    # ---- print
    print(f"{'工程':<6}{'预热':<6}{'语':<4}{'查询':<26}{'@5':<6}{'@10':<6}{'@20':<6} 命中前3")
    print("-" * 102)
    for r in results:
        mark = lambda b: "  ✓  " if b else "  ·  "
        print(
            f"#{r['project']:<5}{('语义' if r['warm'] else '快速'):<6}"
            f"{r['lang']:<4}{r['query'][:24]:<26}"
            f"{mark(r['hit'][5]):<6}{mark(r['hit'][10]):<6}{mark(r['hit'][20]):<6}"
            f" {r['top5'][:3]}"
        )

    print("\n=== 汇总 ===")
    for group, keyfn in (
        ("按工程", lambda r: f"#{r['project']} {projects.get(str(r['project']), '')}"),
        ("按语言", lambda r: "中文" if r["lang"] == "zh" else "英文(对照)"),
        ("按路径", lambda r: "语义(bge)" if r.get("warm") else "快速(哈希)"),
    ):
        print(f"  {group}:")
        buckets = {}
        for r in results:
            buckets.setdefault(keyfn(r), []).append(r)
        for g, rs in sorted(buckets.items()):
            line = "   " + " ".join(
                f"@{k}={sum(1 for r in rs if r['hit'][k])}/{len(rs)}" for k in KS
            )
            print(f"    {g[:46]:<48}{line}")
    total = len(results)
    print(
        "  总体: "
        + " ".join(
            f"@{k}={sum(1 for r in results if r['hit'][k])}/{total}" for k in KS
        )
    )

    if a.out:
        with open(a.out, "w", encoding="utf-8") as f:
            json.dump(results, f, ensure_ascii=False, indent=2)
        print(f"\n基线已写入 {a.out}")

    if a.compare:
        try:
            with open(a.compare, encoding="utf-8") as f:
                base = json.load(f)
        except OSError as e:
            print(f"读取基线失败：{e}", file=sys.stderr)
            return 2
        bmap = {(b["project"], b["query"]): b for b in base}
        print("\n=== 与基线对比（+ 改善 / - 退化）===")
        changed = 0
        for r in results:
            b = bmap.get((r["project"], r["query"]))
            if not b:
                continue
            # After a JSON round-trip keys become strings; convert them all back to int before comparing.
            bh = {int(kk): vv for kk, vv in b["hit"].items()}
            for k in KS:
                if r["hit"][k] != bh[k]:
                    sign = "+" if r["hit"][k] else "-"
                    print(f"  {sign} @{k} #{r['project']} {r['query']}")
                    changed += 1
        if not changed:
            print("  无变化")
    return 0


if __name__ == "__main__":
    sys.exit(main())
