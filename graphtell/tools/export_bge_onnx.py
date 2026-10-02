#!/usr/bin/env python3
"""Export the ModelScope-pulled bge-m3 pytorch weights to ONNX (for Rust `tract` loading).

Exports the XLM-RoBERTa backbone's last_hidden_state. Key point: `position_ids` is passed as an explicit
input to avoid ONNX generating an internal `Range` node (tract fails type inference on int64 Range).
The Rust side takes [CLS] (first token) as the sentence vector and L2-normalizes it, matching bge's official usage.
"""
import os
import shutil
import torch
from transformers import AutoModel, AutoTokenizer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "models", "bge-m3-ms")
DST_DIR = os.path.join(ROOT, "models", "bge-m3-onnx")
DST = os.path.join(DST_DIR, "model.onnx")

# Clear old external-data files to avoid stale leftovers
if os.path.isdir(DST_DIR):
    shutil.rmtree(DST_DIR)
os.makedirs(DST_DIR, exist_ok=True)

print(">> loading bge-m3 backbone ...", flush=True)
tok = AutoTokenizer.from_pretrained(SRC)
model = AutoModel.from_pretrained(SRC)
model.eval()


class Wrapper(torch.nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, input_ids, attention_mask, token_type_ids, position_ids):
        out = self.m(
            input_ids=input_ids,
            attention_mask=attention_mask,
            token_type_ids=token_type_ids,
            position_ids=position_ids,
        )
        return out.last_hidden_state


w = Wrapper(model)
seq = 16
input_ids = torch.arange(0, seq, dtype=torch.long).unsqueeze(0) % 1000 + 1
attention_mask = torch.ones(1, seq, dtype=torch.long)
token_type_ids = torch.zeros(1, seq, dtype=torch.long)
position_ids = torch.arange(0, seq, dtype=torch.long).unsqueeze(0)

print(">> exporting ONNX ...", flush=True)
torch.onnx.export(
    w,
    (input_ids, attention_mask, token_type_ids, position_ids),
    DST,
    input_names=["input_ids", "attention_mask", "token_type_ids", "position_ids"],
    output_names=["last_hidden_state"],
    dynamic_axes={
        "input_ids": {0: "batch", 1: "seq"},
        "attention_mask": {0: "batch", 1: "seq"},
        "token_type_ids": {0: "batch", 1: "seq"},
        "position_ids": {0: "batch", 1: "seq"},
        "last_hidden_state": {0: "batch", 1: "seq"},
    },
    opset_version=14,
    do_constant_folding=True,
    dynamo=False,
)
print(f">> exported: {DST} ({os.path.getsize(DST)/1e6:.1f} MB)")
