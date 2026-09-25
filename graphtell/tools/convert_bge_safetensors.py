#!/usr/bin/env python3
"""把经 ModelScope 拉取的 bge-m3 pytorch 权重（pytorch_model.bin）转 safetensors，
供 Rust `candle` 纯本地加载（绕开 ONNX 与 onnxruntime 的 openssl 依赖）。
"""
import os
import shutil
import torch
from safetensors.torch import save_file
from transformers import AutoModel, AutoTokenizer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "models", "bge-m3-ms")
DST_DIR = os.path.join(ROOT, "models", "bge-m3-safetensors")
os.makedirs(DST_DIR, exist_ok=True)

print(">> 加载 bge-m3 backbone ...", flush=True)
model = AutoModel.from_pretrained(SRC)  # XLMRobertaModel（仅 transformer 主干）
model.eval()

print(">> 写出 safetensors ...", flush=True)
sd = model.state_dict()
save_file(sd, os.path.join(DST_DIR, "model.safetensors"))
print(f"   tensors: {len(sd)}")

# candle 加载还要 config.json 与 tokenizer.json
shutil.copy(os.path.join(SRC, "config.json"), os.path.join(DST_DIR, "config.json"))
shutil.copy(os.path.join(SRC, "tokenizer.json"), os.path.join(DST_DIR, "tokenizer.json"))
print(f">> 完成: {DST_DIR}")
for f in ("model.safetensors", "config.json", "tokenizer.json"):
    p = os.path.join(DST_DIR, f)
    print(f"   {f}: {os.path.getsize(p)/1e6:.1f} MB")
