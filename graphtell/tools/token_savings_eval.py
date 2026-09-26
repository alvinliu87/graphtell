#!/usr/bin/env python3
"""实测「整文件注入」vs「召回 + 按需读」的 token 消耗，量化 GraphTell 省 token 的比例。

做法（每个评测用例）：
  * IDE 整文件基线：读取"答案所在文件"的全文 token 数（AI IDE 默认把相关文件整篇注入）。
  * GraphTell 路径：recall 命令的 markdown 输出（含种子 + 命中列表 + 行号）token 数
    + 模型按需读的那一扇区窗口（±12 行）token 数。
  两侧用同一估算器（字符数 / 4，代码≈4 字符/token），故比值稳健。

用法：
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
WINDOW = 12  # 模型按需读取的上下行数


def est_tokens(text: str) -> int:
    # 同一估算器用于两侧，比值与精确 tokenizer 无关（量级正确即可）。
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
    """从 recall markdown 解析命中：(名称, 绝对路径, 行号)。"""
    out = []
    for line in md.splitlines():
        m = HIT_RE.match(line)
        if m:
            out.append((m.group(1), m.group(2), int(m.group(3))))
    return out


def answer_file(md, targets):
    """答案节点所在文件 = recall 命名单中名字含 target 子串的那条（即模型会导航到的文件）。"""
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
    # 关键模式：召回列表有固定开销（~{rows[0]['md'] if rows else 0} tokens/次），
    # 只在"答案文件够大"时划算；小文件整读本就便宜。
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
