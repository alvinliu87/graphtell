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
            "graphtell mcp: connected to service {} project #{}",
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
                    "description": "Recall code related to a natural-language / identifier prompt on the code graph; returns a compact Markdown context (file paths, line numbers, snippets, call relations). Injecting this context into a code-generating LLM replaces reading the whole repo or repeated greps and saves a lot of tokens. The context carries a **quality tier** at the top (high / medium / low + confidence): when the quality is \"low\" most feature terms missed and the top rows may be generic-term noise — **do not trust it directly**; search again with the feature terms listed at the end, or open the relevant files. A \"medium\" result may be incomplete, so consider an extra search.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {"type":"string","description":"Prompt; Chinese and English may be mixed, e.g. 'modify order discount' or 'payment callback'"},
                            "limit": {"type":"integer","description":"Maximum number of hits returned, default 20"},
                            "hops": {"type":"integer","description":"Hops to expand along call edges, default 2"},
                            "include_body": {"type":"boolean","description":"Also append the full source of the files touched by the hits (default false). When enabled the LLM can read the implementation directly instead of spending another round trip on a full read; only the top-ranked few files are returned and very large files are truncated"}
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "compose_prompt",
                    "description": "On top of the recalled code context, append the \"user task intent + quality constraints\" to produce a complete prompt that can be fed straight to a code-generating LLM. It shares the same server-side template as the Web UI prompt-augmentation page (/compose), so the result is identical; the LLM can start work immediately without assembling context and constraints itself.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {"type":"string","description":"Search terms used to recall code; Chinese and English may be mixed, e.g. 'modify order discount'"},
                            "intent": {"type":"string","description":"The user's task intent / extra context; leave empty to ask the LLM to infer it from the context"},
                            "limit": {"type":"integer","description":"Maximum number of recall hits, default 20"},
                            "hops": {"type":"integer","description":"Hops to expand along call edges, default 2"},
                            "with_snippets": {"type":"boolean","description":"Whether to include source snippets, default true"}
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "check_compliance",
                    "description": "Run a compliance check on the project (rule set already loaded into the graph) and return a severity rollup plus a violation list (file:line + reason). Lets the LLM understand compliance risk before / after a change.",
                    "inputSchema": {
                        "type":"object",
                        "properties": {
                            "rule_ids": {"type":"array","items":{"type":"string"},"description":"Run only the given rules; empty means every enabled rule"}
                        },
                        "required": []
                    }
                },
                {
                    "name": "list_violations",
                    "description": "Read the violations persisted by the most recent compliance check (does not re-run rules).",
                    "inputSchema": {
                        "type":"object",
                        "properties": {
                            "limit": {"type":"integer","description":"Maximum number of entries returned, default 200"}
                        },
                        "required": []
                    }
                },
                {
                    "name": "warmup_status",
                    "description": "Query the background semantic-vector warm-up progress for the current project. Returns whether warm-up finished (warmed), whether it is running (warming), and done / total node counts. The IDE can use this to tell whether recall is still on the cold path (weaker quality) and decide to retry later.",
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
            other => (format!("Unknown tool: {other}"), true),
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
            _ => return ("missing query parameter".to_string(), true),
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
                Err(e) => (format!("Failed to parse the recall response: {e}"), true),
            },
            Err(e) => (
                format!(
                    "Cannot reach the service ({}): {}\nPlease make sure `graphtell serve` is running.",
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
            _ => return ("missing query parameter".to_string(), true),
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
                Err(e) => (format!("Failed to parse the compose response: {e}"), true),
            },
            Err(e) => (
                format!(
                    "Cannot reach the service ({}): {}\nPlease make sure `graphtell serve` is running.",
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
            Err(e) => (format!("Cannot reach the service: {e}"), true),
        }
    }

    fn violations(&self, args: &Value) -> (String, bool) {
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(200);
        let path = format!("/api/projects/{}/violations?limit={limit}", self.project);
        match http_call(&self.base, &path, "GET", None) {
            Ok(resp) => format_violations(&resp),
            Err(e) => (format!("Cannot reach the service: {e}"), true),
        }
    }

    /// 查询当前工程的后台预热进度（语义向量 bge 计算）。
    fn warmup_status(&self) -> (String, bool) {
        let path = format!("/api/projects/{}/warmup", self.project);
        match http_call(&self.base, &path, "GET", None) {
            Ok(resp) => format_warmup(&resp),
            Err(e) => (format!("Cannot reach the service: {e}"), true),
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
            "\n\n---\n⏳ **Semantic vectors warming up ({done}/{total} done)**: recall is currently on the cold path (fast vector / lexical) and quality is weaker. Retry later for full semantic recall."
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
        "(no usable feature terms; try a more specific phrasing)".to_string()
    } else {
        missing
            .iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join("、")
    };
    match quality {
        "low" => format!(
            "{md}\n\n---\n⚠️ **Recall quality low (confidence {confidence:.2}) — do not rely on the context above alone.**\n\
             Search again with these feature terms: {kw}. If still unsure, open the relevant files and read them."
        ),
        "medium" => format!(
            "{md}\n\n---\nℹ️ **Recall quality medium (confidence {confidence:.2})**: results may be incomplete.\n\
             Unmatched feature terms: {kw}. Search a bit more before concluding."
        ),
        _ => md,
    }
}

fn extract_markdown(resp: &str) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(resp)?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error");
        anyhow::bail!("Service returned a failure: {err}");
    }
    let data = v.get("data").ok_or_else(|| anyhow::anyhow!("Response is missing data"))?;
    let md = data
        .get("markdown")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow::anyhow!("Response is missing markdown"))?;
    Ok(md.to_string())
}

/// 从 `/prompt` 响应里取出服务端合成好的提示词。
fn extract_prompt(resp: &str) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(resp)?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error");
        anyhow::bail!("Service returned a failure: {err}");
    }
    let data = v.get("data").ok_or_else(|| anyhow::anyhow!("Response is missing data"))?;
    let p = data
        .get("prompt")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow::anyhow!("Response is missing prompt"))?;
    Ok(p.to_string())
}

fn format_check(resp: &str) -> (String, bool) {
    let v: Value = match serde_json::from_str(resp) {
        Ok(v) => v,
        Err(e) => return (format!("Failed to parse the check response: {e}"), true),
    };
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error");
        return (format!("Service returned a failure: {err}"), true);
    }
    let d = match v.get("data") {
        Some(d) => d,
        None => return ("Response is missing data".into(), true),
    };
    let rules_run = d.get("rules_run").and_then(|x| x.as_u64()).unwrap_or(0);
    let dur = d.get("duration_ms").and_then(|x| x.as_u64()).unwrap_or(0);
    let sev = d.get("by_severity").cloned().unwrap_or(Value::Null);
    let mut out = format!("Compliance check: ran {rules_run} rules in {dur}ms\nSeverity: ");
    for k in ["critical", "error", "warning", "info"] {
        let n = sev.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        out.push_str(&format!("{k}={n} "));
    }
    out.push('\n');
    if let Some(vs) = d.get("violations").and_then(|x| x.as_array()) {
        let cap = 60;
        out.push_str(&format!("Violations (showing first {cap} of {}):\n", vs.len()));
        for v in vs.iter().take(cap) {
            out.push_str(&format_violation(v));
        }
    }
    (out, false)
}

fn format_violations(resp: &str) -> (String, bool) {
    let v: Value = match serde_json::from_str(resp) {
        Ok(v) => v,
        Err(e) => return (format!("Failed to parse: {e}"), true),
    };
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        return (format!("Service returned a failure: {err}"), true);
    }
    let vs = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return ("No violations".into(), false),
    };
    let mut out = format!("Violations ({} in total):\n", vs.len());
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
        Err(e) => return (format!("Failed to parse the warm-up response: {e}"), true),
    };
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        return (format!("Service returned a failure: {err}"), true);
    }
    let d = match v.get("data") {
        Some(d) => d,
        None => return ("No warm-up status".into(), false),
    };
    let warmed = d.get("warmed").and_then(|x| x.as_bool()).unwrap_or(false);
    let warming = d.get("warming").and_then(|x| x.as_bool()).unwrap_or(false);
    let done = d.get("done").and_then(|x| x.as_u64()).unwrap_or(0);
    let total = d.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
    let status = if warmed {
        "✅ warm-up finished (recall uses the full semantic path)".to_string()
    } else if warming {
        format!("⏳ warming up ({done}/{total} done); recall is on the cold path with weaker quality")
    } else {
        "❄️ not warmed yet (recall uses the cold path; the first query triggers a background warm-up)".to_string()
    };
    (format!("Warm-up status: {status}"), false)
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
        .ok_or_else(|| anyhow::anyhow!("Invalid service address: {base}"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("Invalid port: {port}"))?;
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
