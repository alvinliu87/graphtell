#!/usr/bin/env python3
"""查询侧扩展探针：验证「给中文查询补充英文代码 token」能否把真答案拉进余弦前 k。

只改查询编码文本，不动节点向量 —— 因此无需重预热，可在此快速验证方向是否成立。
命中口径与 cosine_diag.py 一致（节点名包含任一 target 即真答案）。

用法：
    python3 tools/query_expand_probe.py            # 全部工程
    python3 tools/query_expand_probe.py --project 1
"""
import argparse
import json
import os
import sqlite3
import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ONNX = os.path.join(ROOT, "models", "bge-m3-onnx", "model.onnx")
TOK = os.path.join(ROOT, "models", "bge-m3", "tokenizer.json")
PREFIX = "Represent this sentence for searching relevant passages: "
KS = (5, 10, 20)

# 通用中文→英文「代码词」词典（与具体业务无关，跨任意代码库都成立）。
# 只收高频通用技术/动作词；不收具体业务逻辑，保持工具通用性。
LEXICON = {
    # 动作
    "查询": "query find search", "列表": "list", "分页": "pagination page",
    "获取": "get fetch obtain", "加载": "load", "读取": "read", "拉取": "fetch pull",
    "保存": "save persist store", "新增": "add create insert", "创建": "create",
    "添加": "add", "删除": "delete remove", "移除": "remove", "修改": "update modify edit",
    "更新": "update", "设置": "set configure", "编辑": "edit", "提交": "submit commit",
    "撤销": "revoke cancel", "回滚": "rollback", "校验": "validate verify check",
    "验证": "validate", "检查": "check", "拦截": "intercept block", "过滤": "filter",
    "搜索": "search", "导出": "export", "导入": "import", "上传": "upload",
    "下载": "download", "发送": "send", "接收": "receive", "通知": "notify",
    "回调": "callback", "登录": "login signin", "登出": "logout", "注册": "register signup",
    "认证": "authenticate auth", "授权": "authorize oauth", "计算": "compute calculate",
    "生成": "generate", "转换": "convert transform", "解析": "parse", "序列化": "serialize",
    "加密": "encrypt", "解密": "decrypt", "排序": "sort order", "统计": "count statistics",
    "汇总": "aggregate", "重置": "reset", "刷新": "refresh",
    # 实体/概念
    "用户": "user", "管理员": "admin", "角色": "role", "权限": "permission",
    "订单": "order", "商品": "product item goods", "库存": "stock inventory",
    "余额": "balance", "优惠": "coupon discount", "折扣": "discount", "支付": "pay payment",
    "金额": "amount price", "价格": "price", "账单": "bill invoice", "评论": "comment",
    "文章": "article post", "博客": "blog", "品牌": "brand", "分类": "category",
    "标签": "tag", "令牌": "token", "票据": "ticket", "凭证": "credential",
    "会话": "session", "缓存": "cache", "配置": "config configuration", "字典": "dict dictionary",
    "数据": "data", "文件": "file", "图片": "image", "视频": "video",
    "消息": "message", "日志": "log", "错误": "error", "异常": "exception",
    "任务": "task job", "服务": "service", "接口": "api interface", "模型": "model",
    "实体": "entity", "字段": "field", "属性": "property attribute",
    "第三方": "third party oauth", "访问": "access", "工厂": "factory",
    "仓库": "repository repo", "控制器": "controller", "路由": "route router",
    "工作台": "workbench dashboard",
}


def expand(query):
    toks = []
    for zh, en in LEXICON.items():
        if zh in query:
            toks.append(en)
    return " ".join(toks)


_sess = None
_tok = None


def embed(text):
    global _sess, _tok
    if _sess is None:
        _sess = ort.InferenceSession(ONNX, providers=["CPUExecutionProvider"])
        _tok = Tokenizer.from_file(TOK)
    ids = _tok.encode(PREFIX + text).ids
    n = len(ids)
    feeds = {
        "input_ids": np.array([ids], dtype=np.int64),
        "attention_mask": np.ones((1, n), dtype=np.int64),
        "token_type_ids": np.zeros((1, n), dtype=np.int64),
        "position_ids": np.arange(n, dtype=np.int64).reshape(1, n),
    }
    out = _sess.run(None, feeds)
    vec = out[0][0, 0, :].astype(np.float32)
    return vec / np.linalg.norm(vec)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--embed-dir", default="data/embeddings")
    ap.add_argument("--db", default="data/graphtell.sqlite")
    ap.add_argument("--project")
    ap.add_argument("--only-miss", action="store_true", help="只看基线漏掉的用例")
    a = ap.parse_args()

    spec = json.load(open(a.cases, encoding="utf-8"))
    cases = spec["cases"]
    if a.project:
        want = {int(p) for p in a.project.split(",")}
        cases = [c for c in cases if c["project"] in want]

    db = sqlite3.connect(a.db)
    proj_cache = {}

    def load(pid):
        if pid in proj_cache:
            return proj_cache[pid]
        emb = json.load(open(os.path.join(a.embed_dir, f"{pid}.json")))["vectors"]
        ids = list(emb.keys())
        mat = np.array([emb[i] for i in ids], dtype=np.float32)
        norms = np.linalg.norm(mat, axis=1, keepdims=True)
        norms[norms == 0] = 1.0
        mat = mat / norms
        name_to_ids = {}
        for nid, name in db.execute("SELECT id,name FROM nodes WHERE project_id=?", (pid,)):
            name_to_ids.setdefault(name.lower(), []).append(str(nid))
        proj_cache[pid] = (ids, mat, name_to_ids)
        return proj_cache[pid]

    print(f"{'工程':<5}{'语':<4}{'查询':<22}{'基线名次':<8}{'扩展名次':<8}{'扩展@5/10/20'}")
    print("-" * 78)
    improved = 0
    rescued = 0
    total_miss = 0
    for c in cases:
        pid = c["project"]
        ids, mat, name_to_ids = load(pid)
        true_ids = set()
        for t in c["targets"]:
            tl = t.lower()
            for nm, nids in name_to_ids.items():
                if tl in nm:
                    true_ids.update(nids)
        q = c["query"]
        qv = embed(q)
        sims = mat @ qv
        order = np.argsort(-sims)
        base_rank = None
        for r, idx in enumerate(order, 1):
            if ids[idx] in true_ids:
                base_rank = r
                break
        # 扩展
        ex = expand(q)
        qe = embed(f"{q} {ex}") if ex else qv
        sims2 = mat @ qe
        order2 = np.argsort(-sims2)
        new_rank = None
        for r, idx in enumerate(order2, 1):
            if ids[idx] in true_ids:
                new_rank = r
                break
        hit = {k: (new_rank is not None and new_rank <= k) for k in KS}
        base_hit = base_rank is not None and base_rank <= 20
        if not base_hit:
            total_miss += 1
            if a.only_miss:
                pass
            if new_rank is not None and new_rank <= 20:
                rescued += 1
        if (base_rank or 9999) > (new_rank or 9999):
            improved += 1
        br = str(base_rank) if base_rank else "—"
        nr = str(new_rank) if new_rank else "—"
        h = "".join(f"{'✓' if hit[k] else '·'}" for k in KS)
        print(f"#{pid:<4}{c.get('lang','zh'):<4}{q[:20]:<22}{br:<8}{nr:<8}{h}")
    print(f"\n基线漏掉 @20 的用例: {total_miss}; 经扩展救回(进前20): {rescued}; 名次下降(变好)的: {improved}")


if __name__ == "__main__":
    main()
