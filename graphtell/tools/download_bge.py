#!/usr/bin/env python3
"""MAINTAINER TOOL — convert BAAI/bge-m3 into candle-ready safetensors for a GitHub Releases bundle.

This is NOT run by end users. The GraphTell backend now downloads a pre-built
`bge-m3-safetensors.zip` directly (see `gt-adapter-http`'s `model_manager.rs`); this script is the
one-time step that produces that zip on the maintainer's machine.

    pip install torch sentence-transformers modelscope safetensors

It writes `models/bge-m3-safetensors/{model.safetensors,config.json,tokenizer.json,...}`.
Then package it for release with `tools/make_bge_release.py` (or just
`cd models && zip -r bge-m3-safetensors.zip bge-m3-safetensors`) and upload the zip to
GitHub Releases as the `bge-m3` tag. The bge-m3 weights remain under the original BAAI (MIT) license.

Progress is reported as `STAGE:<name>` lines on stdout.
"""
import os
import sys
import shutil
import argparse


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--models-dir", default="models", help="directory (relative to repo root) to write into")
    ap.add_argument("--repo", default="BAAI/bge-m3", help="ModelScope repo id")
    ap.add_argument("--fp16", action="store_true",
                    help="store weights as fp16 to halve the bundle size (~1.1GB vs ~2.2GB). "
                         "candle upcasts fp16 -> f32 on load, so quality is unaffected.")
    args = ap.parse_args()

    # Locate the repo root: when this script is an embedded copy inside `data/`, walk up to the dir
    # that contains `tools/`; otherwise fall back to the current working directory.
    here = os.path.dirname(os.path.abspath(__file__))
    root = here
    while root != os.path.dirname(root) and not os.path.isdir(os.path.join(root, "tools")):
        root = os.path.dirname(root)
    if not os.path.isdir(os.path.join(root, "tools")):
        root = os.getcwd()

    ms_dir = os.path.join(root, args.models_dir, "bge-m3-ms")
    dst_dir = os.path.join(root, args.models_dir, "bge-m3-safetensors")
    os.makedirs(dst_dir, exist_ok=True)

    try:
        import torch
        from safetensors.torch import save_file
        from transformers import AutoModel
    except Exception as e:  # pragma: no cover - depends on host environment
        print(f"STAGE:error:missing dependencies: {e}")
        print("Please install once: pip install torch sentence-transformers modelscope safetensors")
        return 2

    # 1) download raw weights via ModelScope (git-lfs, bypasses the HF Xet wall).
    print("STAGE:downloading", flush=True)
    try:
        from modelscope import snapshot_download

        snapshot_download(
            args.repo,
            local_dir=ms_dir,
            allow_patterns=[
                "pytorch_model.bin",
                "config.json",
                "tokenizer.json",
                "tokenizer_config.json",
                "special_tokens_map.json",
                "sentencepiece.bpe.model",
                "1_Pooling/*",
            ],
        )
    except Exception as e:  # pragma: no cover
        print(f"STAGE:error:download failed: {e}", flush=True)
        return 3

    # 2) convert pytorch_model.bin -> model.safetensors (candle reads safetensors directly; upstream only ships .bin).
    print("STAGE:converting", flush=True)
    try:
        model = AutoModel.from_pretrained(ms_dir)
        model.eval()
        sd = model.state_dict()
        if args.fp16:
            # Halve the on-disk / download size. Only float tensors are cast; integer buffers
            # (e.g. position_ids) must stay as-is. candle upcasts fp16 -> f32 on load.
            sd = {k: (v.half() if v.is_floating_point() else v) for k, v in sd.items()}
        save_file(sd, os.path.join(dst_dir, "model.safetensors"))
        for f in (
            "config.json",
            "tokenizer.json",
            "tokenizer_config.json",
            "special_tokens_map.json",
            "sentencepiece.bpe.model",
        ):
            src = os.path.join(ms_dir, f)
            if os.path.exists(src):
                shutil.copy(src, os.path.join(dst_dir, f))
    except Exception as e:  # pragma: no cover
        print(f"STAGE:error:convert failed: {e}", flush=True)
        return 4

    print("STAGE:done", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
