#!/usr/bin/env python3
"""把经 ModelScope 拉取的 bge-m3 pytorch 权重导出为 ONNX（供 Rust `tract` 加载）。

导出 XLM-RoBERTa backbone 的 last_hidden_state。关键点：`position_ids` 作为显式输入
传入，避免 ONNX 内部生成 `Range` 节点（tract 对 int64 的 Range 类型推断会失败）。
Rust 侧取 [CLS]（首 token）做句向量并 L2 归一化，与 bge 官方用法一致。
"""
import os
import shutil
import torch
from transformers import AutoModel, AutoTokenizer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "models", "bge-m3-ms")
DST_DIR = os.path.join(ROOT, "models", "bge-m3-onnx")
DST = os.path.join(DST_DIR, "model.onnx")

# 清掉旧的外部数据文件，避免脏数据残留
if os.path.isdir(DST_DIR):
    shutil.rmtree(DST_DIR)
os.makedirs(DST_DIR, exist_ok=True)

print(">> 加载 bge-m3 backbone ...", flush=True)
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

print(">> 导出 ONNX ...", flush=True)
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
print(f">> 已导出: {DST} ({os.path.getsize(DST)/1e6:.1f} MB)")
