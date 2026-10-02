#!/usr/bin/env python3
"""Recall vs naive grep baseline comparison (proves "graph recall is no worse, and usually beats blind search").

Reuses the 30 labeled cases in tools/recall_cases.json; for each case it runs two retrieval setups:

  1) recall      -- the real `graphtell recall` pipeline (graph expansion + Chinese bridge).
  2) grep(terms) -- grep the source tree with the **same** English keywords the recall expanded.
                    This is the "fair" baseline: same words, just no graph.
  3) grep(raw)   -- grep with the words a **developer would actually type**:
                      * English query -> the English words in the query (minus pure stopwords);
                      * Chinese query -> the query verbatim (a Chinese string, which in source only
                        hits i18n almost everywhere).
                    This is the "naive" baseline: simulating an agent blind-searching with the user's
                    original words.

The hit criterion matches recall_cli_eval.py: hit = result contains any target substring (case-insensitive).
For recall it is "node name contains target"; for grep it is "file content contains target" (i.e. the file is
in the answer-file set).
Answer-file set = every file containing a target identifier, found via `grep -rIl`. This makes recall(node)
and grep(file) symmetrically comparable: both look at "does the answer symbol enter the top-k results".

It also reports "rank of the first answer file" within grep(terms), to show that even when grep can fish out
the answer file, it is usually buried among a pile of same-named files, while graph recall pushes the
**specific method** to the top 1~3.

Usage:
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
# NOTE: these regexes intentionally match the Chinese labels in `graphtell recall`'s current
# stdout ("跳数" = hops, "查询词：" / "结构提示" = query-term / structural-hint). Do NOT translate
# them unless the recall command's output is localized first, or parsing will break.
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
            # "（无）" (Chinese for "none") is the recall command's sentinel when no terms expand.
            terms = [] if t == "（无）" else [x.strip() for x in t.split(",")]
    return names, terms


def grep_ranked_files(root, terms):
    """Return the file list sorted by descending "hit-line count" (source only, noise dirs excluded)."""
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
    """Every file whose content contains a target identifier = the answer-location set."""
    if not targets:
        return set()
    pat = "|".join(re.escape(t) for t in targets)
    out = run(["grep", "-rIl", "-i", "-E", pat, root] + EXC)
    return set(out.splitlines()) if out else set()


def raw_terms(query, lang):
    if lang == "en":
        toks = re.findall(r"[a-zA-Z]+", query.lower())
        return [t for t in toks if t not in STOP]
    return [query.strip()]  # Chinese: a developer greps with the Chinese query verbatim


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
            print(f"#{p} {q}: recall timeout or no root", file=sys.stderr)
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
    print(f"{'proj':<6}{'lang':<4}{'query':<26}{'recall':<14}{'grep(same)':<16}{'grep(raw)':<14} topgraph / grep#")
    print("-" * 120)
    for r in rows:
        rec = "/".join(mk(r["recall"][k]) for k in KS)
        gt = "/".join(mk(r["gterms"][k]) for k in KS)
        gr = "/".join(mk(r["graw"][k]) for k in KS)
        grank = r["grank"] if r["grank"] else "—"
        print(f"#{r['p']:<5}{r['lang']:<4}{r['q'][:24]:<26}"
              f"{rec:<14}{gt:<16}{gr:<14} {r['rrank'] or '—'} / {grank}")
    print()
    print("=== summary ===")
    n = len(rows)
    for label, key in (("recall graph", "recall"),
                       ("grep(same)", "gterms"),
                       ("grep(raw)", "graw")):
        line = "   " + " ".join(
            f"@{k}={sum(1 for r in rows if r[key][k])}/{n}" for k in KS)
        print(f"  {label:<14}{line}")
    # Increment: graph pushes the answer into the top 3, while grep(same) buries the answer file
    # beyond >10 (or can't find it at all).
    better = [r for r in rows if r["rrank"] and r["rrank"] <= 3
              and (r["grank"] == 0 or r["grank"] > 10)]
    print(f"\n  graph recall pushes the answer into top 3, while grep(same) buries the answer "
          f"file beyond >10 or can't find it: {len(better)}/{n}")
    for r in better:
        print(f"    #{r['p']} {r['q']} -> topgraph{r['rrank']} / grep#{r['grank'] or '—'}")
    zh = [r for r in rows if r["lang"] == "zh"]
    zh_fail = [r for r in zh if not r["graw"][20]]
    print(f"  under Chinese queries, grepping with the raw Chinese finds no answer file at all: "
          f"{len(zh_fail)}/{len(zh)}")


if __name__ == "__main__":
    main()
