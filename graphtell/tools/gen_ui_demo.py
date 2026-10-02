#!/usr/bin/env python3
"""Generate a static demo of "real UI + recorded API replay" (GitHub Pages).

Approach
--------
1. Build the graph for the demo samples with the **real backend**;
2. Crawl every `/api/*` endpoint the frontend calls and record responses as static JSON;
3. Copy the built **React SPA** (the real product frontend) into `docs/demo/` and inject a fetch
   interceptor so `/api/*` requests are served from the recorded data instead.

So the static site runs the **real product frontend**: UI, graph, rule checking and prompt
augmentation match local behavior, with no backend needed and no domain to buy (GitHub Pages).

Limitations (by design)
-----------------------
* Write operations (create / delete project, run build-graph, file browsing) are unavailable on
  the static site -- they need a real backend.
* Any **newly typed** recall query can't really be computed; it falls back to one of the
  pre-recorded examples (see the README notes).
* Un-recorded deep requests get an empty response (page degrades rather than crashes).

Usage
-----
    ./tools/gen_ui_demo.py                    # default samples
    ./tools/gen_ui_demo.py --no-build         # skip frontend build (already built)
    ./tools/gen_ui_demo.py --sample name=path # add / override a sample
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUT = ROOT / "docs" / "demo"

# Default demo samples: prefer real well-known projects (CRMEB / Bagisto), plus one self-made fixture.
DEFAULT_SAMPLES = [
    ("CRMEB", "/home/alvin/graphtell/samples/php-projects/thinkphp/CRMEB"),
    ("bagisto", "/home/alvin/graphtell/samples/php-projects/laravel/bagisto"),
    ("frontend-backend-link", "/home/alvin/graphtell/samples/frontend-backend-link"),
]

# Recall queries for the demo (Chinese, showing the "Chinese intent -> English node" bridge)
DEFAULT_QUERIES = {
    "CRMEB": ["订单支付流程", "商品库存扣减", "用户优惠券"],
    "bagisto": ["购物车结算", "商品分类查询"],
    "frontend-backend-link": ["订单创建流程", "用户登录入口"],
}


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Recorder:
    """Record the response of each (method, path, query, body) into a static JSON file."""

    def __init__(self, out_dir: Path):
        self.out_dir = out_dir
        self.files_dir = out_dir / "f"
        if self.files_dir.exists():
            shutil.rmtree(self.files_dir)
        self.files_dir.mkdir(parents=True)
        self.index: dict[str, str] = {}
        self.seen: dict[str, str] = {}  # First entry of each METHOD+path (fallback)
        self.n = 0

    @staticmethod
    def norm(url: str) -> str:
        u = urllib.parse.urlsplit(url)
        q = urllib.parse.parse_qsl(u.query)
        q.sort()
        query = urllib.parse.urlencode(q, doseq=True)
        return u.path + ("?" + query if query else "")

    def _save(self, payload) -> str:
        self.n += 1
        name = f"{self.n:05d}.json"
        (self.files_dir / name).write_text(
            json.dumps(payload, ensure_ascii=False), encoding="utf-8"
        )
        return "f/" + name

    def record(self, method: str, url: str, body=None, timeout: int = 180):
        key = method + " " + self.norm(url)
        base = method + " " + urllib.parse.urlsplit(url).path
        data = None
        headers = {}
        if body is not None:
            data = json.dumps(body, ensure_ascii=False).encode("utf-8")
            headers["Content-Type"] = "application/json"
            # POST/PUT need to be distinguished by body (e.g. different recall queries)
            try:
                norm_body = json.dumps(body, sort_keys=True, separators=(",", ":"))
                key = key + "|" + norm_body
            except Exception:
                pass
        try:
            req = urllib.request.Request(url, data=data, headers=headers, method=method)
            with urllib.request.urlopen(req, timeout=timeout) as r:
                raw = r.read()
            try:
                payload = json.loads(raw)
            except Exception:
                payload = None
        except urllib.error.HTTPError as e:
            payload = None
            print(f"    ! {method} {self.norm(url)} -> HTTP {e.code}", file=sys.stderr)
        except Exception as e:  # Timeout / connection issues
            payload = None
            print(f"    ! {method} {self.norm(url)} -> {e}", file=sys.stderr)

        if payload is None:
            return None
        fname = self._save(payload)
        self.index[key] = fname
        self.seen.setdefault(base, fname)
        return payload

    def finalize(self):
        # Fallback keys:
        #  * `METHOD path|*` -- used when a write op misses the body (avoid a hard page error);
        #  * `METHOD path`  -- fallback that ignores query params: any GET / write op not exactly recorded,
        #    falls back to the path's "first" recording (data valid, page still renders, just filtering may be imprecise).
        for base, fname in self.seen.items():
            self.index.setdefault(base + "|*", fname)
            self.index.setdefault(base, fname)
        (self.out_dir / "index.json").write_text(
            json.dumps(self.index, ensure_ascii=False, indent=1), encoding="utf-8"
        )


REPLAY_JS = r"""/* API replay for the static demo: serve /api/* requests from pre-recorded JSON.
   This way pure static hosting like GitHub Pages still runs the **real product frontend**,
   with no backend needed. */
(() => {
  const IDX_URL = new URL('./index.json', document.currentScript.src).href;
  let idx = null;
  let pending = null;

  const norm = (u) => {
    try {
      const url = new URL(u, location.href);
      const sp = new URLSearchParams(url.search);
      sp.sort();
      const q = sp.toString();
      return url.pathname + (q ? '?' + q : '');
    } catch (e) { return String(u); }
  };

  const jsonResponse = (data) =>
    new Response(JSON.stringify(data), { status: 200, headers: { 'Content-Type': 'application/json' } });

  const loadIdx = async () => {
    if (idx) return idx;
    if (!pending) pending = fetch(IDX_URL).then(r => r.json()).catch(() => ({}));
    idx = await pending;
    return idx;
  };

  const orig = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const req = (input instanceof Request) ? input : new Request(input, init);
    if (req.url.indexOf('/api/') === -1) return orig(input, init);
    const table = await loadIdx();
    const method = (req.method || 'GET').toUpperCase();
    const key = method + ' ' + norm(req.url);
    const base = method + ' ' + new URL(req.url, location.href).pathname;
    let file = null;
    if (method !== 'GET') {
      // Write / query class: prefer exact body match (e.g. different recall queries each
      // return their own result)
      let bodyKey = null;
      try {
        const text = await req.clone().text();
        const obj = JSON.parse(text || '{}');
        bodyKey = key + '|' + JSON.stringify(obj, Object.keys(obj).sort());
      } catch (e) { /* body is not JSON, ignore */ }
      file = (bodyKey && table[bodyKey]) || table[key + '|*'] || table[key];
    } else {
      file = table[key];
    }
    // Fallback ignoring query params: hit the "first recording of the same path" so the page
    // still renders normally
    if (!file) file = table[base];
    if (!file) {
      console.warn('[demo] Unrecorded request (static demo has no backend):', method, norm(req.url));
      return jsonResponse({});
    }
    try {
      const data = await (await fetch(new URL(file, IDX_URL).href)).json();
      return jsonResponse(data);
    } catch (e) {
      return jsonResponse({});
    }
  };
})();
"""

DEMO_README = r"""# GraphTell static demo

This is a purely static site (no backend dependency) generated with the **real product frontend +
recorded API replay**; it can be hosted on GitHub Pages or any static hosting.

## What you can do in this demo
- **Browse projects**: the home page lists the recorded sample projects (CRMEB / Bagisto /
  self-made fixture).
- **Semantic graph**: open a project's "graph" tab to see the semantic dependency graph by
  perspective (routes / tables / events …); click a node to expand its chain (object view).
- **Rule checking**: the `compliance` tab shows rule hits and diagnostics.
- **Prompt augmentation**: the `recall / prompt` tab runs code recall over the **preset Chinese
  queries** and composes a prompt.

## UI preview (real screenshots, from running this demo locally)
![Project overview](../screenshots/home.png)
![Semantic graph](../screenshots/graph-crmeb.png)
![Rule checking: violations table](../screenshots/rules-crmeb.png)
![Prompt augmentation](../screenshots/recall.png)

## Limitations (by design, not bugs)
- Write operations (create / delete project, run build-graph, file browsing) are unavailable on
  the static site -- they need a real backend.
- In the recall / prompt **input box, only preset queries** hit a recording; any newly typed query
  falls back to one of the preset results (the page doesn't error, but the answer isn't the one
  you typed).
- Un-recorded deep requests (e.g. manually editing the URL to jump to a node / perspective that
  wasn't recorded) get an empty or "global perspective" fallback response; the page degrades
  rather than crashes.

## Want your own samples / regenerate
```bash
# Default samples (needs `cargo build -p gt-app` first)
python3 tools/gen_ui_demo.py

# Specify a sample (name=absolute-path)
python3 tools/gen_ui_demo.py --sample "my-project=/abs/path/to/code"

# Skip the frontend build (when already built)
python3 tools/gen_ui_demo.py --no-build
```
Output lands in `docs/demo/`; host that whole directory as the site root (it uses relative paths +
hash routing, so it also works under a sub-path like `https://<user>.github.io/<repo>/`).
"""


def build_ui() -> None:
    """Build the frontend with same-origin + hash routing + relative base (needed for Pages sub-path deploy)."""
    env = dict(os.environ)
    env.update({"VITE_API_BASE": "same-origin", "VITE_STATIC_DEMO": "1"})
    print("==> 构建前端（VITE_API_BASE=same-origin VITE_STATIC_DEMO=1 --base=./）")
    subprocess.run(
        ["npx", "tsc", "-b"], cwd=ROOT / "ui", env=env, check=False,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        ["npx", "vite", "build", "--base=./"], cwd=ROOT / "ui", env=env, check=True
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-build", action="store_true", help="跳过前端构建")
    ap.add_argument("--out", default=str(DEFAULT_OUT))
    ap.add_argument("--sample", action="append", default=[],
                    help="name=path（可多次；给出即覆盖默认样本）")
    args = ap.parse_args()

    samples = DEFAULT_SAMPLES
    if args.sample:
        samples = []
        for s in args.sample:
            name, _, path = s.partition("=")
            samples.append((name, path))

    if not args.no_build:
        build_ui()

    out = Path(args.out)
    # Clean old artifacts before generating (avoid leftovers from the previous run: per-project subdirs / old README)
    if out.exists():
        for child in out.iterdir():
            if child.name in (".gitkeep",):
                continue
            if child.is_dir():
                shutil.rmtree(child)
            else:
                child.unlink()
    out.mkdir(parents=True, exist_ok=True)

    # 1) Build graph: one project per sample
    data_dir = Path("/tmp/graphtell-ui-demo-data")
    if data_dir.exists():
        shutil.rmtree(data_dir)
    data_dir.mkdir(parents=True)
    bin_ = ROOT / "target" / "debug" / "graphtell"
    if not bin_.exists():
        print("找不到 target/debug/graphtell，请先 cargo build -p gt-app", file=sys.stderr)
        return 1

    print("==> 建图")
    for name, path in samples:
        if not Path(path).is_dir():
            print(f"    跳过（不存在）：{path}")
            continue
        t = time.time()
        subprocess.run(
            [str(bin_), "--data-dir", str(data_dir), "--fkb-dir", str(ROOT / "fkb"),
             "create", "--name", name, "--path", path],
            check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        print(f"    {name}: 建图完成 ({time.time()-t:.1f}s)")

    # 2) Start backend
    port = free_port()
    srv = subprocess.Popen(
        [str(bin_), "--data-dir", str(data_dir), "--fkb-dir", str(ROOT / "fkb"),
         "serve", "--bind", "127.0.0.1", "--port", str(port)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    base = f"http://127.0.0.1:{port}"
    try:
        for _ in range(60):
            try:
                urllib.request.urlopen(base + "/api/health", timeout=2)
                break
            except Exception:
                time.sleep(1)
        else:
            print("后端未能启动", file=sys.stderr)
            return 1

        # 3) Record
        rec = Recorder(out / "__api__")
        rec.record("GET", base + "/api/health")
        rec.record("GET", base + "/api/server/status")
        pj = rec.record("GET", base + "/api/projects")
        projects = (pj or {}).get("data", []) if isinstance(pj, dict) else []
        rec.record("GET", base + "/api/rules")

        for pr in projects:
            pid = pr.get("id")
            name = pr.get("name")
            print(f"==> 录制工程 {name} (#{pid})")
            P = f"{base}/api/projects/{pid}"
            for ep in ["", "/sub-projects", "/stats", "/diagnostics",
                       "/diagnostics/summary", "/check/summary", "/rules/config"]:
                rec.record("GET", P + ep)
            rec.record("POST", P + "/check", {"rule_ids": []})
            for limit in (50, 200, 1000, 5000):
                rec.record("GET", f"{P}/violations?limit={limit}")
            # Node list (frontend passes limit / kind / name_contains combinations)
            for limit in (50, 100, 200, 500, 1000, 5000):
                rec.record("GET", f"{P}/nodes?limit={limit}")

            ps = rec.record("GET", P + "/perspectives")
            persps = (ps or {}).get("data", []) if isinstance(ps, dict) else []
            node_ids: set[int] = set()
            edge_ids: set[int] = set()
            # candidates: frontend default limit=300; aggregate perspective default limit=12
            cand_by_persp: dict[str, list[int]] = {}
            for pv in persps:
                key = pv.get("id") or pv.get("key") or pv.get("name")
                if not key:
                    continue
                k = urllib.parse.quote(str(key))
                cands = rec.record("GET", f"{P}/view/{k}/candidates?limit=300")
                cd = (cands or {}).get("data", []) if isinstance(cands, dict) else []
                ids = [int(c["id"]) for c in cd[:60] if isinstance(c, dict) and c.get("id")]
                cand_by_persp[k] = ids
                node_ids.update(ids)
                rec.record("GET", f"{P}/aggregate/{k}?limit=12")
                rec.record("GET", f"{P}/aggregate/{k}?limit=24")
                # Global perspective (defaults to 2 when d omitted)
                for depth in (1, 2, 3):
                    rec.record("GET", f"{P}/view/{k}?depth={depth}")

            # Object view (click a node to expand) -- default depth=2; the first 12 candidates get extra 1/3 coverage,
            # so the auto-selected candidates[0] on the home page matches exactly (instead of falling back to the global perspective).
            for pv in persps[:2]:
                key = pv.get("id") or pv.get("key") or pv.get("name")
                if not key:
                    continue
                k = urllib.parse.quote(str(key))
                ids = cand_by_persp.get(k, [])[:60]
                for i, nid in enumerate(ids):
                    for depth in ([2] if i >= 12 else (1, 2, 3)):
                        ov = rec.record("GET", f"{P}/view/{k}?node={nid}&depth={depth}")
                        ovd = (ov or {}).get("data", {}) if isinstance(ov, dict) else {}
                        for n in (ovd.get("nodes") or []):
                            if isinstance(n, dict) and n.get("id"):
                                node_ids.add(int(n["id"]))
                        for e in (ovd.get("edges") or []):
                            if isinstance(e, dict) and e.get("id"):
                                edge_ids.add(int(e["id"]))

            # Node / edge detail endpoints (used when expanding); cap 120 to control size
            for nid in sorted(node_ids)[:120]:
                rec.record("GET", f"{base}/api/nodes/{nid}")
                rec.record("GET", f"{base}/api/nodes/{nid}/annotations")
                rec.record("GET", f"{base}/api/nodes/{nid}/locations")
                for direction in ("in", "out", "both"):
                    rec.record("GET",
                               f"{base}/api/nodes/{nid}/neighbors?direction={direction}")
                for depth, mx in ((1, 200), (2, 300), (2, 500)):
                    rec.record("GET",
                               f"{base}/api/nodes/{nid}/subgraph?depth={depth}&max_nodes={mx}")
            for eid in sorted(edge_ids)[:120]:
                rec.record("GET", f"{base}/api/edges/{eid}/evidence")

            # Prompt augmentation (recall): pre-record a few example queries
            for q in (DEFAULT_QUERIES.get(name) or ["订单支付流程"]):
                rec.record("POST", P + "/recall", {"query": q, "limit": 8})
                rec.record("POST", P + "/prompt", {"query": q, "limit": 8})

        rec.finalize()
        print(f"==> 录制完成：{rec.n} 个响应 -> {out/'__api__'}")
    finally:
        srv.send_signal(signal.SIGTERM)
        try:
            srv.wait(timeout=10)
        except Exception:
            srv.kill()

    # 4) Copy frontend artifacts + inject interceptor
    dist = ROOT / "ui" / "dist"
    if not dist.exists():
        print("ui/dist 不存在，构建可能失败", file=sys.stderr)
        return 1
    for item in dist.iterdir():
        dst = out / item.name
        if item.is_dir():
            if dst.exists():
                shutil.rmtree(dst)
            shutil.copytree(item, dst)
        else:
            shutil.copy2(item, dst)

    (out / "__api__" / "replay.js").write_text(REPLAY_JS, encoding="utf-8")
    idx_html = out / "index.html"
    html = idx_html.read_text(encoding="utf-8")
    if "replay.js" not in html:
        html = html.replace("</head>", '  <script src="./__api__/replay.js"></script>\n  </head>')
        idx_html.write_text(html, encoding="utf-8")

    # 5) Normalize absolute paths (don't leak local dirs)
    for f in out.rglob("*.json"):
        try:
            t = f.read_text(encoding="utf-8")
        except Exception:
            continue
        new = t.replace("/home/alvin/graphtell/samples/", "/samples/")
        if new != t:
            f.write_text(new, encoding="utf-8")

    # 6) Documentation
    (out / "README.md").write_text(DEMO_README, encoding="utf-8")

    print(f"==> 完成：静态 demo 在 {out}/")
    print("    本地预览：python3 -m http.server -d %s 8080" % out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
