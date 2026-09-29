//! MCP(stdio) 桥接：IDE 启动 `graphtell mcp` 子进程，经 HTTP 连常驻服务。
//!
//! 设计要点：
//! * stdout **只**承载 JSON-RPC 行；任何诊断日志都走 stderr（见 `main` 里 tracing 写 stderr）。
//! * 不引入 HTTP 客户端 crate（离线构建安全）：`http_call` 用 `std::net::TcpStream` 手写
//!   本地 HTTP/1.1 请求，足够对接常驻的 axum 服务。
//! * 暴露四个工具：
//!   - `recall_code`：按提示词召回相关代码，返回紧凑 Markdown 上下文（路径/行号/片段），
//!     让 IDE 里的 LLM 注入这段而非自行读全仓，节省 token。
//!   - `compose_prompt`：在召回上下文之上再拼上「用户意图 + 质量约束」，产出一段可直接
//!     投喂代码生成 LLM 的完整提示词；与 Web UI 的 `/compose` 共用服务端同一份模板。
//!   - `check_compliance`：跑合规检查，返回严重度汇总 + 违规清单。
//!   - `list_violations`：读取最近一次落库的违规。

use std::io::{BufRead, BufWriter, Read, Write};

use serde_json::{json, Value};

/// MCP 桥：记住常驻服务地址与目标工程。
pub struct McpBridge {
    base: String,
    project: i64,
}

impl McpBridge {
    pub fn new(base: String, project: i64) -> Self {
        Self { base, project }
    }

    /// 从 stdin 逐行读 JSON-RPC，把响应写回 stdout；EOF 退出。
    pub fn run(&self) -> anyhow::Result<()> {
        eprintln!(
            "graphtell mcp: 连接常驻服务 {} 工程 #{}",
            self.base, self.project
        );
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut out = BufWriter::new(stdout.lock());
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(resp) = self.handle(line) {
                let s = serde_json::to_string(&resp)?;
                out.write_all(s.as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
        }
        Ok(())
    }

    /// 处理一行 JSON-RPC 请求；通知类（无 id）返回 `None` 表示不回包。
    fn handle(&self, line: &str) -> Option<Value> {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                }));
            }
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = v.get("params").cloned().unwrap_or(Value::Null);

        // 通知类：不回包。
        if method.starts_with("notifications/") {
            return None;
        }

        let result = match method {
            "initialize" => Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "graphtell", "version": env!("CARGO_PKG_VERSION")}
            })),
            "ping" => Some(json!({})),
            "tools/list" => Some(self.tools_list()),
            "tools/call" => self.tools_call(&params),
            _ => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("method not found: {method}")}
                }));
            }
        };

        result.map(|r| json!({ "jsonrpc": "2.0", "id": id, "result": r }))
    }

    fn tools_list(&self) -> Value {
        json!({
            "tools": [
                {
                    "name": "recall_code",
                    "description": "按自然语言/标识符提示词在代码图上召回相关代码，返回紧凑的 Markdown 上下文（文件路径、行号、片段、调用关系）。把这段上下文注入给生成代码的 LLM，可替代它自行读取全仓或多次 grep，显著节省 token。返回的上下文顶部带**质量档位**（高/中/低 + 置信度）：当质量为「低」时说明多数特征词未命中、前排可能是泛词噪声，**不要直接采信**，应按文末给出的特征词自行检索或直接阅读相关文件；质量为「中」时结果可能不完整，建议补充检索。",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {"type":"string","description":"提示词，可中英文混写，如「修改订单优惠」「payment callback」"},
                            "limit": {"type":"integer","description":"返回命中上限，默认 20"},
                            "hops": {"type":"integer","description":"沿调用边扩展跳数，默认 2"},
                            "include_body": {"type":"boolean","description":"是否把命中涉及的完整文件源码也一并附上（默认 false）。开启后 LLM 可直接阅读实现，省去再发 read 拉全文的一轮往返；仅返回排名最前的少数文件，超大文件会被截断"}
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "compose_prompt",
                    "description": "在召回到的相关代码上下文之上再拼上「用户任务意图 + 质量约束」，产出一段可直接投喂代码生成 LLM 的完整提示词。与 Web UI 的「提示词增强」页（/compose）共用服务端同一套模板，结果完全一致；LLM 拿着它可直接开工，不必自行拼装上下文与约束。",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {"type":"string","description":"用于召回代码的检索词，可中英文混写，如「修改订单优惠」"},
                            "intent": {"type":"string","description":"用户的任务意图/补充说明；留空则要求 LLM 依据上下文推断"},
                            "limit": {"type":"integer","description":"召回命中上限，默认 20"},
                            "hops": {"type":"integer","description":"沿调用边扩展跳数，默认 2"},
                            "with_snippets": {"type":"boolean","description":"是否附带源码片段，默认 true"}
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "check_compliance",
                    "description": "对工程跑合规检查（已建图的规则集），返回严重度汇总与违规清单（文件:行 + 原因）。用于让 LLM 在改动前/后了解合规风险。",
                    "inputSchema": {
                        "type":"object",
                        "properties": {
                            "rule_ids": {"type":"array","items":{"type":"string"},"description":"只跑指定规则；留空跑全部启用规则"}
                        },
                        "required": []
                    }
                },
                {
                    "name": "list_violations",
                    "description": "读取最近一次合规检查落库的违规（不重跑规则）。",
                    "inputSchema": {
                        "type":"object",
                        "properties": {
                            "limit": {"type":"integer","description":"最多返回条数，默认 200"}
                        },
                        "required": []
                    }
                },
                {
                    "name": "warmup_status",
                    "description": "查询当前工程的后台语义向量预热进度。返回是否已预热完成（warmed）、是否正在预热（warming）以及已完成/总节点数。IDE 可据此判断召回是否还在走冷路径（质量偏弱），决定是否稍后重试。",
                    "inputSchema": {
                        "type":"object",
                        "properties": {},
                        "required": []
                    }
                }
            ]
        })
    }

    fn tools_call(&self, params: &Value) -> Option<Value> {
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let (text, is_error) = match name {
            "recall_code" => self.recall(&args),
            "compose_prompt" => self.compose(&args),
            "check_compliance" => self.check(&args),
            "list_violations" => self.violations(&args),
            "warmup_status" => self.warmup_status(),
            other => (format!("未知工具: {other}"), true),
        };
        Some(json!({
            "content": [{"type":"text","text": text}],
            "isError": is_error
        }))
    }

    // ---- 工具实现 ----

    fn recall(&self, args: &Value) -> (String, bool) {
        let query = match args.get("query").and_then(|q| q.as_str()) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return ("缺少 query 参数".to_string(), true),
        };
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
        let hops = args.get("hops").and_then(|h| h.as_u64()).unwrap_or(2) as u32;
        let include_body = args
            .get("include_body")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let body = json!({
            "query": query,
            "limit": limit,
            "hops": hops,
            "kinds": [],
            "with_snippets": true,
            "include_body": include_body
        })
        .to_string();
        let path = format!("/api/projects/{}/recall", self.project);
        match http_call(&self.base, &path, "POST", Some(&body)) {
            Ok(resp) => match extract_markdown(&resp) {
                Ok(md) => {
                    let (quality, confidence, missing) = extract_quality(&resp);
                    let warm_note = extract_warmup_note(&resp);
                    (
                        with_quality_guidance(md, &quality, confidence, &missing) + &warm_note,
                        false,
                    )
                }
                Err(e) => (format!("解析召回响应失败: {e}"), true),
            },
            Err(e) => (
                format!(
                    "连接常驻服务失败（{}）：{}\n请确认 `graphtell serve` 正在运行。",
                    self.base, e
                ),
                true,
            ),
        }
    }

    /// 合成提示词：走服务端 `/prompt` 端点，与 Web UI 的 `/compose` 共用同一套模板，
    /// 因此两处产出的提示词完全一致（此处不另复制一份模板）。
    fn compose(&self, args: &Value) -> (String, bool) {
        let query = match args.get("query").and_then(|q| q.as_str()) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return ("缺少 query 参数".to_string(), true),
        };
        let intent = args
            .get("intent")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
        let hops = args.get("hops").and_then(|h| h.as_u64()).unwrap_or(2) as u32;
        let with_snippets = args
            .get("with_snippets")
            .and_then(|b| b.as_bool())
            .unwrap_or(true);
        let body = json!({
            "query": query,
            "intent": intent,
            "limit": limit,
            "hops": hops,
            "with_snippets": with_snippets
        })
        .to_string();
        let path = format!("/api/projects/{}/prompt", self.project);
        match http_call(&self.base, &path, "POST", Some(&body)) {
            Ok(resp) => match extract_prompt(&resp) {
                Ok(p) => (p, false),
                Err(e) => (format!("解析合成响应失败: {e}"), true),
            },
            Err(e) => (
                format!(
                    "连接常驻服务失败（{}）：{}\n请确认 `graphtell serve` 正在运行。",
                    self.base, e
                ),
                true,
            ),
        }
    }

    fn check(&self, args: &Value) -> (String, bool) {
        let rule_ids = args
            .get("rule_ids")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            });
        let body = json!({ "rule_ids": rule_ids }).to_string();
        let path = format!("/api/projects/{}/check", self.project);
        match http_call(&self.base, &path, "POST", Some(&body)) {
            Ok(resp) => format_check(&resp),
            Err(e) => (format!("连接常驻服务失败: {e}"), true),
        }
    }

    fn violations(&self, args: &Value) -> (String, bool) {
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(200);
        let path = format!("/api/projects/{}/violations?limit={limit}", self.project);
        match http_call(&self.base, &path, "GET", None) {
            Ok(resp) => format_violations(&resp),
            Err(e) => (format!("连接常驻服务失败: {e}"), true),
        }
    }

    /// 查询当前工程的后台预热进度（语义向量 bge 计算）。
    fn warmup_status(&self) -> (String, bool) {
        let path = format!("/api/projects/{}/warmup", self.project);
        match http_call(&self.base, &path, "GET", None) {
            Ok(resp) => format_warmup(&resp),
            Err(e) => (format!("连接常驻服务失败: {e}"), true),
        }
    }
}

// ---------------------------------------------------------------- 响应格式化

/// 抽取召回的**质量元数据**（quality / confidence / missing_terms）。
///
/// 单独一个函数而非塞进 [`extract_markdown`]：markdown 解析失败时仍能独立诊断，
/// 也让 MCP 侧可以按档位追加"下一步该做什么"。缺省按 high 处理（不打扰正常结果）。
fn extract_quality(resp: &str) -> (String, f64, Vec<String>) {
    let Ok(v) = serde_json::from_str::<Value>(resp) else {
        return ("high".to_string(), 1.0, Vec::new());
    };
    let Some(data) = v.get("data") else {
        return ("high".to_string(), 1.0, Vec::new());
    };
    let quality = data
        .get("quality")
        .and_then(|q| q.as_str())
        .unwrap_or("high")
        .to_string();
    let confidence = data.get("confidence").and_then(|c| c.as_f64()).unwrap_or(1.0);
    let missing = data
        .get("missing_terms")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    (quality, confidence, missing)
}

/// 从召回响应抽取后台预热状态，并在「预热中」时给出提示，让 IDE 知道召回质量可能偏弱。
fn extract_warmup_note(resp: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(resp) else {
        return String::new();
    };
    let Some(data) = v.get("data") else {
        return String::new();
    };
    let Some(w) = data.get("warmup") else {
        return String::new();
    };
    let warmed = w.get("warmed").and_then(|x| x.as_bool()).unwrap_or(false);
    let warming = w.get("warming").and_then(|x| x.as_bool()).unwrap_or(false);
    if warming && !warmed {
        let done = w.get("done").and_then(|x| x.as_u64()).unwrap_or(0);
        let total = w.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
        return format!(
            "\n\n---\n⏳ **语义向量预热中（已完成 {done}/{total}）**：当前召回暂走冷路径（快速向量/词面），质量偏弱。稍后重试可获得完整语义召回。"
        );
    }
    String::new()
}

/// 按质量档位追加"下一步该做什么"。
///
/// **只提示，绝不丢弃已召回的片段** —— 片段仍有参考价值，扔掉已付出的检索成本
/// 并不划算。目的是避免"拿着噪声当证据"的静默失败：低质量时明确要求调用方
/// 改用给出的特征词自行检索 / 直接阅读文件。
fn with_quality_guidance(
    md: String,
    quality: &str,
    confidence: f64,
    missing: &[String],
) -> String {
    let kw = if missing.is_empty() {
        "（无可用特征词，建议换更具体的说法重试）".to_string()
    } else {
        missing
            .iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join("、")
    };
    match quality {
        "low" => format!(
            "{md}\n\n---\n⚠️ **召回质量低（置信度 {confidence:.2}）—— 不要只依赖以上上下文。**\n\
             请改用这些特征词自行检索：{kw}。仍无法确定时，直接打开相关文件阅读。"
        ),
        "medium" => format!(
            "{md}\n\n---\nℹ️ **召回质量中等（置信度 {confidence:.2}）**：结果可能不完整。\n\
             未命中的特征词：{kw}。建议补充检索后再下结论。"
        ),
        _ => md,
    }
}

fn extract_markdown(resp: &str) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(resp)?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("未知错误");
        anyhow::bail!("服务返回失败: {err}");
    }
    let data = v.get("data").ok_or_else(|| anyhow::anyhow!("响应缺少 data"))?;
    let md = data
        .get("markdown")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow::anyhow!("响应缺少 markdown"))?;
    Ok(md.to_string())
}

/// 从 `/prompt` 响应里取出服务端合成好的提示词。
fn extract_prompt(resp: &str) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(resp)?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("未知错误");
        anyhow::bail!("服务返回失败: {err}");
    }
    let data = v.get("data").ok_or_else(|| anyhow::anyhow!("响应缺少 data"))?;
    let p = data
        .get("prompt")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow::anyhow!("响应缺少 prompt"))?;
    Ok(p.to_string())
}

fn format_check(resp: &str) -> (String, bool) {
    let v: Value = match serde_json::from_str(resp) {
        Ok(v) => v,
        Err(e) => return (format!("解析检查响应失败: {e}"), true),
    };
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("未知错误");
        return (format!("服务返回失败: {err}"), true);
    }
    let d = match v.get("data") {
        Some(d) => d,
        None => return ("响应缺少 data".into(), true),
    };
    let rules_run = d.get("rules_run").and_then(|x| x.as_u64()).unwrap_or(0);
    let dur = d.get("duration_ms").and_then(|x| x.as_u64()).unwrap_or(0);
    let sev = d.get("by_severity").cloned().unwrap_or(Value::Null);
    let mut out = format!("合规检查：运行 {rules_run} 条规则，耗时 {dur}ms\n严重度：");
    for k in ["critical", "error", "warning", "info"] {
        let n = sev.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        out.push_str(&format!("{k}={n} "));
    }
    out.push('\n');
    if let Some(vs) = d.get("violations").and_then(|x| x.as_array()) {
        let cap = 60;
        out.push_str(&format!("违规（显示前 {cap} 条，共 {} 条）：\n", vs.len()));
        for v in vs.iter().take(cap) {
            out.push_str(&format_violation(v));
        }
    }
    (out, false)
}

fn format_violations(resp: &str) -> (String, bool) {
    let v: Value = match serde_json::from_str(resp) {
        Ok(v) => v,
        Err(e) => return (format!("解析失败: {e}"), true),
    };
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        return (format!("服务返回失败: {err}"), true);
    }
    let vs = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return ("无违规".into(), false),
    };
    let mut out = format!("违规（共 {} 条）：\n", vs.len());
    for v in vs.iter().take(200) {
        out.push_str(&format_violation(v));
    }
    (out, false)
}

fn format_violation(v: &Value) -> String {
    let sev = v.get("severity").and_then(|x| x.as_str()).unwrap_or("?");
    let rid = v.get("rule_id").and_then(|x| x.as_str()).unwrap_or("?");
    let file = v.get("file").and_then(|x| x.as_str()).unwrap_or("-");
    let line = v.get("line").and_then(|x| x.as_u64()).unwrap_or(0);
    let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
    format!("[{sev}] {rid} {file}:{line} — {msg}\n")
}

/// 把 `/warmup` 响应格式化为人话状态。
fn format_warmup(resp: &str) -> (String, bool) {
    let v: Value = match serde_json::from_str(resp) {
        Ok(v) => v,
        Err(e) => return (format!("解析预热响应失败: {e}"), true),
    };
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        return (format!("服务返回失败: {err}"), true);
    }
    let d = match v.get("data") {
        Some(d) => d,
        None => return ("无预热状态".into(), false),
    };
    let warmed = d.get("warmed").and_then(|x| x.as_bool()).unwrap_or(false);
    let warming = d.get("warming").and_then(|x| x.as_bool()).unwrap_or(false);
    let done = d.get("done").and_then(|x| x.as_u64()).unwrap_or(0);
    let total = d.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
    let status = if warmed {
        "✅ 已预热完成（召回走完整语义路）".to_string()
    } else if warming {
        format!("⏳ 预热中（已完成 {done}/{total}），召回暂走冷路径质量偏弱")
    } else {
        "❄️ 未预热（召回走冷路径；首次查询会触发后台预热）".to_string()
    };
    (format!("预热状态：{status}"), false)
}

// ---------------------------------------------------------------- 极简本地 HTTP 客户端

/// 向常驻服务发一次 HTTP/1.1 请求，返回响应体（已处理 chunked）。
fn http_call(base: &str, path: &str, method: &str, body: Option<&str>) -> anyhow::Result<String> {
    let (host, port) = parse_base(base)?;
    let req = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        ),
    };
    let mut stream = std::net::TcpStream::connect((host.as_str(), port))?;
    stream.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (headers, body_str) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => return Ok(text.to_string()),
    };
    if headers.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        Ok(decode_chunked(&body_str))
    } else {
        Ok(body_str)
    }
}

/// 解析 `http://host:port` 或 `host:port`。
fn parse_base(base: &str) -> anyhow::Result<(String, u16)> {
    let s = base.trim_end_matches('/').strip_prefix("http://").unwrap_or(base);
    let (host, port) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("非法服务地址: {base}"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("非法端口: {port}"))?;
    Ok((host.to_string(), port))
}

/// 解码 HTTP chunked 传输编码。
fn decode_chunked(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        let line = match rest.find("\r\n") {
            Some(i) => &rest[..i],
            None => break,
        };
        let size = match line.split(';').next().unwrap_or("").trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => break,
        };
        let after = &rest[line.len() + 2..];
        if after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
    out
}
