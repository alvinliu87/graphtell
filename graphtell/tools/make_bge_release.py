#!/usr/bin/env python3
"""MAINTAINER TOOL — build the `bge-m3-safetensors.zip` release bundle.

Runs `download_bge.py` (which fetches + converts BAAI/bge-m3 into candle-ready safetensors),
then zips the produced directory so it can be uploaded to GitHub Releases.

Usage:
    python3 tools/make_bge_release.py                # writes models/bge-m3-safetensors.zip
    python3 tools/make_bge_release.py --out /tmp/x.zip

The resulting zip's top-level directory (`bge-m3-safetensors/`) is auto-stripped on extraction by
the backend, so the layout inside the zip is irrelevant — but we keep it for clarity.
"""
import argparse
import os
import shutil
import subprocess
import sys
import zipfile


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--models-dir", default="models", help="directory (relative to repo root) to read/write into")
    ap.add_argument("--out", default=None, help="output zip path (default: <models-dir>/bge-m3-safetensors.zip)")
    ap.add_argument("--fp16", action=argparse.BooleanOptionalAction, default=True,
                    help="store weights as fp16 to halve the bundle size (default: on; pass --no-fp16 to keep f32)")
    args = ap.parse_args()

    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    src_dir = os.path.join(root, args.models_dir, "bge-m3-safetensors")
    out_zip = args.out or os.path.join(root, args.models_dir, "bge-m3-safetensors.zip")

    # Convert (fetches from ModelScope if the safetensors are not already present).
    convert_cmd = [sys.executable, os.path.join(here, "download_bge.py"), "--models-dir", args.models_dir]
    if args.fp16:
        convert_cmd.append("--fp16")
    rc = subprocess.call(convert_cmd)
    if rc != 0:
        print(f"[error] download_bge.py failed with exit code {rc}", file=sys.stderr)
        return rc

    if not os.path.isfile(os.path.join(src_dir, "model.safetensors")):
        print(f"[error] expected {src_dir}/model.safetensors not found", file=sys.stderr)
        return 1

    # Zip it (deflate). Include a single top-level directory for clarity.
    print(f"[ok] zipping {src_dir} -> {out_zip}", flush=True)
    if os.path.exists(out_zip):
        os.remove(out_zip)
    with zipfile.ZipFile(out_zip, "w", zipfile.ZIP_DEFLATED) as zf:
        for base, _dirs, files in os.walk(src_dir):
            for name in files:
                full = os.path.join(base, name)
                arc = os.path.relpath(full, os.path.dirname(src_dir))  # -> bge-m3-safetensors/...
                zf.write(full, arc)

    size_mb = os.path.getsize(out_zip) / (1024 * 1024)
    print(f"[ok] wrote {out_zip} ({size_mb:.1f} MB)")
    print("Next: upload this zip to GitHub Releases (tag `bge-m3`). Override the default URL with "
          "GT_BGE_DOWNLOAD_URL if you host it elsewhere.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
