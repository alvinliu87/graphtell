#!/usr/bin/env python3
"""Verify the exported bge-m3 ONNX matches the sentence-transformers output (same tokens + CLS + L2)."""
import os
import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer
from sentence_transformers import SentenceTransformer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ONNX = os.path.join(ROOT, "models", "bge-m3-onnx", "model.onnx")
TOK = os.path.join(ROOT, "models", "bge-m3", "tokenizer.json")
MS = os.path.join(ROOT, "models", "bge-m3-ms")
PREFIX = "Represent this sentence for searching relevant passages: "


def enc_onnx(text):
    tok = Tokenizer.from_file(TOK)
    enc = tok.encode(text)
    ids = enc.ids
    n = len(ids)
    feeds = {
        "input_ids": np.array([ids], dtype=np.int64),
        "attention_mask": np.ones((1, n), dtype=np.int64),
        "token_type_ids": np.zeros((1, n), dtype=np.int64),
    }
    sess = ort.InferenceSession(ONNX, providers=["CPUExecutionProvider"])
    out = sess.run(None, feeds)
    vec = out[0][0, 0, :].astype(np.float32)
    return vec / np.linalg.norm(vec)


def cos(a, b):
    return float(np.dot(a, b) / (np.linalg.norm(a) * np.linalg.norm(b)))


st = SentenceTransformer(MS)
q = PREFIX + "下单改优惠"
a = enc_onnx(q)
b = st.encode(q, normalize_embeddings=True, convert_to_numpy=True)
print("query onnx vs st cosine:", cos(a, b))

for t in ["applyDiscount", "placeOrder", "userLogin", "unused_log"]:
    o = enc_onnx(t)
    s = st.encode(t, normalize_embeddings=True, convert_to_numpy=True)
    print(f"  {t:14s} cosine(onnx,st) = {cos(o, s):.4f}")
print("VALIDATE_DONE")
