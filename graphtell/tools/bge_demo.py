#!/usr/bin/env python3
"""Demonstrate semantic-vector recall of "Chinese intent -> English code symbol" with real bge-m3 (local CPU inference).

Weights are pulled via ModelScope (git-lfs, bypassing HF's Xet wall). Dependencies: torch(CPU) +
sentence-transformers + modelscope.
"""
import os
import numpy as np
from modelscope import snapshot_download
from sentence_transformers import SentenceTransformer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MS_DIR = os.path.join(ROOT, "models", "bge-m3-ms")  # Local weights pulled via modelscope

QUERY_PREFIX = "Represent this sentence for searching relevant passages: "


def ensure_model():
    if os.path.exists(os.path.join(MS_DIR, "pytorch_model.bin")) or \
       os.path.exists(os.path.join(MS_DIR, "model.safetensors")):
        return MS_DIR
    print(">> 经 ModelScope 下载 bge-m3（git-lfs，绕开 Xet 墙）...", flush=True)
    return snapshot_download(
        "BAAI/bge-m3",
        local_dir=MS_DIR,
        allow_patterns=["*.json", "*.bin", "*.safetensors", "*.model", "*.txt"],
    )


def cos(a, b):
    return float(np.dot(a, b) / (np.linalg.norm(a) * np.linalg.norm(b)))


def main():
    path = ensure_model()
    print(">> 加载 bge-m3 ...", flush=True)
    model = SentenceTransformer(path)

    # Candidate "business node" texts (CRMEB-style code, English naming)
    nodes = {
        "placeOrder (Method)": "placeOrder",
        "applyDiscount (Method)": "applyDiscount",
        "OrderService (Class)": "OrderService",
        "getOrderList (Method)": "getOrderList",
        "createOrder (Method)": "createOrder",
        "discountCoupon (Method)": "discountCoupon",
        "userLogin (Method)": "userLogin",
        "unused_log (noise)": "unused_log",
        "configCacheScheme (noise)": "configCacheScheme",
        "redisSet (noise)": "redisSet",
    }
    # Nodes (document side): no query prefix
    node_vecs = {k: model.encode(v, normalize_embeddings=True, convert_to_numpy=True)
                 for k, v in nodes.items()}

    queries = ["下单改优惠", "用户登录", "订单列表", "清理无用日志"]
    for q in queries:
        # Query side: add the bge retrieval prefix
        qv = model.encode(QUERY_PREFIX + q, normalize_embeddings=True, convert_to_numpy=True)
        ranked = sorted(
            ((name, cos(qv, v)) for name, v in node_vecs.items()),
            key=lambda x: -x[1],
        )
        print(f"\n查询: {q}")
        for name, s in ranked[:5]:
            print(f"  {s:+.4f}  {name}")
    print("\nDONE")


if __name__ == "__main__":
    main()
