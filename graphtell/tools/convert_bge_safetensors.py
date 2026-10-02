#!/usr/bin/env python3
"""Convert the ModelScope-pulled bge-m3 pytorch weights (pytorch_model.bin) to safetensors
for pure-local loading by Rust `candle` (bypassing ONNX and onnxruntime's openssl dependency).
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

print(">> loading bge-m3 backbone ...", flush=True)
model = AutoModel.from_pretrained(SRC)  # XLMRobertaModel (transformer backbone only)
model.eval()

print(">> writing safetensors ...", flush=True)
sd = model.state_dict()
save_file(sd, os.path.join(DST_DIR, "model.safetensors"))
print(f"   tensors: {len(sd)}")

# candle also needs config.json and tokenizer.json to load
shutil.copy(os.path.join(SRC, "config.json"), os.path.join(DST_DIR, "config.json"))
shutil.copy(os.path.join(SRC, "tokenizer.json"), os.path.join(DST_DIR, "tokenizer.json"))
print(f">> done: {DST_DIR}")
for f in ("model.safetensors", "config.json", "tokenizer.json"):
    p = os.path.join(DST_DIR, f)
    print(f"   {f}: {os.path.getsize(p)/1e6:.1f} MB")
