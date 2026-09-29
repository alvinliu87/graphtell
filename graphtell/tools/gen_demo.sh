#!/usr/bin/env bash
#
# 生成 GraphTell 的公开 demo：对每个样本工程跑「建图 → 规则检测 → 召回示例 → 图导出」，
# 产出可供 GitHub Pages 直接托管的静态站点数据（docs/demo/data/）与 GitHub 原生可渲染的
# Markdown 画廊（docs/demo/README.md）。
#
# 设计要点：
#   * 只依赖已有 CLI（create / stats / check --json / recall --markdown / export）。
#     不提交 13G 模型权重 —— 召回走哈希兜底即可演示。
#   * 样本树已合并到**仓库根 samples/**（内层旧的 samples/ 已删除）。可用
#     GRAPHTELL_SAMPLES_DIR 覆盖。样本缺失即跳过，与集成测试的软依赖一致。
#   * 大工程（CRMEB 约 9.6 万节点）的**全量图不能直接塞给浏览器**：这里按“有意思的
#     节点类型”筛选并截断到 MAX_NODES，只导出子图；完整规模写在 meta 里如实标注。
#   * 产物是纯静态文件，推到默认分支即可在 GitHub 渲染；配合
#     .github/workflows/deploy-demo.yml 可发布到 GitHub Pages（免域名）。
#
# 用法：
#   ./tools/gen_demo.sh                 # 用已存在的 target/release|debug/graphtell
#   ./tools/gen_demo.sh --build         # 先以 --no-default-features 编译（便携、无需模型）
#   GT_BINARY=/path/to/graphtell ./tools/gen_demo.sh
#   DEMO_ONLY=express ./tools/gen_demo.sh            # 只跑单个样本
#   GRAPHTELL_SAMPLES_DIR=/path/to/samples ./tools/gen_demo.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
REPO_ROOT="$(cd "$ROOT/.." && pwd)"

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
# 能力校验：`export` 子命令是后加的，旧的 release 二进制没有它。若直接拿来用，
# clap 会以退出码 2 报“未知子命令”，而本脚本把子命令的 stderr 丢进了 /dev/null，
# 结果就是**静默失败**（只见脚本中断、不见原因）。这里先显式校验，把问题说清楚。
if ! "$BIN" export --help >/dev/null 2>&1; then
  echo "\`$BIN\` 不支持 export 子命令（多半是旧二进制）。请加 --build 重新编译，或用 GT_BINARY 指向新二进制。" >&2
  exit 1
fi

# 样本树位置：优先显式指定，否则用仓库根 samples/（合并后的唯一位置）。
SAMPLES_DIR="${GRAPHTELL_SAMPLES_DIR:-}"
if [[ -z "$SAMPLES_DIR" ]]; then
  if [[ -d "$REPO_ROOT/samples" ]]; then SAMPLES_DIR="$REPO_ROOT/samples"; else SAMPLES_DIR="$ROOT/samples"; fi
fi
echo "==> 样本树：$SAMPLES_DIR"

DATA="$(mktemp -d "${TMPDIR:-/tmp}/graphtell-demo.XXXXXX")"
trap 'rm -rf "$DATA"' EXIT
OUT="$ROOT/docs/demo"
# 注意：产物**不能**放进 `data/` 子目录 —— 仓库 .gitignore 有 `**/data/`（用于运行时数据），
# 会把它们静默吞掉（而按 git 规范，父目录被排除后子路径无法再用 `!` 重新包含）。
# 因此每个样本直接落在 `docs/demo/<slug>/` 下。
mkdir -p "$OUT"
MAX_NODES="${DEMO_MAX_NODES:-400}"

# 样本清单：slug | 相对样本树的路径 | 简介 | 许可证 | 上游
# 只选许可证清晰的项目（宽松许可 + 自造夹具），署名会写进 demo 页面。
SAMPLES=(
  "frontend-backend-link|frontend-backend-link|自造合成夹具：前端 ↔ 后端跨端链路|本项目自有|—"
  "express|node-projects/express|Express 起步项目（Node）|MIT|https://github.com/expressjs/express"
  "litemall|java-projects/litemall|litemall 电商系统（Java/SpringBoot）|MIT|https://github.com/linlinjava/litemall"
  "bagisto|php-projects/laravel/bagisto|Bagisto 电商系统（PHP/Laravel）|MIT|https://github.com/bagisto/bagisto"
  "CRMEB|php-projects/thinkphp/CRMEB|CRMEB 商城（PHP/ThinkPHP）|Apache-2.0|https://github.com/crmeb/CRMEB"
)

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
  ["express"]="中间件处理逻辑|路由定义"
  ["litemall"]="订单支付流程|商品库存扣减"
  ["bagisto"]="购物车结算|商品分类查询"
  ["CRMEB"]="订单支付流程|商品库存扣减|用户优惠券"
)

# 把 check --json 的报告（对象，含 violations 数组）转成 Markdown 表格。
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
  awk '/^[[:space:]]+[A-Za-z]+[[:space:]]+[0-9]+[[:space:]]*$/{
         gsub(/^[[:space:]]+/,""); n=split($0,a,/[[:space:]]+/);
         kind=a[1]; cnt=a[n]; printf("| %s | %s |\n", kind, cnt);
       }' "$1"
}

# 把 export 的全量图降采样成浏览器可承受的子图 + meta。
# 用法：downsample_graph <full.json> <outdir> <max_nodes>
downsample_graph() {
  python3 - "$1" "$2" "$3" <<'PY'
import json,sys,collections
src, outdir, maxn = sys.argv[1], sys.argv[2], int(sys.argv[3])
d = json.load(open(src))
nodes, edges, files = d.get("nodes", []), d.get("edges", []), d.get("files", {})
total_nodes, total_edges = len(nodes), len(edges)

# FILE / Namespace 这类结构节点不进演示图：数量大且只会把图撑成一团散点。
SKIP = {"File", "Namespace"}
allowed = {n.get("id") for n in nodes if n.get("kind") not in SKIP}

# 无向邻接，供 BFS 取连通子图。
adj = collections.defaultdict(list)
for e in edges:
    f, t = e.get("from_id"), e.get("to_id")
    if f in allowed and t in allowed:
        adj[f].append(t)
        adj[t].append(f)

# 种子：演示价值高的类型优先，保证子图里都是“有语义”的节点。
SEED = ["HttpContract","Table","Class","Service","Dao","Method","Cache","Queue",
        "EventBus","Schedule","Page","Store","ConfigKey","Module","Function"]
by_kind = collections.defaultdict(list)
for n in nodes:
    if n.get("id") in allowed:
        by_kind[n.get("kind", "")].append(n)
seeds = []
for k in SEED:
    seeds.extend(by_kind.get(k, []))

# 从种子 BFS 扩张到 maxn 为止 —— 得到的是**连通**子图。
# （按 id 直接截断会取到互不相干的一盘散沙：实测 CRMEB 400 节点只有 0 条边。）
from collections import deque
seen, queue = set(), deque()
# 种子只占一部分预算：CRMEB 光 HttpContract 就有 1603 个，若让种子直接铺满 maxn，
# BFS 就永远没机会扩张，结果是一袋互不相干的契约节点（实测 400 节点 / 0 条边）。
# 留出 ~3/4 预算给 BFS，把连接它们的中间节点（CallSite / Method / Dao …）带进来。
seed_budget = max(1, maxn // 4)
for s in seeds:
    if len(seen) >= seed_budget:
        break
    if s["id"] not in seen:
        seen.add(s["id"])
        queue.append(s["id"])
while queue and len(seen) < maxn:
    u = queue.popleft()
    for v in adj[u]:
        if v not in seen:
            seen.add(v)
            queue.append(v)
            if len(seen) >= maxn:
                break

kept = [n for n in nodes if n.get("id") in seen]
ids = seen
sub_edges = [e for e in edges if e.get("from_id") in ids and e.get("to_id") in ids]

def slim(n):
    return {
        "id": n.get("id"),
        "kind": n.get("kind"),
        "name": n.get("name"),
        "fqn": n.get("fqn"),
        "file": files.get(str(n.get("file_id"))) if n.get("file_id") is not None else None,
    }

graph = {
    "nodes": [slim(n) for n in kept],
    "edges": [{"from": e.get("from_id"), "to": e.get("to_id"), "kind": e.get("kind")} for e in sub_edges],
}
json.dump(graph, open(f"{outdir}/graph.json","w"), ensure_ascii=False)
meta = {
    "total_nodes": total_nodes,
    "total_edges": total_edges,
    "shown_nodes": len(graph["nodes"]),
    "shown_edges": len(graph["edges"]),
    "truncated": total_nodes > len(graph["nodes"]),
    "kinds": dict(sorted(collections.Counter(n.get("kind","") for n in nodes).items(),
                         key=lambda kv: -kv[1])),
}
json.dump(meta, open(f"{outdir}/meta.json","w"), ensure_ascii=False, indent=1)
print(f"    图：全量 {total_nodes} 节点 / {total_edges} 边 → 展示 {len(graph['nodes'])} / {len(graph['edges'])}")
PY
}

MANIFEST='['
first=1

for entry in "${SAMPLES[@]}"; do
  IFS='|' read -r slug path desc lic url <<< "$entry"
  sample_dir="$SAMPLES_DIR/$path"
  if [[ ! -d "$sample_dir" ]]; then
    echo "==> 跳过（样本不存在）: $path"
    continue
  fi
  echo "==> 处理 $slug ($path)"

  create_out="$("$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" create --name "demo-$slug" --path "$sample_dir" 2>&1)"
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

  # 规则检测结果
  "$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" check --project "$pid" --json > "$sdir/violations.json" 2>/dev/null || echo '{}' > "$sdir/violations.json"

  # 图导出 + 降采样
  "$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" export --project "$pid" > "$DATA/full-$slug.json" 2>/dev/null
  downsample_graph "$DATA/full-$slug.json" "$sdir" "$MAX_NODES"

  # 召回示例（Markdown）
  q="${QUERIES[$slug]:-}"
  if [[ -n "$q" ]]; then
    : > "$sdir/recall.md"
    IFS='|' read -ra qs <<< "$q"
    for query in "${qs[@]}"; do
      {
        echo "### 「$query」"
        echo
        "$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" recall --project "$pid" --query "$query" --markdown --limit 5 2>/dev/null || echo "（召回失败）"
        echo
      } >> "$sdir/recall.md"
    done
  fi

  nviol="$(count_violations "$sdir/violations.json")"
  meta_nodes="$(python3 -c 'import json;print(json.load(open('"'$sdir/meta.json'"'))["total_nodes"])')"

  # 每个样本的 README（GitHub 可渲染）
  {
    echo "# $slug"
    echo
    echo "$desc"
    echo
    echo "- 许可证：**$lic**"
    [[ "$url" != "—" ]] && echo "- 上游：$url"
    echo "- 样本路径（本地，不随仓库分发）：\`samples/$path\`"
    echo
    echo "## 图规模"
    echo
    echo "| 节点类型 | 数量 |"
    echo "| --- | --- |"
    stats_to_md "$sdir/stats.txt"
    echo
    echo "## 规则检测结果"
    echo
    json_violations_to_md "$sdir/violations.json"
  } > "$sdir/README.md"

  [[ $first -eq 1 ]] || MANIFEST+=','
  first=0
  MANIFEST+="{\"slug\":\"$slug\",\"desc\":\"$desc\",\"license\":\"$lic\",\"upstream\":\"$url\",\"nodes\":$meta_nodes,\"violations\":$nviol}"
done

MANIFEST+=']'
echo "$MANIFEST" > "$OUT/projects.json"

# 顶层画廊（GitHub 浏览用）
{
  echo "# GraphTell 示例画廊"
  echo
  echo "> 自动生成（\`tools/gen_demo.sh\`）。交互式版本见 [index.html](index.html)（GitHub Pages）。"
  echo "> 每个样本在本地跑建图 + 规则检测 + 召回示例，无需模型权重（召回走哈希兜底）。"
  echo "> 第三方样本按各自许可证授权，来源见 [samples-licenses.md](../samples-licenses.md)；其源码不随本仓库分发。"
  echo
  echo "| 样本 | 简介 | 许可证 | 图规模 | 规则违规 | 详情 |"
  echo "| --- | --- | --- | --- | --- | --- |"
  python3 - "$OUT/projects.json" <<'PY'
import json,sys
for p in json.load(open(sys.argv[1])):
    print(f"| {p['slug']} | {p['desc']} | {p['license']} | {p['nodes']} 节点 | {p['violations']} 条 | [详情]({p['slug']}/README.md) |")
PY
} > "$OUT/README.md"

# 把生成物里的绝对仓库路径收敛成相对路径（避免泄露本地绝对路径）。
find "$OUT" \( -name '*.md' -o -name '*.txt' -o -name '*.json' \) -exec sed -i "s|${ROOT}/||g; s|${REPO_ROOT}/||g" {} +

echo "==> 完成，产物在 $OUT/"
