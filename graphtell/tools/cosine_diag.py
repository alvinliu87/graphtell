#!/usr/bin/env python3
"""余弦诊断：孤立地重放召回流程的「向量种子」一步 —— 把中文/英文查询用 bge 编码后，
对所有已落盘节点向量做纯余弦排序，看「真答案节点」落在第几名。

目的：判定中文召回差距（中文@20≈46%）到底是
  (A) 检索/排序问题：真答案在余弦空间里其实很靠前，只是被 lexical 种子 / BFS 展开 / top-k
      截断「漏掉」了 → 便宜可修（改种子选择、扩展策略）。
  (B) 嵌入问题：真答案在余弦空间里就排不进前 k → 跨语言桥本身不够，要更强桥/更好 doc 文本。

判定口径与 recall_eval.py 完全一致（命中名包含任一 target 即算命中），所以可直接和
recall_baseline.json（流水线结果）对照：
  - 纯余弦命中 但 流水线漏掉  → 检索/排序 bug（便宜修）
  - 纯余弦也漏掉              → 嵌入/桥问题（难）

用法：
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

    # 流水线基线（用于对照「纯余弦命中 vs 流水线漏掉」）
    base = {}
    if a.baseline and os.path.exists(a.baseline):
        with open(a.baseline, encoding="utf-8") as f:
            for b in json.load(f):
                base[(b["project"], b["query"])] = b

    db = sqlite3.connect(a.db)
    # 预加载：每个工程 (id->name, name->[ids])
    proj_nodes = {}
    proj_vecs = {}

    def load_project(pid):
        if pid in proj_vecs:
            return
        ep = os.path.join(a.embed_dir, f"{pid}.json")
        with open(ep, encoding="utf-8") as f:
            emb = json.load(f)
        vecs = emb["vectors"]
        # 转 numpy 矩阵 + id 列表
        ids = list(vecs.keys())
        mat = np.array([vecs[i] for i in ids], dtype=np.float32)
        # 归一化（落盘已是 L2，但保险）
        norms = np.linalg.norm(mat, axis=1, keepdims=True)
        norms[norms == 0] = 1.0
        mat = mat / norms
        proj_vecs[pid] = (ids, mat)
        # 节点名映射
        name_to_ids = {}
        id_to_name = {}
        for nid, name in db.execute(
            "SELECT id,name FROM nodes WHERE project_id=?", (pid,)
        ):
            id_to_name[str(nid)] = name
            name_to_ids.setdefault(name.lower(), []).append(str(nid))
        proj_nodes[pid] = (name_to_ids, id_to_name)

    # 先收集每个工程要用到的查询（去重）并编码
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
        # 所有节点余弦 = dot（已归一化）
        sims = mat @ qvec
        order = np.argsort(-sims)
        # 真答案节点
        true_ids = set()
        for t in c["targets"]:
            tl = t.lower()
            for nm, nids in name_to_ids.items():
                if tl in nm:
                    true_ids.update(nids)
        # 找最佳真答案名次
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

    # ---- 打印
    print(f"{'工程':<6}{'语':<4}{'查询':<24}{'真答案名次':<10}{'@5':<6}{'@10':<6}{'@20':<6} 对照(流水线)")
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

    print("\n=== 纯余弦汇总（按语言）===")
    for lang, label in (("zh", "中文"), ("en", "英文(对照)")):
        rs = [r for r in rows if r["lang"] == lang]
        if not rs:
            continue
        line = "   " + " ".join(
            f"@{k}={sum(1 for r in rs if r['hit'][k])}/{len(rs)}" for k in KS
        )
        print(f"  {label}:{line}")

    # 关键诊断：中文漏掉里，多少是「纯余弦能救」（检索/排序 bug）
    print("\n=== 诊断：中文差距归因 ===")
    zh = [r for r in rows if r["lang"] == "zh"]
    miss20 = [r for r in zh if not r["hit"][20]]
    rescuable = [r for r in miss20 if r["best_rank"] is not None and r["best_rank"] <= 20]
    embedding_gap = [r for r in miss20 if r["best_rank"] is None or r["best_rank"] > 20]
    print(f"  中文用例 {len(zh)} 个，@20 漏掉 {len(miss20)} 个")
    print(f"    → 纯余弦可救（真答案在前20，检索/排序 bug）：{len(rescuable)} 个")
    for r in rescuable:
        print(f"        #{r['project']} {r['query']}  真答案名次={r['best_rank']} ({r['best_name']})")
    print(f"    → 嵌入/桥缺口（真答案不在前20）：{len(embedding_gap)} 个")
    for r in embedding_gap:
        rank_s = str(r["best_rank"]) if r["best_rank"] else "未进索引"
        print(f"        #{r['project']} {r['query']}  真答案{rank_s} ({r['best_name']})")


if __name__ == "__main__":
    main()
