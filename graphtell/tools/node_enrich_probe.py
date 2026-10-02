#!/usr/bin/env python3
"""Node-text enrichment probe: use the project's own i18n bridge (reverse index) to add
"Chinese phrase + synonyms" to node embed text, testing whether it lifts the true answer's cosine into the top 20.

Only the node embed text changes, not the query. First measure whether the enriched "true answer node" cosine is high enough
relative to the current ranking threshold, to avoid blind-editing Rust and spending 56 minutes re-warming only to find the direction wrong.

Dependencies: the same ONNX bge path as cosine_diag.py (the query side still uses expand_intent_aliases + the project bridge).
"""
import argparse
import json
import os
import re
import sqlite3
import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ONNX = os.path.join(ROOT, "models", "bge-m3-onnx", "model.onnx")
TOK = os.path.join(ROOT, "models", "bge-m3", "tokenizer.json")
PREFIX = "Represent this sentence for searching relevant passages: "

# ---- replicate the Rust-side key_tokens / location_tokens / bridge ----
STOPWORDS = {"template","templates","src","app","apps","pages","page","components","component",
    "views","view","index","main","static","assets","public","utils","util","common","shared",
    "lib","libs","core","vendor","dist","build","admin","api","js","ts","vue","jsx","tsx","php",
    "java","py","html","css","scss","min","module","modules","service","services"}

CJK = re.compile(r'[\u4e00-\u9fff]')

def contains_cjk(s): return bool(CJK.search(s))

def split_camel(seg):
    # Rough camel split: break before capitals / at digit boundaries
    out = []
    buf = ""
    prev = ""
    for ch in seg:
        if ch.isupper() and buf and (prev.islower() or (len(buf) > 1 and buf[-1].islower())):
            out.append(buf); buf = ch
        elif ch.isdigit() and buf and not buf[-1].isdigit():
            out.append(buf); buf = ch
        else:
            buf += ch
        prev = ch
    if buf: out.append(buf)
    return out

def key_tokens(key):
    out = []
    for seg in re.split(r'[^0-9a-zA-Z]+', key):
        for t in split_camel(seg):
            t = t.lower()
            if len(t) >= 2 and t.isalnum() and t not in out:
                out.append(t)
    return out

def location_tokens(props):
    out = []
    locs = (props.get("locations") if isinstance(props, dict) else None)
    if not isinstance(locs, list): return out
    for loc in locs[:8]:
        f = loc.get("file") if isinstance(loc, dict) else None
        if not f: continue
        parts = f.replace("\\", "/").split("/")
        for p in parts:
            stem = re.split(r'[.\-_]', p)[0]
            if stem and stem.lower() not in STOPWORDS:
                for t in key_tokens(stem):
                    if t not in out: out.append(t)
    return out

# ---- replicate the query-side expansion (INTENT_ALIASES + project bridge) ----
# A trimmed generic dictionary (synonymous with Rust's INTENT_ALIASES, probe-only)
GENERIC = {
    "查询":"query find search", "获取":"get fetch find load", "列表":"list all index",
    "新增":"add create insert", "添加":"add create insert", "创建":"create add insert",
    "删除":"delete remove destroy", "移除":"remove delete", "修改":"update modify edit save",
    "更新":"update modify save", "编辑":"edit update", "详情":"detail info get",
    "校验":"validate check verify", "验证":"validate verify check", "上传":"upload",
    "下载":"download", "导入":"import", "导出":"export", "统计":"count stat summary total",
    "提交":"submit commit", "撤销":"revoke cancel", "回滚":"rollback", "拦截":"intercept block",
    "过滤":"filter", "搜索":"search", "发送":"send", "接收":"receive", "通知":"notify",
    "回调":"callback", "登录":"login auth signin", "登出":"logout", "注册":"register signup",
    "认证":"authenticate auth", "授权":"authorize oauth", "计算":"compute calculate",
    "生成":"generate", "转换":"convert transform", "解析":"parse", "序列化":"serialize",
    "加密":"encrypt", "解密":"decrypt", "排序":"sort order", "汇总":"aggregate",
    "重置":"reset", "刷新":"refresh",
    "支付":"pay payment checkout", "付款":"pay payment", "用户":"user member customer",
    "角色":"role", "权限":"permission authority", "管理员":"admin", "订单":"order",
    "商品":"product goods item", "库存":"stock inventory", "余额":"balance",
    "优惠":"coupon discount", "折扣":"discount", "金额":"amount price", "价格":"price",
    "账单":"bill invoice", "评论":"comment", "文章":"article post", "博客":"blog",
    "品牌":"brand", "分类":"category", "标签":"tag", "令牌":"token", "凭证":"credential",
    "第三方":"third party oauth", "访问":"access", "字典":"dict dictionary", "配置":"config configuration",
    "工作台":"workbench dashboard", "缓存":"cache", "会话":"session", "消息":"message",
    "日志":"log", "错误":"error", "异常":"exception", "任务":"task job", "服务":"service",
    "接口":"api interface", "模型":"model", "实体":"entity", "字段":"field", "属性":"property attribute",
    "文件":"file", "图片":"image", "视频":"video", "数据":"data",
    "下单":"order place create", "扣减":"deduct reduce decrement dec", "失败":"fail failure",
    "不足":"insufficient lack",
}

def expand_query(query, bridge):
    terms = []
    for zh, en in GENERIC.items():
        if zh in query:
            for t in en.split():
                if t not in terms: terms.append(t)
    # Project bridge: a hit Chinese phrase -> its English tokens
    for zh, toks in bridge:
        if zh in query:
            for t in toks:
                if t not in terms: terms.append(t)
    return terms

def build_bridge(db, project):
    """Replicate project_bridge: walk the i18n / Chinese / source-Chinese-fragment nodes."""
    rows = db.execute(
        "SELECT name, properties FROM nodes WHERE project_id=?", (project,)).fetchall()
    entries = []
    for name, props_json in rows:
        try: props = json.loads(props_json) if props_json else {}
        except Exception: props = {}
        if not isinstance(props, dict): continue
        # English-side tokens
        toks = key_tokens(name)
        for t in location_tokens(props):
            if t not in toks: toks.append(t)
        if not toks: continue
        # Chinese-side phrases
        texts = []
        texts_obj = props.get("texts")
        if isinstance(texts_obj, dict):
            for v in texts_obj.values():
                if isinstance(v, str): texts.append(v.strip())
        if not texts and contains_cjk(name):
            texts.append(name.strip())
        snip = props.get("snippet")
        if isinstance(snip, str):
            for r in CJK.findall(snip):
                pass
            # Coarsely grab contiguous Chinese runs
            for m in re.finditer(r'[\u4e00-\u9fff]{2,30}', snip):
                texts.append(m.group(0))
        added = 0
        for s in texts:
            s = s.strip()
            if 2 <= len(s) <= 30 and contains_cjk(s):
                entries.append((s, toks[:]))
                added += 1
                if added >= 4: break
    return entries

def reverse_index(bridge):
    inv = {}
    for zh, toks in bridge:
        for t in toks:
            inv.setdefault(t, []).append((zh, toks))
    return inv

def node_enrichment(node_name, fqn, inv, cap=8):
    toks = key_tokens(node_name)
    if fqn: toks += key_tokens(fqn)
    seen = set(); added = []
    for t in toks:
        for zh, btoks in inv.get(t, []):
            # Append "Chinese phrase + remaining English tokens"
            chunk = [zh] + [x for x in btoks if x != t]
            for c in chunk:
                if c not in seen:
                    seen.add(c); added.append(c)
            if len(added) >= cap: break
        if len(added) >= cap: break
    return added

_sess = None; _tok = None
def embed(text):
    global _sess, _tok
    if _sess is None:
        _sess = ort.InferenceSession(ONNX, providers=["CPUExecutionProvider"])
        _tok = Tokenizer.from_file(TOK)
    ids = _tok.encode(PREFIX + text).ids
    n = len(ids)
    feeds = {"input_ids": np.array([ids], dtype=np.int64),
             "attention_mask": np.ones((1, n), dtype=np.int64),
             "token_type_ids": np.zeros((1, n), dtype=np.int64),
             "position_ids": np.arange(n, dtype=np.int64).reshape(1, n)}
    out = _sess.run(None, feeds)
    v = out[0][0, 0, :].astype(np.float32)
    return v / np.linalg.norm(v)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", type=int, required=True)
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--db", default="data/graphtell.sqlite")
    ap.add_argument("--embed-dir", default="data/embeddings")
    a = ap.parse_args()
    spec = json.load(open(a.cases, encoding="utf-8"))
    cases = [c for c in spec["cases"] if c["project"] == a.project and c.get("lang") == "zh"]
    db = sqlite3.connect(a.db)
    emb = json.load(open(os.path.join(a.embed_dir, f"{a.project}.json")))["vectors"]
    ids = list(emb.keys())
    mat = np.array([emb[i] for i in ids], dtype=np.float32)
    norms = np.linalg.norm(mat, axis=1, keepdims=True); norms[norms==0]=1
    mat = mat / norms

    bridge = build_bridge(db, a.project)
    inv = reverse_index(bridge)
    print(f"project {a.project}: bridge entries={len(bridge)}, reverse tokens={len(inv)}")
    sample = [t for t in ("coupon","payment","order","stock","balance") if t in inv]
    print("  sample reverse tokens:", sample)

    name_to_ids = {}
    for nid, name in db.execute("SELECT id,name FROM nodes WHERE project_id=?", (a.project,)):
        name_to_ids.setdefault(name.lower(), []).append(str(nid))

    for c in cases:
        q = c["query"]
        qterms = expand_query(q, bridge)
        qvec = embed(f"{q} {' '.join(qterms)}") if qterms else embed(q)
        sims = mat @ qvec
        order = np.argsort(-sims)
        # Current ranking threshold (cosine of the 20th)
        thr20 = sims[order[19]] if len(order) > 19 else -1
        true_ids = set()
        for t in c["targets"]:
            tl = t.lower()
            for nm, nids in name_to_ids.items():
                if tl in nm: true_ids.update(nids)
        # True answer's current rank
        cur_rank = None
        for r, idx in enumerate(order, 1):
            if ids[idx] in true_ids: cur_rank = r; break
        # Enrich the true answer node and re-encode
        new_cos = {}
        for t in c["targets"]:
            tl = t.lower()
            for nm, nids in name_to_ids.items():
                if tl in nm:
                    for nid in nids:
                        row = db.execute("SELECT name,fqn FROM nodes WHERE id=?", (int(nid),)).fetchone()
                        enr = node_enrichment(row[0], row[1], inv)
                        if not enr: continue
                        base = f"{row[0]} {' '.join(key_tokens(row[0]))}"
                        if row[1]: base += f" {row[1]} {' '.join(key_tokens(row[1]))}"
                        newtext = base + " " + " ".join(enr)
                        v = embed(newtext)
                        new_cos[nm] = (len(enr), new_cos.get(nm, (0,None))[1] or np.dot(v, qvec))
        best_enr = max(new_cos.items(), key=lambda kv: kv[1][1]) if new_cos else (None,(0,0))
        print(f"\n# {q}")
        print(f"    当前真答案名次={cur_rank}  第20名门槛余弦={thr20:.3f}")
        print(f"    富化后最佳余弦={best_enr[1][1]:.3f} (补{best_enr[1][0]}词: {best_enr[0]})")
        print(f"    -> 富化后能否进前20: {'✓' if best_enr[1][1] >= thr20 else '✗ (仍差)'}")


if __name__ == "__main__":
    main()
