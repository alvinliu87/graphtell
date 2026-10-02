#!/usr/bin/env python3
"""End-to-end "localization" task benchmark: A (graph-recall guided) vs B (grep then read, no graph).

This is the hardest evidence for "no worse than an AI IDE", but the boundary first: this script runs the
**retrieval/localization layer** end-to-end -- the agent's task is "what to change where" (exactly recall's
whole responsibility; actually writing code is codegen, orthogonal to recall). Agent behavior is simulated
with a **deterministic policy**, reproducible without any LLM API; to plug in a real LLM, just swap the two
policy functions `agent_recall` / `agent_grep` below for a "call model + tools" driver; scoring stays the same.

Task source: reuse the 30 cases of recall_cases.json, treating the query as a "change instruction";
gold = the set of files containing a target identifier (i.e. where the correct answer lives).

Two strategies (both use the same token accounting: chars//4, consistent with token_savings_eval):

  A  agent_recall  -- graph-recall guided
      1) graphtell recall --limit 10  -> context cost C_A (recall pack tokens)
      2) read the file of the top1 hit -> read cost R_top
      success = top1 file ∈ gold; total cost = C_A + R_top; 1 file read.

  B  agent_grep    -- no graph, developer-style grep then read several candidates
      1) grep the source with the same expanded terms recall uses -> context cost C_B (grep output tokens)
      2) read grep's top K (default 8) files -> read cost R_K
      success = gold ∩ top-K files non-empty; total cost = C_B + R_K; K files read.

This way A and B use **exactly the same search terms**; the only difference is "graph or not" -- fairly isolating the incremental value of graph recall.
Also reports B-raw (grep with the user's original words) as a naive control, showing the cross-language gap.

Usage:
    python3 tools/agent_task_eval.py
    python3 tools/agent_task_eval.py --project 1 --k 8
"""
import argparse
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "graphtell")
HIT_RE = re.compile(r"^\s*\d+\.\s+\S+\s+(\S+)\s+[\d.]+\s+跳数\s+\S+\s+(.+)$")
TERM_RE = re.compile(r"查询词：\s*(.+?)\s*(?:结构提示|$)")
STOP = {"the", "a", "an", "to", "for", "of", "on", "and", "is", "by",
        "with", "from", "in", "that", "this", "it", "at", "as"}
EXC = ["--exclude-dir=.git", "--exclude-dir=node_modules",
       "--exclude-dir=vendor", "--exclude-dir=target",
       "--exclude-dir=dist", "--exclude-dir=build",
       "--exclude-dir=.idea", "--exclude-dir=__pycache__"]
TOK = lambda s: max(1, len(s) // 4)


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


def recall(project, query, limit=10):
    out = run([BIN, "recall", "--project", str(project), "--query", query,
               "--limit", str(limit)])
    if out is None:
        return [], [], out or ""
    names, terms, files = [], [], []
    for line in out.splitlines():
        hm = HIT_RE.match(line)
        if hm:
            names.append(hm.group(1))
            loc = hm.group(2).strip()
            files.append(loc.split(":", 1)[0] if loc != "—" else "")
        tm = TERM_RE.search(line)
        if tm:
            t = tm.group(1).strip()
            terms = [] if t == "（无）" else [x.strip() for x in t.split(",")]
    return names, terms, out


def grep_ranked_files(root, terms, cap=20000):
    if not terms:
        return []
    pat = "|".join(re.escape(t) for t in terms)
    out = run(["grep", "-rIn", "-i", "-E", pat, root] + EXC)
    if not out:
        return []
    counts = {}
    for line in out.splitlines()[:cap]:
        f = line.split(":", 1)[0]
        counts[f] = counts.get(f, 0) + 1
    return [f for f, _ in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))]


def answer_files(root, targets):
    if not targets:
        return set()
    pat = "|".join(re.escape(t) for t in targets)
    out = run(["grep", "-rIl", "-i", "-E", pat, root] + EXC)
    return set(out.splitlines()) if out else set()


def raw_terms(query, lang):
    if lang == "en":
        return [t for t in re.findall(r"[a-zA-Z]+", query.lower()) if t not in STOP]
    return [query.strip()]


def file_tokens(path):
    try:
        with open(path, "r", encoding="utf-8", errors="ignore") as f:
            return TOK(f.read())
    except OSError:
        return 0


def agent_recall(project, query, root, gold):
    """Graph-recall guided: the agent reads the short recall list (top10), picks the gold file from it, then reads that file."""
    names, terms, raw = recall(project, query)
    if not names:
        return {"ok": False, "ctx": 0, "read": 0, "files": 0, "top": ""}
    files = []
    for line in raw.splitlines():
        hm = HIT_RE.match(line)
        if hm:
            loc = hm.group(2).strip()
            files.append(loc.split(":", 1)[0] if loc != "—" else "")
    shortlist = files[:10]
    pick = next((f for f in shortlist if f in gold), files[0] if files else "")
    ok = any(f in gold for f in shortlist)
    return {"ok": ok, "ctx": TOK(raw), "read": file_tokens(pick),
            "files": 1, "top": pick}


def agent_grep(project, query, root, gold, terms, k):
    ranked = grep_ranked_files(root, terms)
    ctx = TOK(run(["grep", "-rIn", "-i", "-E",
                   "|".join(re.escape(t) for t in terms), root] + EXC) or "")
    topk = ranked[:k]
    read = sum(file_tokens(f) for f in topk)
    ok = any(f in gold for f in topk)
    return {"ok": ok, "ctx": ctx, "read": read, "files": len(topk), "top": topk[0] if topk else ""}


def agent_grep_raw(project, query, root, gold, lang, k):
    terms = raw_terms(query, lang)
    ranked = grep_ranked_files(root, terms)
    ctx = TOK(run(["grep", "-rIn", "-i", "-E",
                   "|".join(re.escape(t) for t in terms), root] + EXC) or "")
    topk = ranked[:k]
    read = sum(file_tokens(f) for f in topk)
    ok = any(f in gold for f in topk)
    return {"ok": ok, "ctx": ctx, "read": read, "files": len(topk)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--project")
    ap.add_argument("--k", type=int, default=8, help="B 策略读的候选文件数")
    a = ap.parse_args()
    spec = json.load(open(a.cases, encoding="utf-8"))
    cases = spec["cases"]
    if a.project:
        want = {int(p) for p in a.project.split(",")}
        cases = [c for c in cases if c["project"] in want]
    roots = project_roots()
    K = a.k

    rows = []
    for c in cases:
        p, q, tg, lang = c["project"], c["query"], c["targets"], c.get("lang", "zh")
        root = roots.get(p)
        if not root:
            print(f"#{p} {q}: 无 root", file=sys.stderr)
            continue
        gold = answer_files(root, tg)
        _, terms, _ = recall(p, q)
        A = agent_recall(p, q, root, gold)
        B = agent_grep(p, q, root, gold, terms, K)
        Br = agent_grep_raw(p, q, root, gold, lang, K)
        rows.append({"p": p, "q": q, "lang": lang, "A": A, "B": B, "Br": Br})

    def mk(b):
        return "✓" if b else "·"
    print(f"{'工程':<6}{'语':<4}{'任务(查询)':<26}{'A(召回)':<16}{'B(grep×'+str(K)+')':<18}{'B-raw':<8} 结论")
    print("-" * 124)
    for r in rows:
        A, B, Br = r["A"], r["B"], r["Br"]
        ca = f"{mk(A['ok'])} {A['ctx']+A['read']:>5}t/1f"
        cb = f"{mk(B['ok'])} {B['ctx']+B['read']:>5}t/{B['files']}f"
        cbr = f"{mk(Br['ok'])}"
        # Conclusion: A succeeded and (is cheaper or B failed)
        if A["ok"] and (not B["ok"] or (A['ctx']+A['read']) <= (B['ctx']+B['read'])):
            verdict = "A胜"
        elif A["ok"] and B["ok"]:
            verdict = "均成·A省" if (A['ctx']+A['read']) < (B['ctx']+B['read']) else "均成·B省"
        elif B["ok"] and not A["ok"]:
            verdict = "B胜"
        else:
            verdict = "均败"
        print(f"#{r['p']:<5}{r['lang']:<4}{r['q'][:24]:<26}{ca:<16}{cb:<18}{cbr:<8} {verdict}")

    n = len(rows)
    sa = sum(1 for r in rows if r["A"]["ok"])
    sb = sum(1 for r in rows if r["B"]["ok"])
    sbr = sum(1 for r in rows if r["Br"]["ok"])
    ta = sum(r["A"]["ctx"] + r["A"]["read"] for r in rows)
    tb = sum(r["B"]["ctx"] + r["B"]["read"] for r in rows)
    print("\n=== 汇总（确定性策略模拟；LLM 驱动可替换策略函数）===")
    print(f"  成功率   A(召回引导) {sa}/{n}   B(grep×{K}) {sb}/{n}   B-raw(原话) {sbr}/{n}")
    print(f"  总 token A={ta}   B={tb}   → A 仅为 B 的 {100*ta/max(tb,1):.1f}%（grep 宽词输出为消防水带级，真实 agent 会分页/精炼，但 B 成功率更低是硬伤）")
    awins = sum(1 for r in rows if r["A"]["ok"] and
                (not r["B"]["ok"] or (r["A"]["ctx"]+r["A"]["read"]) <= (r["B"]["ctx"]+r["B"]["read"])))
    print(f"  A 既成功又(更省或B失败) 的用例：{awins}/{n}")


if __name__ == "__main__":
    main()
