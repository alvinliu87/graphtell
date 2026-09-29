#!/usr/bin/env bash
#
# 生成 GraphTell 的 GitHub 展示 demo：对每个样本工程跑建图 + 规则检测 + 召回示例，
# 把结果落盘成 GitHub 原生可渲染的 Markdown（表格 / 召回上下文包），放进 `docs/demo/`。
#
# 设计要点：
#   * 只依赖**已有 CLI**（`create` / `stats` / `check --json` / `recall --markdown`），
#     不引入新命令，也不需要在仓库里提交 13G 的模型权重——召回走哈希兜底即可演示。
#   * 样本（`samples/`）是仓库内受跟踪的目录，但不进发布包；本脚本在「完整源码检出」
#     里跑（和 CI / 本地开发一致）。样本缺失的测试会软跳过，这里同理：样本不在就跳过该样本。
#   * 产物是纯静态 MD，推到默认分支即可在 GitHub 渲染；想做可交互图（graph.json + 静态
#     查看器）可后续加一个 `export --json` 命令，本脚本已预留 `graph.json` 写入位。
#
# 用法：
#   ./tools/gen_demo.sh                 # 用已存在的 target/release|debug/graphtell
#   ./tools/gen_demo.sh --build         # 先以 --no-default-features 编译（便携、无需模型）
#   GT_BINARY=/path/to/graphtell ./tools/gen_demo.sh
#   DEMO_ONLY=frontend-backend-link ./tools/gen_demo.sh   # 只跑单个样本（快速验证）
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${GT_BINARY:-}"
if [[ -z "$BIN" ]]; then
  if [[ -x target/release/graphtell ]]; then BIN=target/release/graphtell
  elif [[ -x target/debug/graphtell ]]; then BIN=target/debug/graphtell
  fi
fi
if [[ "${1:-}" == "--build" ]]; then
  echo "==> 编译 graphtell (--no-default-features) ..."
  cargo build -p gt-app --no-default-features
  BIN=target/debug/graphtell
fi
if [[ -z "$BIN" || ! -x "$BIN" ]]; then
  echo "找不到 graphtell 二进制。请先 \`cargo build -p gt-app\` 或传 GT_BINARY=... 或加 --build" >&2
  exit 1
fi

DATA="$(mktemp -d "${TMPDIR:-/tmp}/graphtell-demo.XXXXXX")"
trap 'rm -rf "$DATA"' EXIT
OUT="$ROOT/docs/demo"
mkdir -p "$OUT"

# 样本清单：slug | 样本路径（相对仓库根）| 一句话简介
# 注：CRMEB / Bagisto 这类大型第三方工程不随仓库分发（授权 + 体积），需用
# `GRAPHTELL_SAMPLE_DIR` 自行提供；它们不在时本脚本自动跳过，与测试软跳过一致。
SAMPLES=(
  "frontend-backend-link|samples/frontend-backend-link|前端 + 后端小示例：演示跨端调用链路与表读写"
  "nestjs-realworld-example-app|samples/nestjs-realworld-example-app|NestJS + TypeScript 真实项目"
  "hackathon-starter|samples/hackathon-starter|Node/Express 起步项目"
  "typescript-starter|samples/typescript-starter|TypeScript 起步项目"
  "php-laravel-starter|samples/php-projects/laravel-starter|Laravel 起步项目"
)

# 可选：只跑某个样本（按 slug 前缀过滤），用于快速验证。
if [[ -n "${DEMO_ONLY:-}" ]]; then
  filtered=()
  for e in "${SAMPLES[@]}"; do
    [[ "$e" == "${DEMO_ONLY}|"* ]] && filtered+=("$e")
  done
  SAMPLES=("${filtered[@]}")
fi

# 每个样本的演示召回问句（中文，验证「中文意图 → 英文节点」桥接）
declare -A QUERIES=(
  ["frontend-backend-link"]="订单创建流程|用户登录入口"
  ["nestjs-realworld-example-app"]="用户注册与鉴权|文章发布的接口"
  ["hackathon-starter"]="用户登录的实现|发送邮件的逻辑"
  ["typescript-starter"]="核心工具函数|配置文件加载"
  ["php-laravel-starter"]="用户注册与登录|路由定义"
)

# 把 check --json 的报告（对象，含 violations 数组）转成 Markdown 表格；无违规则给一行说明。
json_violations_to_md() {
  python3 - "$1" <<'PY'
import json,sys
try:
    data=json.load(open(sys.argv[1]))
except Exception:
    data={}
rows = data.get("violations") if isinstance(data, dict) else data
sev = data.get("by_severity") if isinstance(data, dict) else None
if sev:
    s = "、".join(f"{k} {v}" for k,v in sev.items())
    print(f"> 共 {len(rows)} 条违规（{s}）。\n")
if not rows:
    print("> 未发现违规。")
    sys.exit(0)
print("| 规则 | 严重度 | 位置 | 信息 |")
print("| --- | --- | --- | --- |")
for r in rows[:50]:
    rid=r.get("rule_id") or "-"
    sv=r.get("severity") or "-"
    loc="%s:%s" % (r.get("file") or r.get("node_name") or "-", r.get("line") or "?")
    msg=(r.get("message") or "").replace("|","\\|").replace("\n"," ")
    print(f"| `{rid}` | {sv} | {loc} | {msg} |")
if len(rows)>50:
    print(f"\n> …共 {len(rows)} 条，仅展示前 50。")
PY
}

# 返回违规条数（报告对象取 violations 数组长度）。
count_violations() {
  python3 - "$1" <<'PY'
import json,sys
try:
    d=json.load(open(sys.argv[1]))
except Exception:
    print(0); sys.exit(0)
v = d.get("violations") if isinstance(d, dict) else d
print(len(v) if v else 0)
PY
}

# 把 stats 的「节点类型计数」段抽成 Markdown 表格。
stats_to_md() {
  # stats 输出形如：  Table             12  /  Method           34
  awk '/^[[:space:]]+[A-Za-z]+[[:space:]]+[0-9]+[[:space:]]*$/{
         gsub(/^[[:space:]]+/,""); n=split($0,a,/[[:space:]]+/);
         kind=a[1]; cnt=a[n]; printf("| %s | %s |\n", kind, cnt);
       }' "$1"
}

GALLERY="# GraphTell 示例画廊\n"
GALLERY+="\n> 自动生成（\`tools/gen_demo.sh\`）。每个样本在本地跑建图 + 规则检测 + 召回示例，\n"
GALLERY+="> 无需模型权重（召回走哈希兜底）。源码样本见仓库 \`samples/\`。\n\n"
GALLERY+="| 样本 | 简介 | 图规模 | 规则违规 | 召回示例 |\n| --- | --- | --- | --- | --- |\n"

for entry in "${SAMPLES[@]}"; do
  IFS='|' read -r slug path desc <<< "$entry"
  sample_dir="$ROOT/$path"
  if [[ ! -d "$sample_dir" ]]; then
    echo "==> 跳过（样本不存在）: $path"
    continue
  fi
  echo "==> 处理 $slug ($path)"
  name="demo-$slug"
  # 建图
  create_out="$("$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" create --name "$name" --path "$sample_dir" 2>&1)"
  pid="$(echo "$create_out" | grep -oE '#[0-9]+' | head -1 | tr -d '#')"
  if [[ -z "$pid" ]]; then
    echo "    建图未返回工程 id，跳过。输出：$create_out" >&2
    continue
  fi
  sdir="$OUT/$slug"
  mkdir -p "$sdir"

  # 图规模
  stats_out="$("$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" stats --project "$pid" 2>&1)"
  echo "$stats_out" > "$sdir/stats.txt"
  {
    echo "# $slug"
    echo
    echo "$desc"
    echo
    echo "源码样本：[\`$path\`](../../$path)"
    echo
    echo "## 图规模"
    echo
    echo "| 节点类型 | 数量 |"
    echo "| --- | --- |"
    stats_to_md "$sdir/stats.txt"
  } > "$sdir/README.md"

  # 规则检测结果
  check_json="$("$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" check --project "$pid" --json 2>/dev/null || echo '[]')"
  echo "$check_json" > "$sdir/violations.json"
  {
    echo
    echo "## 规则检测结果"
    echo
    json_violations_to_md "$sdir/violations.json"
  } >> "$sdir/README.md"

  # 召回示例
  q="${QUERIES[$slug]:-}"
  if [[ -n "$q" ]]; then
    {
      echo
      echo "## 提示词增强（召回）示例"
      echo
      echo "> 以下为中文问句经 GraphTell 召回出的相关代码上下文包（Markdown）。"
      echo
      IFS='|' read -ra qs <<< "$q"
      for query in "${qs[@]}"; do
        echo "### 「$query」"
        echo
        "$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" recall --project "$pid" --query "$query" --markdown --limit 5 2>/dev/null || echo "（召回失败）"
        echo
      done
    } >> "$sdir/README.md"
  fi

  # 画廊行
  n_violations="$(count_violations "$sdir/violations.json")"
  GALLERY+="| [$slug](./$slug/README.md) | $desc | [stats](./$slug/README.md#图规模) | $n_violations 条 | [召回](./$slug/README.md#提示词增强召回示例) |\n"
done

echo -e "$GALLERY" > "$OUT/README.md"

# 把生成物里的绝对仓库路径收敛成相对路径（避免把本地绝对路径泄露进公开 demo）。
find "$OUT" \( -name '*.md' -o -name '*.txt' -o -name '*.json' \) -exec sed -i "s|${ROOT}/||g" {} +

echo "==> 完成，产物在 $OUT/"
