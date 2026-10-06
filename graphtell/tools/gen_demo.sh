#!/usr/bin/env bash
#
# Generate GraphTell's public demo: for each sample project run "build graph -> rule check -> recall example -> graph export",
# producing static site data hostable directly on GitHub Pages (docs/demo/data/) and a GitHub-natively-renderable
# Markdown gallery (docs/demo/README.md).
#
# Design notes:
#   * Depends only on the existing CLI (create / stats / check --json / recall --markdown / export).
#      We don't commit the 13G model weights -- recall falls back to hashing, enough for a demo.
#   * The sample tree has been merged into the **repo-root samples/** (the old inner samples/ is deleted). Override with
#     GRAPHTELL_SAMPLES_DIR. Missing samples are skipped, consistent with the integration tests' soft dependency.
#   * A large project's (~96k nodes) **full graph can't be handed to the browser directly**: here we filter by
#     "interesting node kinds" and truncate to MAX_NODES, exporting only a subgraph; the full scale is stated honestly in meta.
#   * Artifacts are pure static files: push to the default branch and GitHub renders them; together with
#     .github/workflows/deploy-demo.yml they can be published to GitHub Pages (no domain needed).
#
# Usage:
#   ./tools/gen_demo.sh                 # use an existing target/release|debug/graphtell
#   ./tools/gen_demo.sh --build         # compile with --no-default-features first (portable, no model needed)
#   GT_BINARY=/path/to/graphtell ./tools/gen_demo.sh
#   DEMO_ONLY=frontend-backend-link ./tools/gen_demo.sh   # run a single sample only
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
  echo "==> Compiling graphtell (--no-default-features) ..."
  cargo build -p gt-app --no-default-features
  BIN=target/debug/graphtell
fi
if [[ -z "$BIN" || ! -x "$BIN" ]]; then
  echo "graphtell binary not found. Please run \`cargo build -p gt-app\` first, or pass GT_BINARY=... or add --build" >&2
  exit 1
fi
# Capability check: the `export` subcommand was added later; an old release binary lacks it. If used directly,
# clap exits with code 2 complaining "unknown subcommand", while this script sends the subcommand's stderr to /dev/null,
# so the result is a **silent failure** (you see the script abort, but not why). Here we check explicitly and state the problem.
if ! "$BIN" export --help >/dev/null 2>&1; then
  echo "\`$BIN\` does not support the export subcommand (most likely an old binary). Please recompile with --build, or point GT_BINARY at a new binary." >&2
  exit 1
fi

# Sample tree location: explicit setting wins, otherwise repo-root samples/ (the single merged location).
SAMPLES_DIR="${GRAPHTELL_SAMPLES_DIR:-}"
if [[ -z "$SAMPLES_DIR" ]]; then
  if [[ -d "$REPO_ROOT/samples" ]]; then SAMPLES_DIR="$REPO_ROOT/samples"; else SAMPLES_DIR="$ROOT/samples"; fi
fi
echo "==> Sample tree: $SAMPLES_DIR"

DATA="$(mktemp -d "${TMPDIR:-/tmp}/graphtell-demo.XXXXXX")"
trap 'rm -rf "$DATA"' EXIT
OUT="$ROOT/docs/demo"
# Note: artifacts must **not** go into a `data/` subdir -- the repo's .gitignore has `**/data/` (for runtime data),
# which would silently swallow them (and per git rules, once a parent dir is excluded, no `!` can re-include a subpath).
# So each sample lands directly under `docs/demo/<slug>/`.
mkdir -p "$OUT"
MAX_NODES="${DEMO_MAX_NODES:-400}"

# Sample manifest: slug | path relative to the sample tree | blurb | license | upstream
#
# The committed default is this project's **own synthetic fixture** only: no third-party project is
# named anywhere in this repository. To record extra checkouts, point GRAPHTELL_SAMPLES_DIR at your own
# sample tree and pass them via DEMO_SAMPLES (same `slug|relpath|blurb|license|upstream` format, one
# entry per line); attribution for those stays with whoever runs the generator.
SAMPLES=(
  "frontend-backend-link|frontend-backend-link|self-made synthetic fixture: frontend ↔ backend cross-end chain|owned by this project|—"
)
if [[ -n "${DEMO_SAMPLES:-}" ]]; then
  while IFS= read -r entry; do
    [[ -n "$entry" ]] && SAMPLES+=("$entry")
  done <<< "$DEMO_SAMPLES"
fi

if [[ -n "${DEMO_ONLY:-}" ]]; then
  filtered=()
  for e in "${SAMPLES[@]}"; do
    [[ "$e" == "${DEMO_ONLY}|"* ]] && filtered+=("$e")
  done
  SAMPLES=("${filtered[@]}")
fi

# Demo recall queries per sample (Chinese, verifying the "Chinese intent -> English node" bridge)
declare -A QUERIES=(
  ["frontend-backend-link"]="订单创建流程|用户登录入口"
)

# Turn the check --json report (an object with a violations array) into a Markdown table.
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

# Extract the stats "node kind counts" section into a Markdown table.
stats_to_md() {
  awk '/^[[:space:]]+[A-Za-z]+[[:space:]]+[0-9]+[[:space:]]*$/{
         gsub(/^[[:space:]]+/,""); n=split($0,a,/[[:space:]]+/);
         kind=a[1]; cnt=a[n]; printf("| %s | %s |\n", kind, cnt);
       }' "$1"
}

# Downsample the export's full graph into a browser-affordable subgraph + meta.
# Usage: downsample_graph <full.json> <outdir> <max_nodes>
downsample_graph() {
  python3 - "$1" "$2" "$3" <<'PY'
import json,sys,collections
src, outdir, maxn = sys.argv[1], sys.argv[2], int(sys.argv[3])
d = json.load(open(src))
nodes, edges, files = d.get("nodes", []), d.get("edges", []), d.get("files", {})
total_nodes, total_edges = len(nodes), len(edges)

# Structural nodes like FILE / Namespace are excluded from the demo graph: there are many and they only blow the graph into a blob of scattered points.
SKIP = {"File", "Namespace"}
allowed = {n.get("id") for n in nodes if n.get("kind") not in SKIP}

# Undirected adjacency, used by BFS to take a connected subgraph.
adj = collections.defaultdict(list)
for e in edges:
    f, t = e.get("from_id"), e.get("to_id")
    if f in allowed and t in allowed:
        adj[f].append(t)
        adj[t].append(f)

# Seeds: kinds with high demo value first, so the subgraph is all "semantically meaningful" nodes.
SEED = ["HttpContract","Table","Class","Service","Dao","Method","Cache","Queue",
        "EventBus","Schedule","Page","Store","ConfigKey","Module","Function"]
by_kind = collections.defaultdict(list)
for n in nodes:
    if n.get("id") in allowed:
        by_kind[n.get("kind", "")].append(n)
seeds = []
for k in SEED:
    seeds.extend(by_kind.get(k, []))

# BFS from the seeds until maxn -- yielding a **connected** subgraph.
# (Truncating directly by id gives a pile of unrelated sand: measured on a large project, 400 nodes had 0 edges.)
from collections import deque
seen, queue = set(), deque()
# Seeds take only part of the budget: A large project alone has 1603 HttpContracts; if seeds filled maxn directly,
# BFS would never get a chance to expand, and the result would be a bag of unrelated contract nodes (measured: 400 nodes / 0 edges).
# Reserve ~3/4 of the budget for BFS so the connecting middle nodes (CallSite / Method / Dao …) get pulled in.
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
    echo "==> skipped (sample missing): $path"
    continue
  fi
  echo "==> processing $slug ($path)"

  create_out="$("$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" create --name "demo-$slug" --path "$sample_dir" 2>&1)"
  pid="$(echo "$create_out" | grep -oE '#[0-9]+' | head -1 | tr -d '#')"
  if [[ -z "$pid" ]]; then
    echo "    graph build returned no project id, skipped. output: $create_out" >&2
    continue
  fi
  sdir="$OUT/$slug"
  mkdir -p "$sdir"

  # graph scale
  stats_out="$("$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" stats --project "$pid" 2>&1)"
  echo "$stats_out" > "$sdir/stats.txt"

  # rule-check results
  "$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" check --project "$pid" --json > "$sdir/violations.json" 2>/dev/null || echo '{}' > "$sdir/violations.json"

  # graph export + downsampling
  "$BIN" --data-dir "$DATA" --fkb-dir "$ROOT/fkb" export --project "$pid" > "$DATA/full-$slug.json" 2>/dev/null
  downsample_graph "$DATA/full-$slug.json" "$sdir" "$MAX_NODES"

  # recall example (Markdown)
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

  # per-sample README (GitHub-renderable)
  {
    echo "# $slug"
    echo
    echo "$desc"
    echo
    echo "- license: **$lic**"
    [[ "$url" != "—" ]] && echo "- upstream: $url"
    echo "- sample path (local, not distributed with the repo): \`samples/$path\`"
    echo
    echo "## Graph scale"
    echo
    echo "| node type | count |"
    echo "| --- | --- |"
    stats_to_md "$sdir/stats.txt"
    echo
    echo "## Rule-check results"
    echo
    json_violations_to_md "$sdir/violations.json"
  } > "$sdir/README.md"

  [[ $first -eq 1 ]] || MANIFEST+=','
  first=0
  MANIFEST+="{\"slug\":\"$slug\",\"desc\":\"$desc\",\"license\":\"$lic\",\"upstream\":\"$url\",\"nodes\":$meta_nodes,\"violations\":$nviol}"
done

MANIFEST+=']'
echo "$MANIFEST" > "$OUT/projects.json"

# top-level gallery (for GitHub browsing)
{
  echo "# GraphTell sample gallery"
  echo
  echo "> auto-generated by (\`tools/gen_demo.sh\`). For the interactive version see [index.html](index.html) (GitHub Pages)."
  echo "> each sample runs graph build + rule check + recall example locally, no model weights needed (recall falls back to hashing)."
  echo "> third-party samples are licensed under their own terms; see [samples-licenses.md](../samples-licenses.md); their source is not distributed with this repo."
  echo
  echo "| sample | blurb | license | graph size | rule violations | details |"
  echo "| --- | --- | --- | --- | --- | --- |"
  python3 - "$OUT/projects.json" <<'PY'
import json,sys
for p in json.load(open(sys.argv[1])):
    print(f"| {p['slug']} | {p['desc']} | {p['license']} | {p['nodes']} 节点 | {p['violations']} 条 | [详情]({p['slug']}/README.md) |")
PY
} > "$OUT/README.md"

# Normalize absolute repo paths in the output to relative (avoid leaking local absolute paths).
find "$OUT" \( -name '*.md' -o -name '*.txt' -o -name '*.json' \) -exec sed -i "s|${ROOT}/||g; s|${REPO_ROOT}/||g" {} +

echo "==> done, artifacts at $OUT/"
