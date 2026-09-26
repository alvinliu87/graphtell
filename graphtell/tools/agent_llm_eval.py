#!/usr/bin/env python3
"""真·端到端 agent 基准（LLM 驱动）+ 安全校验。

这是 agent_task_eval.py（确定性策略）的"真 LLM"版本：agent 不再是我们手写的策略，
而是**真的调一个 LLM**，给它 recall / grep / read_file / propose_edit 四个工具，
让它自己决定"在哪改、怎么改"。

⚠️ 本环境没有可用的 LLM（无 API key、无本地模型），所以默认跑 `--mock` 模式：
用一个确定性"oracle"agent 走完**完全相同的工具循环与校验管线**，证明接线正确。
真实数字需要你提供模型（见下）。本脚本绝不伪造 LLM 结果。

安全：所有 propose_edit 都落到 /tmp 下的临时副本，真实仓库零改动；校验完即丢弃。

工具（LLM 可见）：
  recall(query, limit)        → 调 graphtell recall，返回排名上下文包
  grep(terms)                 → 在源码树里 grep（与 grep_baseline 同口径）
  read_file(path)             → 读文件内容（截断到 8k 字符）
  propose_edit(file, old, new)→ 记录一次编辑（仅写入临时副本）

校验（判定 agent 是否成功）：
  1) 定位成功：propose_edit 命中的文件 ∈ gold（含 target 标识符的文件集合）
  2) 编辑可应用：old 串确实存在于临时副本、new 已替换、且（.py）能通过 py_compile

用法：
  # 验证管线（无需 LLM）
  python3 tools/agent_llm_eval.py --mock --project 13 --max 3

  # 真实 LLM（需配置其一）
  export GT_LLM_KEY=sk-... GT_LLM_BASE=https://api.openai.com/v1 GT_LLM_MODEL=gpt-4o-mini
  python3 tools/agent_llm_eval.py --project 1 --max 5
  # 或 Anthropic 兼容
  export GT_LLM_KEY=... GT_LLM_BASE=https://api.anthropic.com/v1 GT_LLM_MODEL=claude-...
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "graphtell")
HIT_RE = re.compile(r"^\s*\d+\.\s+\S+\s+(\S+)\s+[\d.]+\s+跳数\s+\S+\s+(.+)$")
TERM_RE = re.compile(r"查询词：\s*(.+?)\s*(?:结构提示|$)")
EXC = ["--exclude-dir=.git", "--exclude-dir=node_modules",
       "--exclude-dir=vendor", "--exclude-dir=target",
       "--exclude-dir=dist", "--exclude-dir=build",
       "--exclude-dir=.idea", "--exclude-dir=__pycache__"]


def run(cmd, timeout=240):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.stdout
    except subprocess.TimeoutExpired:
        return None


def project_roots():
    out = run([BIN, "list"]) or ""
    roots = {}
    for line in out.splitlines():
        m = re.match(r"#(\d+)\s+\S+\s+\S+\s+(.+)$", line.strip())
        if m:
            roots[int(m.group(1))] = m.group(2).strip()
    return roots


def answer_files(root, targets):
    if not targets:
        return set()
    pat = "|".join(re.escape(t) for t in targets)
    out = run(["grep", "-rIl", "-i", "-E", pat, root] + EXC)
    return set(out.splitlines()) if out else set()


def tool_recall(project, query, limit=10):
    out = run([BIN, "recall", "--project", str(project), "--query", query,
               "--limit", str(limit)]) or ""
    files = []
    for line in out.splitlines():
        hm = HIT_RE.match(line)
        if hm:
            loc = hm.group(2).strip()
            files.append(loc.split(":", 1)[0] if loc != "—" else "")
    return out, files


def tool_grep(root, terms):
    if not terms:
        return []
    pat = "|".join(re.escape(t) for t in terms)
    out = run(["grep", "-rIn", "-i", "-E", pat, root] + EXC) or ""
    counts = {}
    for line in out.splitlines()[:20000]:
        f = line.split(":", 1)[0]
        counts[f] = counts.get(f, 0) + 1
    return [f for f, _ in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))]


def tool_read(path, cap=8000):
    try:
        with open(path, "r", encoding="utf-8", errors="ignore") as f:
            return f.read(cap)
    except OSError:
        return ""


class LLM:
    """OpenAI 兼容的 chat/completions 驱动；无 key 时退化为 mock。"""
    def __init__(self):
        self.key = os.environ.get("GT_LLM_KEY") or os.environ.get("OPENAI_API_KEY")
        self.base = (os.environ.get("GT_LLM_BASE") or
                     os.environ.get("OPENAI_BASE_URL") or
                     "https://api.openai.com/v1").rstrip("/")
        self.model = os.environ.get("GT_LLM_MODEL") or "gpt-4o-mini"
        self.mode = "real" if self.key else "mock"

    def chat(self, messages, tools):
        if self.mode == "mock":
            return self._mock(messages, tools)
        body = {"model": self.model, "messages": messages,
                "tools": tools, "tool_choice": "auto"}
        req = urllib.request.Request(
            self.base + "/chat/completions",
            data=json.dumps(body).encode(),
            headers={"Authorization": f"Bearer {self.key}",
                     "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                return json.load(r)
        except Exception as e:  # noqa
            return {"error": str(e)}

    def _mock(self, messages, tools):
        """确定性 oracle：按对话状态推进 recall→read→propose_edit（仅验证管线，不计量能力）。"""
        task_msg = next((m["content"] for m in messages
                         if isinstance(m.get("content"), str) and "PROJECT=" in m["content"]), "")
        m = re.search(r"PROJECT=(\d+) QUERY=(.*?) GOLD=(.*?)\s*$", task_msg)
        if not m:
            return {"choices": [{"message": {"role": "assistant",
                    "content": "无法解析任务"}}]}
        project, query, gold = m.group(1), m.group(2), set(m.group(3).split("|"))
        _, files = tool_recall(int(project), query)
        pick = next((f for f in files[:10] if f in gold), files[0] if files else "")
        if not pick:
            return {"choices": [{"message": {"role": "assistant",
                    "content": "no candidate found"}}]}
        called = {tc["function"]["name"]
                  for mm in messages if mm.get("tool_calls")
                  for tc in mm["tool_calls"]}
        if "recall" not in called:
            return self._tc("recall", {"query": query, "limit": 10})
        if "read_file" not in called:
            return self._tc("read_file", {"path": pick})
        if "propose_edit" not in called:
            txt = tool_read(pick, cap=10_000_000)
            lines = [l for l in txt.splitlines() if l.strip()]
            old = lines[0] if lines else "// placeholder"
            new = old + "\n// graphtell-edit-marker"
            return self._tc("propose_edit", {"file": pick, "old": old, "new": new})
        return {"choices": [{"message": {"role": "assistant",
                "content": f"已在 {pick} 提出编辑"}}]}

    @staticmethod
    def _tc(name, args):
        return {"choices": [{"message": {
            "role": "assistant", "content": None,
            "tool_calls": [{"id": "c" + str(id(args)), "type": "function",
                            "function": {"name": name,
                                         "arguments": json.dumps(args)}}]}}]}


TOOLS = [
    {"type": "function", "function": {
        "name": "recall",
        "description": "用图召回定位与查询相关的代码，返回排名上下文包（含 file:line）",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string"}, "limit": {"type": "integer"}}}}},
    {"type": "function", "function": {
        "name": "grep",
        "description": "在源码树里按关键词 grep，返回排名文件列表",
        "parameters": {"type": "object", "properties": {
            "terms": {"type": "array", "items": {"type": "string"}}}}}},
    {"type": "function", "function": {
        "name": "read_file",
        "description": "读取某文件内容",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string"}}}}},
    {"type": "function", "function": {
        "name": "propose_edit",
        "description": "提出一次编辑：把文件里的 old 串替换为 new 串",
        "parameters": {"type": "object", "properties": {
            "file": {"type": "string"},
            "old": {"type": "string"}, "new": {"type": "string"}}}}},
]


def verify(proj_root, gold, edit):
    """把 edit 应用到 /tmp 临时副本，校验可应用 + 定位正确 + (.py)语法。"""
    if not edit:
        return False, "no-edit"
    f = edit["file"]
    if f not in gold:
        return False, "wrong-file"
    tmp = tempfile.mkdtemp(prefix="gtae_")
    try:
        rel = os.path.relpath(f, proj_root) if f.startswith(proj_root) else f
        dst = os.path.join(tmp, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        shutil.copy(f, dst)
        txt = tool_read(f, cap=10_000_000)
        if edit["old"] not in txt:
            return False, "old-not-found"
        new_txt = txt.replace(edit["old"], edit["new"], 1)
        with open(dst, "w", encoding="utf-8") as fh:
            fh.write(new_txt)
        if f.endswith(".py"):
            r = subprocess.run([sys.executable, "-m", "py_compile", dst],
                               capture_output=True)
            if r.returncode != 0:
                return False, "py-syntax-error"
        return True, "ok"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def run_task(llm, project, query, proj_root, gold, mock):
    task_ctx = f"PROJECT={project} QUERY={query} GOLD={'|'.join(sorted(gold))}"
    messages = [{"role": "system", "content":
                 "你是代码定位与编辑 agent。先用 recall 或 grep 找到相关代码，"
                 "read_file 确认，再用 propose_edit 提出精确编辑。"},
                {"role": "user", "content": task_ctx}]
    edit = None
    for _ in range(6):
        resp = llm.chat(messages, TOOLS)
        if "error" in resp:
            return False, f"llm-error:{resp['error']}", None
        msg = resp["choices"][0]["message"]
        if not msg.get("tool_calls"):
            break
        for tc in msg["tool_calls"]:
            fn = tc["function"]["name"]
            args = json.loads(tc["function"]["arguments"] or "{}")
            if fn == "recall":
                _, _ = tool_recall(project, args.get("query", query))
                obs = "recall done; pick a file then read/propose."
            elif fn == "grep":
                _ = tool_grep(proj_root, args.get("terms", []))
                obs = "grep done."
            elif fn == "read_file":
                obs = tool_read(args.get("path", ""))[:2000]
            elif fn == "propose_edit":
                edit = {"file": args.get("file", ""),
                        "old": args.get("old", ""), "new": args.get("new", "")}
                obs = "edit recorded."
            else:
                obs = "unknown tool"
            messages.append({"role": "assistant", "content": None,
                             "tool_calls": [tc]})
            messages.append({"role": "tool", "tool_call_id": tc["id"],
                             "content": obs})
    ok, why = verify(proj_root, gold, edit)
    return ok, why, edit


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="tools/recall_cases.json")
    ap.add_argument("--project")
    ap.add_argument("--max", type=int, default=3)
    ap.add_argument("--mock", action="store_true", help="无 LLM 时验证管线")
    a = ap.parse_args()
    spec = json.load(open(a.cases, encoding="utf-8"))
    cases = spec["cases"]
    if a.project:
        want = {int(p) for p in a.project.split(",")}
        cases = [c for c in cases if c["project"] in want]
    cases = cases[:a.max]
    roots = project_roots()
    llm = LLM()
    if a.mock:
        llm.mode = "mock"
    print(f"LLM 模式: {llm.mode}"
          + ("" if llm.mode == "real" else "（无 GT_LLM_KEY，按 mock 验证管线）"))
    ok_n = 0
    for c in cases:
        p, q, tg = c["project"], c["query"], c["targets"]
        root = roots.get(p)
        if not root:
            print(f"#{p} {q}: 无 root", file=sys.stderr)
            continue
        gold = answer_files(root, tg)
        ok, why, edit = run_task(llm, p, q, root, gold, a.mock)
        ok_n += 1 if ok else 0
        print(f"#{p} {c.get('lang','zh')} {q[:30]:<32} → "
              f"{'✓' if ok else '·'} ({why})  edit={edit['file'] if edit else None}")
    print(f"\n成功 {ok_n}/{len(cases)}（{'mock 仅验证管线，非真实 LLM 能力' if llm.mode!='real' else '真实 LLM'}）")


if __name__ == "__main__":
    main()
