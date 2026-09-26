#!/usr/bin/env python3
"""召回 vs 朴素 grep 基线对照（证明"图召回不输、且常优于盲搜"）。

复用 tools/recall_cases.json 的 30 个带标注用例，对每一例跑两套检索：

  1) recall      —— `graphtell recall` 真实流水线（图扩展 + 中文桥）。
  2) grep(terms) —— 用**召回同一套**展开出的英文关键词去 grep 源码树。
                    这是"公平"基线：同样的词，只是没有图。
  3) grep(raw)   —— 用**开发者会直接敲**的词去 grep：
                      * 英文查询 → 查询里的英文词（去掉纯停用词）；
                      * 中文查询 → 查询原文（中文串，源码里几乎只命中 i18n）。
                    这是"朴素"基线：模拟 agent 拿用户原话去盲搜。

判定口径与 recall_cli_eval.py 一致：命中 = 结果包含任一 target 子串（大小写不敏感）。
对召回是"节点名含 target"；对 grep 是"文件内容含 target"（即文件在答案文件集合里）。
答案文件集合 = 用 grep -rIl 找出所有含 target 标识符的文件。这样召回(节点) 与
grep(文件) 对称可比：两边都看"答案符号是否进前 k 结果"。

额外给出 grep(terms) 里"首个答案文件的排名"，用来展示：即使 grep 也能捞到答案文件，
它常被埋在一堆同名文件里，而图召回把**具体方法**顶到前 1~3。

用法：
    python3 tools/grep_baseline_eval.py
    python3 tools/grep_baseline_eval.py --project 1
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
TERM_RE = re.compile(r"查询词：\s*(.+?)\s*(?:结构提示|$)")
STOP = {"the", "a", "an", "to", "for", "of", "on", "and", "is", "by",
        "with", "from", "in", "that", "this", "it", "at", "as"}
EXC = ["--exclude-dir=.git", "--exclude-dir=node_modules",
       "--exclude-dir=vendor", "--exclude-dir=target",
       "--exclude-dir=dist", "--exclude-dir=build",
       "--exclude-dir=.idea", "--exclude-dir=__pycache__"]


def run(cmd, timeout=240):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.stdout
    except subprocess.TimeoutExpired:
        return None


def project_roots():
    out = run([BIN, "list"]) or ""
    roots = {}
    for line in out.splitlines():
        m = re.match(r"#(\d+)\s+\S+\s+\S+\s+(.+)$", line.strip())
        if m:
            roots[int(m.group(1))] = m.group(2).strip()
    return roots


def recall(project, query, limit=20):
    out = run([BIN, "recall", "--project", str(project), "--query", query,
               "--limit", str(limit)])
    if out is None:
        return None, []
    names, terms = [], []
    for line in out.splitlines():
        hm = HIT_RE.match(line)
        if hm:
            names.append(hm.group(1).strip())
        tm = TERM_RE.match(line)
        if tm:
            t = tm.group(1).strip()
            terms = [] if t == "（无）" else [x.strip() for x in t.split(",")]
    return names, terms


def grep_ranked_files(root, terms):
    """返回按"命中行数"降序排列的文件列表（仅源码，排除噪声目录）。"""
    if not terms:
        return []
    pat = "|".join(re.escape(t) for t in terms)
    out = run(["grep", "-rIn", "-i", "-E", pat, root] + EXC)
    if not out:
        return []
    counts = {}
    for line in out.splitlines():
        f = line.split(":", 1)[0]
        counts[f] = counts.get(f, 0) + 1
    return [f for f, _ in sorted(counts.items(),
                                 key=lambda kv: (-kv[1], kv[0]))]


def answer_files(root, targets):
    """所有"内容含 target 标识符"的文件 = 答案所在地集合。"""
    if not targets:
        return set()
    pat = "|".join(re.escape(t) for t in targets)
    out = run(["grep", "-rIl", "-i", "-E", pat, root] + EXC)
    return set(out.splitlines()) if out else set()


def raw_terms(query, lang):
    if lang == "en":
        toks = re.findall(r"[a-zA-Z]+", query.lower())
        return [t for t in toks if t not in STOP]
    return [query.strip()]  # 中文：开发者直接拿中文去搜


def hit_at_names(names, targets, k):
    low = [n.lower() for n in names[:k]]
    return any(t.lower() in n for n in low for t in targets)


def hit_at_files(ordered, ans, k):
    return any(f in ans for f in ordered[:k])


def first_rank(ordered, ans):
    for i, f in enumerate(ordered, 1):
        if f in ans:
            return i
    return 0


def recall_rank(names, targets):
    for i, n in enumerate(names, 1):
        if any(t.lower() in n.lower() for t in targets):
            return i
    return 0


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
    roots = project_roots()

    rows = []
    for c in cases:
        p, q, tg, lang = c["project"], c["query"], c["targets"], c.get("lang", "zh")
        root = roots.get(p)
        names, terms = recall(p, q)
        if names is None or root is None:
            print(f"#{p} {q}: recall 超时或无 root", file=sys.stderr)
            continue
        ans = answer_files(root, tg)
        g_terms = grep_ranked_files(root, terms)
        g_raw = grep_ranked_files(root, raw_terms(q, lang))
        rows.append({
            "p": p, "q": q, "lang": lang,
            "recall": {k: hit_at_names(names, tg, k) for k in KS},
            "gterms": {k: hit_at_files(g_terms, ans, k) for k in KS},
            "graw": {k: hit_at_files(g_raw, ans, k) for k in KS},
            "grank": first_rank(g_terms, ans),
            "rrank": recall_rank(names, tg),
        })

    def mk(b):
        return "✓" if b else "·"
    print(f"{'工程':<6}{'语':<4}{'查询':<26}{'recall':<14}{'grep(同词)':<16}{'grep(原始)':<14} 图top / grep#")
    print("-" * 120)
    for r in rows:
        rec = "/".join(mk(r["recall"][k]) for k in KS)
        gt = "/".join(mk(r["gterms"][k]) for k in KS)
        gr = "/".join(mk(r["graw"][k]) for k in KS)
        grank = r["grank"] if r["grank"] else "—"
        print(f"#{r['p']:<5}{r['lang']:<4}{r['q'][:24]:<26}"
              f"{rec:<14}{gt:<16}{gr:<14} {r['rrank'] or '—'} / {grank}")
    print()
    print("=== 汇总 ===")
    n = len(rows)
    for label, key in (("recall 图召回", "recall"),
                       ("grep(同词)", "gterms"),
                       ("grep(原始)", "graw")):
        line = "   " + " ".join(
            f"@{k}={sum(1 for r in rows if r[key][k])}/{n}" for k in KS)
        print(f"  {label:<14}{line}")
    # 增量：图把答案顶进前3，而 grep(同词) 把答案文件埋到 >10（或捞不到）
    better = [r for r in rows if r["rrank"] and r["rrank"] <= 3
              and (r["grank"] == 0 or r["grank"] > 10)]
    print(f"\n  图召回把答案顶进前3、而 grep(同词) 把答案文件埋到 >10 或捞不到：{len(better)}/{n}")
    for r in better:
        print(f"    #{r['p']} {r['q']} → 图top{r['rrank']} / grep#{r['grank'] or '—'}")
    zh = [r for r in rows if r["lang"] == "zh"]
    zh_fail = [r for r in zh if not r["graw"][20]]
    print(f"  中文查询下，拿原始中文去 grep 捞不到任何答案文件：{len(zh_fail)}/{len(zh)}")


if __name__ == "__main__":
    main()
