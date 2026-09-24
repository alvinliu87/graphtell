#!/usr/bin/env python3
"""用真实 bge-m3（本地 CPU 推理）演示「中文意图 -> 英文代码符号」语义向量召回。

权重经 ModelScope 拉取（git-lfs，绕开 HF 的 Xet 墙）。依赖：torch(CPU) +
sentence-transformers + modelscope。
"""
import os
import numpy as np
from modelscope import snapshot_download
from sentence_transformers import SentenceTransformer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MS_DIR = os.path.join(ROOT, "models", "bge-m3-ms")  # 经 modelscope 拉取的本地权重

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

    # 候选「业务节点」文本（CRMEB 风格代码，英文命名）
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
    # 节点（文档侧）：不加查询前缀
    node_vecs = {k: model.encode(v, normalize_embeddings=True, convert_to_numpy=True)
                 for k, v in nodes.items()}

    queries = ["下单改优惠", "用户登录", "订单列表", "清理无用日志"]
    for q in queries:
        # 查询侧：加 bge 检索前缀
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
