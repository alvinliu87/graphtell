#!/usr/bin/env python3
"""生成「真 UI + 录制 API 回放」的静态 demo（GitHub Pages）。

思路
----
1. 用**真实后端**对 demo 样本建图；
2. 爬取前端会调用的所有 `/api/*` 接口，把响应录成静态 JSON；
3. 把构建好的 **React SPA**（真产品前端）拷进 `docs/demo/`，注入一个 fetch 拦截器，
   让 `/api/*` 请求改从录制数据回放。

于是静态站上跑的是**真产品前端**：界面、图、规则检验、提示词增强与本地一致，
且不需要后端、不需要买域名（GitHub Pages）。

局限（设计如此）
----------------
* 写操作（新建 / 删除工程、跑建图、文件浏览）在静态站上不可用 —— 它们需要真后端。
* 任意**新输入**的召回问句无法真算，会回落到预录的几条示例之一（见 README 说明）。
* 未经录制的深度请求会拿到空响应（页面降级而非崩溃）。

用法
----
    ./tools/gen_ui_demo.py                    # 默认样本
    ./tools/gen_ui_demo.py --no-build         # 跳过前端构建（已构建过）
    ./tools/gen_ui_demo.py --sample name=path # 追加/覆盖样本
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

# 默认 demo 样本：优先真实知名项目（CRMEB / Bagisto），再带一个自造夹具。
DEFAULT_SAMPLES = [
    ("CRMEB", "/home/alvin/graphtell/samples/php-projects/thinkphp/CRMEB"),
    ("bagisto", "/home/alvin/graphtell/samples/php-projects/laravel/bagisto"),
    ("frontend-backend-link", "/home/alvin/graphtell/samples/frontend-backend-link"),
]

# 演示用的召回问句（中文，展示「中文意图 → 英文节点」的桥接）
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
    """把每个 (method, path, query, body) 的响应录成一个静态 JSON 文件。"""

    def __init__(self, out_dir: Path):
        self.out_dir = out_dir
        self.files_dir = out_dir / "f"
        if self.files_dir.exists():
            shutil.rmtree(self.files_dir)
        self.files_dir.mkdir(parents=True)
        self.index: dict[str, str] = {}
        self.seen: dict[str, str] = {}  # 每个 METHOD+path 的首条（兜底）
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
            # POST/PUT 需要按 body 区分（例如不同的召回问句）
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
        except Exception as e:  # 超时 / 连接问题
            payload = None
            print(f"    ! {method} {self.norm(url)} -> {e}", file=sys.stderr)

        if payload is None:
            return None
        fname = self._save(payload)
        self.index[key] = fname
        self.seen.setdefault(base, fname)
        return payload

    def finalize(self):
        # 兜底键：
        #  * `METHOD path|*` —— 写操作未命中 body 时用（避免页面直接报错）；
        #  * `METHOD path`  —— 忽略查询参数的兜底：任意未精确录制的 GET / 写操作，
        #    回落到该路径「第一条」录制（数据有效、页面照常渲染，只是过滤不一定精确）。
        for base, fname in self.seen.items():
            self.index.setdefault(base + "|*", fname)
            self.index.setdefault(base, fname)
        (self.out_dir / "index.json").write_text(
            json.dumps(self.index, ensure_ascii=False, indent=1), encoding="utf-8"
        )


REPLAY_JS = r"""/* 静态 demo 的 API 回放：把 /api/* 请求改从预录的 JSON 读取。
   这样 GitHub Pages 这种纯静态托管上跑的仍是**真产品前端**，无需后端。 */
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
      // 写 / 查询类：优先按 body 精确匹配（例如不同的召回问句各返回各的结果）
      let bodyKey = null;
      try {
        const text = await req.clone().text();
        const obj = JSON.parse(text || '{}');
        bodyKey = key + '|' + JSON.stringify(obj, Object.keys(obj).sort());
      } catch (e) { /* body 不是 JSON，忽略 */ }
      file = (bodyKey && table[bodyKey]) || table[key + '|*'] || table[key];
    } else {
      file = table[key];
    }
    // 忽略查询参数的兜底：命中「同路径第一条」录制，保证页面照常渲染
    if (!file) file = table[base];
    if (!file) {
      console.warn('[demo] 未录制的请求（静态 demo 无后端）:', method, norm(req.url));
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

DEMO_README = r"""# GraphTell 静态 Demo

这是用**真实产品前端 + 录制 API 回放**生成的纯静态站点（无后端依赖），
可直接托管在 GitHub Pages 等任意静态空间。

## 这个 demo 里能做什么
- **浏览工程**：首页列出已录制的示例工程（CRMEB / Bagisto / 自造夹具）。
- **语义图**：进入某工程的「图」页签，看路由/表/事件等视角的语义依赖图；
  点节点可展开链路（对象视图）。
- **规则检验**：`合规` 页签看规则命中与诊断。
- **提示词增强**：`召回 / 提示词` 页签，对**预置的中文问句**做代码召回并合成提示词。

## 界面预览（真机截图，来自本 demo 本地运行）
![工程总览](../screenshots/home.png)
![语义图](../screenshots/graph-crmeb.png)
![规则检验：违规表](../screenshots/rules-crmeb.png)
![提示词增强](../screenshots/recall.png)

## 局限性（设计如此，非 bug）
- 写操作（新建/删除工程、跑建图、文件浏览）在静态站上不可用——它们需要真后端。
- 召回/提示词的**输入框里只能选预置问句**会命中录制；任意新输入的问句会回落到某条
  预置结果（页面不报错，但答案不是你输入的那个）。
- 未经录制到的深度请求（比如手动改 URL 跳到一个没录的节点/视角）会拿到空或「全局视角」
  兜底响应，页面降级而非崩溃。

## 想换成你自己的样本 / 重新生成
```bash
# 默认样本（需先 cargo build -p gt-app）
python3 tools/gen_ui_demo.py

# 指定样本（name=绝对路径）
python3 tools/gen_ui_demo.py --sample "我的项目=/abs/path/to/code"

# 跳过前端构建（已构建过时）
python3 tools/gen_ui_demo.py --no-build
```
生成物在 `docs/demo/`，整套作为站点根目录托管即可（已用相对路径 + hash 路由，
挂在 `https://<user>.github.io/<repo>/` 子路径下也能正常工作）。
"""


def build_ui() -> None:
    """用 same-origin + hash 路由 + 相对 base 构建前端（Pages 子路径部署所需）。"""
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
    # 生成前清理旧产物（避免上一轮残留的 per-project 子目录 / 旧 README 混进来）
    if out.exists():
        for child in out.iterdir():
            if child.name in (".gitkeep",):
                continue
            if child.is_dir():
                shutil.rmtree(child)
            else:
                child.unlink()
    out.mkdir(parents=True, exist_ok=True)

    # 1) 建图：每个样本一个工程
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

    # 2) 起后端
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

        # 3) 录制
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
            # 节点列表（前端带 limit / kind / name_contains 组合）
            for limit in (50, 100, 200, 500, 1000, 5000):
                rec.record("GET", f"{P}/nodes?limit={limit}")

            ps = rec.record("GET", P + "/perspectives")
            persps = (ps or {}).get("data", []) if isinstance(ps, dict) else []
            node_ids: set[int] = set()
            edge_ids: set[int] = set()
            # candidates 前端默认 limit=300；聚合视角默认 limit=12
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
                # 全局视角（d 省略时默认 2）
                for depth in (1, 2, 3):
                    rec.record("GET", f"{P}/view/{k}?depth={depth}")

            # 对象视图（点击节点展开）—— 默认 depth=2；前 12 个候选额外覆盖 1/3，
            # 确保首页自动选中的 candidates[0] 能精确命中（而非回落到全局视角）。
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

            # 节点 / 边的详情端点（点击展开时用）；上限 120 控规模
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

            # 提示词增强（召回）：预录几条示例问句
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

    # 4) 拷贝前端产物 + 注入拦截器
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

    # 5) 收敛绝对路径（不泄露本机目录）
    for f in out.rglob("*.json"):
        try:
            t = f.read_text(encoding="utf-8")
        except Exception:
            continue
        new = t.replace("/home/alvin/graphtell/samples/", "/samples/")
        if new != t:
            f.write_text(new, encoding="utf-8")

    # 6) 说明文档
    (out / "README.md").write_text(DEMO_README, encoding="utf-8")

    print(f"==> 完成：静态 demo 在 {out}/")
    print("    本地预览：python3 -m http.server -d %s 8080" % out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
