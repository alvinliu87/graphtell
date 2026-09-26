#!/usr/bin/env python3
"""端到端「定位」任务基准：A(图召回引导) vs B(grep 后读，无图)。

这是"不输 AI IDE"最硬的一块证据，但先说清边界：本脚本跑的是**检索定位层**的
端到端——agent 的任务就是"该在哪改"（这正是 recall 的全部职责；真正写代码是
codegen，与召回正交）。agent 行为用**确定性策略**模拟，可在不依赖 LLM API 的前提
下复现；要接真 LLM，只需把下面 `agent_recall` / `agent_grep` 两个策略函数换成
"调用模型 + 工具"的驱动即可，评分口径不变。

任务来源：复用 recall_cases.json 的 30 例，把查询当"改动指令"，gold = 含 target
标识符的文件集合（即正确答案所在地）。

两种策略（都用同一套 token 计量：字符数//4，与 token_savings_eval 一致）：

  A  agent_recall  —— 图召回引导
      1) graphtell recall --limit 10  → 上下文成本 C_A（召回包 token）
      2) 读召回 top1 命中所在文件     → 读文件成本 R_top
      成功 = top1 文件 ∈ gold；总成本 = C_A + R_top；读了 1 个文件。

  B  agent_grep    —— 无图，开发者式 grep 后读若干候选
      1) 用召回同一套展开词 grep 源码 → 上下文成本 C_B（grep 输出 token）
      2) 读 grep 排名前 K(默认8) 的文件 → 读文件成本 R_K
      成功 = gold ∩ 前K文件 非空；总成本 = C_B + R_K；读了 K 个文件。

这样 A 与 B **检索词完全相同**，唯一区别是"有没有图"——公平隔离出图召回的增量价值。
另报 B-raw（拿用户原话 grep）作朴素对照，体现跨语言鸿沟。

用法：
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
    """图召回引导：agent 读召回短列表(top10)，从中挑出 gold 文件，再读该文件。"""
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
        # 结论：A 成功且(更省 或 B 失败)
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
