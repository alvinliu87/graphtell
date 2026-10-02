#!/usr/bin/env bash
#
# Package the GraphTell release artifacts (no Docker): build the frontend + backend release binary,
# assemble them into a directory and tar it up. Output goes to ./release/.
#
# Usage:
#   ./scripts/package-release.sh            # default: candle/ort default features on
#   GT_FEATURES="--no-default-features" ./scripts/package-release.sh   # lexical only, smaller
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
OUT="release/graphtell-$VERSION"
GT_FEATURES="${GT_FEATURES:---}"

echo "==> 版本：$VERSION  输出目录：$OUT"

# 1) frontend
echo "==> 构建前端 UI (VITE_API_BASE=same-origin)"
cd "$ROOT/ui"
npm ci
VITE_API_BASE=same-origin npm run build
cd "$ROOT"

# 2) backend
echo "==> 构建后端 release 二进制 (cargo build --release $GT_FEATURES -p gt-app)"
cargo build --release $GT_FEATURES -p gt-app

# 3) assemble
echo "==> 组装发布目录"
rm -rf "$OUT"
mkdir -p "$OUT"
cp "target/release/graphtell" "$OUT/graphtell"
cp -r ui/dist "$OUT/ui"
cp -r fkb rules views "$OUT/"

cat > "$OUT/README.txt" <<EOF
GraphTell $VERSION —— 发布包

启动（前端由后端同源托管，单端口）：
  ./graphtell --data-dir ./data serve --bind 0.0.0.0 --port 5177 --ui-dir ./ui

然后浏览器打开 http://<host>:5177/

说明：
- 默认编译开启 model-candle / model-ort：若本地有 bge-m3 权重（GT_BGE_MODEL，默认 models/bge-m3-safetensors）
  则召回走真实语义向量；权重缺失时自动退回本地哈希编码器（离线、零依赖、不调 LLM）。
- fkb/ rules/ views/ 是内置框架知识、检查规则与视角声明，已随包提供。
- data/ 存放 SQLite 与落盘向量，建议挂载持久卷。
EOF

# 4) package
TARBALL="release/graphtell-$VERSION.tar.gz"
tar -czf "$TARBALL" -C release "graphtell-$VERSION"
echo "==> 完成：$TARBALL"
echo "    解压后执行其中 README.txt 的启动命令即可。"
