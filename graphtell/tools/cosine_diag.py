#!/usr/bin/env python3
"""Cosine diagnostic: replay, in isolation, the "vector seed" step of the recall pipeline -- encode the
Chinese/English query with bge, then rank all persisted node vectors by pure cosine and see where the "true answer node" lands.

Purpose: decide whether the Chinese recall gap (Chinese @20 ≈ 46%) is
  (A) a retrieval / ranking problem: the true answer is actually high in cosine space but got
      "missed" by the lexical seeds / BFS expansion / top-k truncation -> cheap to fix (change seed selection, expansion policy).
  (B) an embedding problem: the true answer doesn't rank within top-k even in cosine space -> a stronger bridge / better doc text is needed.

The hit criterion is exactly the same as recall_eval.py (a hit name containing any target counts),
so it can be compared directly against recall_baseline.json (pipeline results):
  - pure cosine hits but the pipeline misses -> retrieval / ranking bug (cheap fix)
  - pure cosine also misses -> embedding / bridge problem (hard)

Usage:
    python3 tools/cosine_diag.py
    python3 tools/cosine_diag.py --project 13
"""
import argparse
import json
import os
import sqlite3

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ONNX = os.path.join(ROOT, "models", "bge-m3-onnx", "model.onnx")
TOK = os.path.join(ROOT, "models", "bge-m3", "tokenizer.json")
PREFIX = "Represent this sentence for searching relevant passages: "
KS = (5, 10, 20)

_sess = None
_tok = None


def embed(text):
    global _sess, _tok
    if _sess is None:
        _sess = ort.InferenceSession(ONNX, providers=["CPUExecutionProvider"])
        _tok = Tokenizer.from_file(TOK)
    ids = _tok.encode(PREFIX + text).ids
    n = len(ids)
    feeds = {
        "input_ids": np.array([ids], dtype=np.int64),
        "attention_mask": np.ones((1, n), dtype=np.int64),
        "token_type_ids": np.zeros((1, n), dtype=np.int64),
        "position_ids": np.arange(n, dtype=np.int64).reshape(1, n),
    }
    out = _sess.run(None, feeds)
    vec = out[0][0, 0, :].astype(np.float32)
    return vec / np.linalg.norm(vec)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--embed-dir", default="data/embeddings")
    ap.add_argument("--db", default="data/graphtell.sqlite")
    ap.add_argument("--baseline", default="tools/recall_baseline.json")
    ap.add_argument("--project", help="只诊断指定工程（逗号分隔）")
    a = ap.parse_args()

    with open(a.cases, encoding="utf-8") as f:
        spec = json.load(f)
    cases = spec["cases"]
    if a.project:
        want = {int(p) for p in a.project.split(",") if p.strip()}
        cases = [c for c in cases if c["project"] in want]
    projects = spec.get("projects", {})

    # Pipeline baseline (to compare "pure cosine hits vs pipeline misses")
    base = {}
    if a.baseline and os.path.exists(a.baseline):
        with open(a.baseline, encoding="utf-8") as f:
            for b in json.load(f):
                base[(b["project"], b["query"])] = b

    db = sqlite3.connect(a.db)
    # Preload: per project (id->name, name->[ids])
    proj_nodes = {}
    proj_vecs = {}

    def load_project(pid):
        if pid in proj_vecs:
            return
        ep = os.path.join(a.embed_dir, f"{pid}.json")
        with open(ep, encoding="utf-8") as f:
            emb = json.load(f)
        vecs = emb["vectors"]
        # Convert to a numpy matrix + id list
        ids = list(vecs.keys())
        mat = np.array([vecs[i] for i in ids], dtype=np.float32)
        # Normalize (already L2 on disk, but just in case)
        norms = np.linalg.norm(mat, axis=1, keepdims=True)
        norms[norms == 0] = 1.0
        mat = mat / norms
        proj_vecs[pid] = (ids, mat)
        # Node name mapping
        name_to_ids = {}
        id_to_name = {}
        for nid, name in db.execute(
            "SELECT id,name FROM nodes WHERE project_id=?", (pid,)
        ):
            id_to_name[str(nid)] = name
            name_to_ids.setdefault(name.lower(), []).append(str(nid))
        proj_nodes[pid] = (name_to_ids, id_to_name)

    # First collect (dedup) and encode the queries each project needs
    queries = {}
    for c in cases:
        queries.setdefault(c["query"], None)
    for q in queries:
        queries[q] = embed(q)

    rows = []
    for c in cases:
        pid = c["project"]
        load_project(pid)
        ids, mat = proj_vecs[pid]
        name_to_ids, id_to_name = proj_nodes[pid]
        qvec = queries[c["query"]]
        # Cosine to all nodes = dot (already normalized)
        sims = mat @ qvec
        order = np.argsort(-sims)
        # True answer node
        true_ids = set()
        for t in c["targets"]:
            tl = t.lower()
            for nm, nids in name_to_ids.items():
                if tl in nm:
                    true_ids.update(nids)
        # Find the best rank of a true answer
        best_rank = None
        best_id = None
        for rank, idx in enumerate(order, 1):
            if ids[idx] in true_ids:
                best_rank = rank
                best_id = ids[idx]
                break
        hit = {k: (best_rank is not None and best_rank <= k) for k in KS}
        rows.append(
            {
                "project": pid,
                "lang": c.get("lang", "zh"),
                "query": c["query"],
                "best_rank": best_rank,
                "best_name": id_to_name.get(best_id, "?") if best_id else "?",
                "true_ids": len(true_ids),
                "hit": hit,
                "top3": [
                    (id_to_name.get(ids[i], "?"), float(sims[i]))
                    for i in order[:3]
                ],
            }
        )

    # ---- print
    print(f"{'proj':<6}{'lang':<4}{'query':<24}{'true-rank':<10}{'@5':<6}{'@10':<6}{'@20':<6} vs(pipeline)")
    print("-" * 110)
    for r in rows:
        br = base.get((r["project"], r["query"]))
        if br:
            bh = {int(k): v for k, v in br["hit"].items()}
            cmp = " ".join(
                f"@{k}{('✓' if bh[k] else '·')}{('/' if r['hit'][k] else '×')}"
                for k in KS
            )
        else:
            cmp = "  (无基线)"
        rank_s = str(r["best_rank"]) if r["best_rank"] else "——"
        print(
            f"#{r['project']:<5}{r['lang']:<4}{r['query'][:22]:<24}"
            f"{rank_s:<10}"
            f"{'✓' if r['hit'][5] else '·':<6}"
            f"{'✓' if r['hit'][10] else '·':<6}"
            f"{'✓' if r['hit'][20] else '·':<6}"
            f" {cmp}"
        )

    print("\n=== pure-cosine summary (by language) ===")
    for lang, label in (("zh", "中文"), ("en", "英文(对照)")):
        rs = [r for r in rows if r["lang"] == lang]
        if not rs:
            continue
        line = "   " + " ".join(
            f"@{k}={sum(1 for r in rs if r['hit'][k])}/{len(rs)}" for k in KS
        )
        print(f"  {label}:{line}")

    # Key diagnostic: of the Chinese misses, how many are "rescuable by pure cosine" (retrieval / ranking bug)
    print("\n=== diagnosis: attribution of the Chinese gap ===")
    zh = [r for r in rows if r["lang"] == "zh"]
    miss20 = [r for r in zh if not r["hit"][20]]
    rescuable = [r for r in miss20 if r["best_rank"] is not None and r["best_rank"] <= 20]
    embedding_gap = [r for r in miss20 if r["best_rank"] is None or r["best_rank"] > 20]
    print(f"  {len(zh)} Chinese cases, {len(miss20)} missed at @20")
    print(f"    -> rescuable by pure cosine (true answer in top 20, retrieval/ranking bug): {len(rescuable)} cases")
    for r in rescuable:
        print(f"        #{r['project']} {r['query']}  true-rank={r['best_rank']} ({r['best_name']})")
    print(f"    -> embedding/bridge gap (true answer not in top 20): {len(embedding_gap)} cases")
    for r in embedding_gap:
        rank_s = str(r["best_rank"]) if r["best_rank"] else "未进索引"
        print(f"        #{r['project']} {r['query']}  true-rank {rank_s} ({r['best_name']})")


if __name__ == "__main__":
    main()
