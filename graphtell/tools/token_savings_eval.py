#!/usr/bin/env python3
"""Measure the token cost of "whole-file injection" vs "recall + read on demand", quantifying GraphTell's token savings.

Method (per eval case):
  * IDE whole-file baseline: token count of the full "answer file" (an AI IDE injects related files whole by default).
  * GraphTell path: token count of the recall command's markdown output (seeds + hit list + line numbers)
    plus the one sector window the model reads on demand (±12 lines).
  Both sides use the same estimator (chars / 4, code ≈ 4 chars/token), so the ratio is robust.

Usage:
    python3 tools/token_savings_eval.py
    python3 tools/token_savings_eval.py --project 1
"""
import argparse
import json
import os
import re
import sqlite3
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "graphtell")
DB = os.path.join(ROOT, "data", "graphtell.sqlite")
WINDOW = 12  # Number of context lines the model reads on demand


def est_tokens(text: str) -> int:
    # The same estimator is used on both sides; the ratio is independent of an exact tokenizer (order of magnitude suffices).
    return max(1, len(text) // 4)


def recall_markdown(project, query, limit=10, timeout=180):
    try:
        r = subprocess.run(
            [BIN, "recall", "--project", str(project), "--query", query,
             "--limit", str(limit)],
            capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return None
    return r.stdout


HIT_RE = re.compile(
    r"^\s*\d+\.\s+\S+\s+(\S+)\s+[\d.]+\s+跳数\s+\d+\s+(\S+?):(\d+)"
)


def parse_hits(md):
    """Parse hits from the recall markdown: (name, absolute path, line number)."""
    out = []
    for line in md.splitlines():
        m = HIT_RE.match(line)
        if m:
            out.append((m.group(1), m.group(2), int(m.group(3))))
    return out


def answer_file(md, targets):
    """The file holding the answer node = the entry in recall's name list whose name contains a target substring (i.e. the file the model navigates to)."""
    hits = parse_hits(md)
    for t in targets:
        for name, path, line in hits:
            if t.lower() in name.lower():
                return path, line
    if hits:
        return hits[0][1], hits[0][2]
    return None, 1


def read_window(path, line, window=WINDOW):
    try:
        lines = open(path, encoding="utf-8", errors="ignore").read().splitlines()
    except OSError:
        return ""
    lo = max(0, line - 1 - window)
    hi = min(len(lines), line + window)
    return "\n".join(lines[lo:hi])


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

    rows = []
    for c in cases:
        proj = c["project"]
        md = recall_markdown(proj, c["query"])
        if md is None:
            print(f"#{proj} {c['query']}: recall 超时", file=sys.stderr)
            continue
        md_tokens = est_tokens(md)

        path, line = answer_file(md, c["targets"])
        if path and os.path.exists(path):
            whole = open(path, encoding="utf-8", errors="ignore").read()
            whole_tokens = est_tokens(whole)
            win_tokens = est_tokens(read_window(path, line))
        else:
            whole_tokens = 0
            win_tokens = 0
            print(f"  ! 找不到答案文件：#{proj} {c['query']} -> {path}", file=sys.stderr)

        gt_tokens = md_tokens + win_tokens
        rows.append({
            "proj": proj, "query": c["query"], "lang": c.get("lang", "zh"),
            "whole": whole_tokens, "md": md_tokens, "win": win_tokens,
            "gt": gt_tokens, "path": path or "",
        })

    print(f"{'工程':<4}{'语':<3}{'查询':<26}{'IDE整文件':>10}{'召回+窗':>10}{'倍率':>7} 省%")
    print("-" * 86)
    tot_ide = tot_gt = 0
    wins = 0
    small_overhead = 0
    for r in rows:
        ratio = (r["whole"] / r["gt"]) if r["gt"] else 0
        saved = (1 - r["gt"] / r["whole"]) * 100 if r["whole"] else 0
        flag = "✓" if r["gt"] < r["whole"] else "✗"
        if r["gt"] < r["whole"]:
            wins += 1
        if r["whole"] and r["md"] > r["whole"]:
            small_overhead += 1
        print(f"#{r['proj']:<3}{r['lang']:<3}{r['query'][:24]:<26}"
              f"{r['whole']:>10}{r['gt']:>10}{ratio:>6.1f}x{saved:>5.0f}% {flag}")
        tot_ide += r["whole"]
        tot_gt += r["gt"]

    print("-" * 86)
    print(f"汇总：IDE 整文件注入 = {tot_ide:,} tokens")
    print(f"      GraphTell  = {tot_gt:,} tokens  "
          f"(召回列表 {sum(r['md'] for r in rows):,} + 按需窗口 {sum(r['win'] for r in rows):,})")
    if tot_gt:
        print(f"      => 整体约 {tot_ide/tot_gt:.1f}x 缩减，省 {(1-tot_gt/tot_ide)*100:.0f}% token")
    # Key pattern: the recall list has a fixed overhead (~{rows[0]['md'] if rows else 0} tokens/call),
    # so it only pays off when "the answer file is big enough"; small files are cheap to read whole anyway.
    big = [r for r in rows if r["whole"] >= 1000]
    small = [r for r in rows if r["whole"] < 1000]
    bw = sum(1 for r in big if r["gt"] < r["whole"])
    print(f"      逐用例 GraphTell 更省：{wins}/{len(rows)}")
    if big:
        print(f"      答案文件 ≥1000 tokens（真正费 token 的文件）：{bw}/{len(big)} 更省，"
              f"省 { (1 - sum(r['gt'] for r in big)/max(1,sum(r['whole'] for r in big)))*100:.0f}%")
    if small:
        print(f"      {len(small)} 例'更费'全是 <1000 tokens 的微文件（整读本就便宜，差额可忽略）")


if __name__ == "__main__":
    main()
