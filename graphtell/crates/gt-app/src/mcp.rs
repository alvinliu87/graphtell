//! MCP (stdio) bridge: the IDE launches the `graphtell mcp` child process, which connects to the resident service over HTTP.
//!
//! Design notes:
//! * stdout carries **only** JSON-RPC lines; every diagnostic log goes to stderr (see tracing writing to stderr in `main`).
//! * No HTTP-client crate is pulled in (offline build safety): `http_call` hand-writes a local HTTP/1.1 request with
//!   `std::net::TcpStream`, which is enough to talk to the resident axum service.
//! * Four tools are exposed:
//!   - `recall_code`: recall relevant code by the prompt and return compact Markdown context (path / line / snippet),
//!     so the LLM inside the IDE injects this instead of reading the whole repo, saving tokens.
//!   - `compose_prompt`: on top of the recalled context, append "user intent + quality constraints" to produce a complete
//!     prompt ready to feed a code-generation LLM; shares the same server-side template with the Web UI's `/compose`.
//!   - `check_compliance`: run the compliance check and return a severity rollup + violation list.
//!   - `list_violations`: read the most recently persisted violations.

use std::io::{BufRead, BufWriter, Read, Write};

use serde_json::{json, Value};

/// The MCP bridge: remembers the resident service address and the target project.
pub struct McpBridge {
    base: String,
    project: i64,
}

impl McpBridge {
    pub fn new(base: String, project: i64) -> Self {
        Self { base, project }
    }

    /// Read JSON-RPC line by line from stdin, write responses back to stdout; exit on EOF.
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

    /// Handle one JSON-RPC request line; a notification (no id) returns `None` meaning no reply is sent.
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

        // Notification: no reply.
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

    // ---- Tool implementations ----

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

    /// Compose the prompt: go through the server-side `/prompt` endpoint, sharing the same template with the Web UI's
    /// `/compose`, so the two places produce identical prompts (no second template is copied here).
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

    /// Query the background warm-up progress of the current project (semantic bge vector computation).
    fn warmup_status(&self) -> (String, bool) {
        let path = format!("/api/projects/{}/warmup", self.project);
        match http_call(&self.base, &path, "GET", None) {
            Ok(resp) => format_warmup(&resp),
            Err(e) => (format!("Cannot reach the service: {e}"), true),
        }
    }
}

// ---------------------------------------------------------------- Response formatting

/// Extract the recall's **quality metadata** (quality / confidence / missing_terms).
///
/// A separate function rather than stuffed into [`extract_markdown`]: it can still be diagnosed independently when
/// Markdown parsing fails, and lets the MCP side append "what to do next" by tier. Defaults to high (does not disturb normal results).
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

/// Extract the background warm-up status from the recall response, and hint when "warming up" so the IDE knows recall quality may be weak.
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

/// Append "what to do next" by quality tier.
///
/// **Only hint, never drop the already-recalled snippet** — the snippet is still informative, and discarding the
/// retrieval cost already paid is not worthwhile. The goal is to avoid a silent failure of "taking noise as evidence":
/// at low quality, explicitly ask the caller to search with the given feature terms / read the file directly.
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

/// Take the server-composed prompt from the `/prompt` response.
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

/// Format the `/warmup` response into a human-readable status.
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

// ---------------------------------------------------------------- Minimal local HTTP client

/// Send one HTTP/1.1 request to the resident service and return the body (chunked already handled).
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

/// Parse `http://host:port` or `host:port`.
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

/// Decode HTTP chunked transfer encoding.
fn decode_chunked(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        let line = match rest.find("\r\n") {
            Some(i) => &rest[..i],
            None => break,
        };
        // HTTP/1.1 chunk size is **hexadecimal** (RFC 7230 §4.1). Parsing it as decimal breaks every chunk whose
        // size contains a-f (e.g. `d` = 13) and mis-sizes every chunk >= 16 (e.g. `10` = 16 bytes, not 10),
        // which silently truncates the recall Markdown returned to the IDE.
        let size = match usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The base never connects in these tests: only the non-network code paths are exercised.
    fn bridge() -> McpBridge {
        McpBridge::new("http://127.0.0.1:5177".to_string(), 1)
    }

    // ---- parse_base: resident-service address parsing ----

    #[test]
    fn parse_base_accepts_common_forms() {
        assert_eq!(
            parse_base("http://127.0.0.1:5177").unwrap(),
            ("127.0.0.1".to_string(), 5177)
        );
        assert_eq!(
            parse_base("http://127.0.0.1:5177/").unwrap(),
            ("127.0.0.1".to_string(), 5177),
            "尾部斜杠应被忽略"
        );
        assert_eq!(
            parse_base("127.0.0.1:5177").unwrap(),
            ("127.0.0.1".to_string(), 5177),
            "缺省 scheme 也应可用"
        );
        assert_eq!(parse_base("localhost:8080").unwrap(), ("localhost".to_string(), 8080));
    }

    #[test]
    fn parse_base_rejects_malformed_addresses() {
        assert!(parse_base("127.0.0.1").is_err(), "缺端口必须报错");
        assert!(parse_base("http://127.0.0.1:notaport").is_err());
        assert!(parse_base("http://127.0.0.1:99999").is_err(), "端口溢出 u16 必须报错");
    }

    // ---- decode_chunked ----

    #[test]
    fn decode_chunked_joins_chunks() {
        assert_eq!(decode_chunked("5\r\nHello\r\n6\r\n World\r\n0\r\n\r\n"), "Hello World");
    }

    /// HTTP/1.1 chunk size is **hexadecimal** (RFC 7230 §4.1): `d` = 13, `10` = 16.
    /// Parsing it as decimal silently breaks every chunk containing a-f and mis-sizes every chunk >= 16.
    #[test]
    fn decode_chunked_uses_hexadecimal_chunk_size() {
        assert_eq!(decode_chunked("d\r\nHello, World!\r\n0\r\n\r\n"), "Hello, World!");
        assert_eq!(
            decode_chunked("10\r\n0123456789abcdef\r\n0\r\n\r\n"),
            "0123456789abcdef",
            "`10` 是 16 字节，不是 10"
        );
        assert_eq!(decode_chunked("A\r\n0123456789\r\n0\r\n\r\n"), "0123456789", "大写十六进制同样合法");
    }

    #[test]
    fn decode_chunked_skips_chunk_extensions() {
        assert_eq!(decode_chunked("5;ext=1\r\nHello\r\n0\r\n\r\n"), "Hello");
    }

    #[test]
    fn decode_chunked_is_safe_on_empty_and_malformed() {
        assert_eq!(decode_chunked(""), "");
        assert_eq!(decode_chunked("garbage"), "", "无 CRLF 时不应 panic");
        assert_eq!(decode_chunked("5\r\nHi\r\n"), "", "声明长度超出实际内容时应安全中断");
    }

    // ---- handle: the JSON-RPC protocol layer (no network) ----

    #[test]
    fn handle_replies_parse_error_for_invalid_json() {
        let r = bridge().handle("not json").expect("解析错误也要回复");
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["error"]["code"].as_i64(), Some(-32700));
    }

    #[test]
    fn handle_replies_method_not_found() {
        let r = bridge()
            .handle(r#"{"jsonrpc":"2.0","id":7,"method":"nope"}"#)
            .unwrap();
        assert_eq!(r["id"].as_i64(), Some(7), "错误响应必须回带原 id");
        assert_eq!(r["error"]["code"].as_i64(), Some(-32601));
    }

    #[test]
    fn handle_sends_no_reply_for_notifications() {
        assert!(
            bridge()
                .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none(),
            "通知（无 id）不应回包"
        );
    }

    #[test]
    fn handle_initialize_and_ping() {
        let b = bridge();
        let init = b
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"].as_str(), Some("2024-11-05"));
        assert_eq!(init["result"]["serverInfo"]["name"].as_str(), Some("graphtell"));
        let ping = b.handle(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#).unwrap();
        assert_eq!(ping["result"], json!({}));
    }

    #[test]
    fn handle_tools_list_exposes_the_five_tools() {
        let r = bridge()
            .handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
            .unwrap();
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "recall_code",
                "compose_prompt",
                "check_compliance",
                "list_violations",
                "warmup_status"
            ]
        );
    }

    #[test]
    fn handle_tools_call_unknown_tool_is_error_without_network() {
        let r = bridge()
            .handle(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"no_such_tool"}}"#)
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool"));
    }

    // ---- recall quality metadata ----

    #[test]
    fn extract_quality_defaults_to_high_when_absent() {
        assert_eq!(extract_quality("{}"), ("high".to_string(), 1.0, Vec::new()));
        assert_eq!(extract_quality("not json"), ("high".to_string(), 1.0, Vec::new()));
        assert_eq!(
            extract_quality(r#"{"data":{}}"#),
            ("high".to_string(), 1.0, Vec::new())
        );
    }

    #[test]
    fn extract_quality_reads_tier_confidence_and_missing_terms() {
        let (q, c, m) =
            extract_quality(r#"{"data":{"quality":"low","confidence":0.42,"missing_terms":["库存","扣减"]}}"#);
        assert_eq!(q, "low");
        assert_eq!(c, 0.42);
        assert_eq!(m, vec!["库存".to_string(), "扣减".to_string()]);
    }

    #[test]
    fn extract_quality_ignores_non_string_terms() {
        let (_, _, m) = extract_quality(r#"{"data":{"missing_terms":["ok",1,null]}}"#);
        assert_eq!(m, vec!["ok".to_string()]);
    }

    #[test]
    fn extract_warmup_note_only_when_warming_and_not_warmed() {
        assert_eq!(extract_warmup_note("{}"), "");
        assert_eq!(
            extract_warmup_note(r#"{"data":{"warmup":{"warmed":true,"warming":true,"done":5,"total":10}}}"#),
            "",
            "已预热完成时不应再提示"
        );
        assert_eq!(
            extract_warmup_note(r#"{"data":{"warmup":{"warmed":false,"warming":false}}}"#),
            ""
        );
        let note = extract_warmup_note(r#"{"data":{"warmup":{"warmed":false,"warming":true,"done":3,"total":9}}}"#);
        assert!(note.contains("3/9"), "预热中应带上进度: {note}");
        assert!(note.contains("cold path"), "预热中应提示质量偏弱");
    }

    #[test]
    fn with_quality_guidance_only_hints_never_drops_markdown() {
        let md = "# ctx".to_string();
        assert_eq!(
            with_quality_guidance(md.clone(), "high", 1.0, &[]),
            md,
            "高质量应原样返回"
        );
        assert_eq!(
            with_quality_guidance(md.clone(), "weird", 0.1, &[]),
            md,
            "未知档位不应改动"
        );
        let low = with_quality_guidance(md.clone(), "low", 0.42, &["库存".to_string()]);
        assert!(low.starts_with("# ctx"), "低质量也绝不能丢弃已召回内容");
        assert!(low.contains("Recall quality low"));
        assert!(low.contains("0.42"));
        assert!(low.contains("`库存`"));
        let med = with_quality_guidance(md.clone(), "medium", 0.7, &[]);
        assert!(med.contains("Recall quality medium"));
        assert!(med.contains("no usable feature terms"), "缺失词为空时应给占位提示");
    }

    // ---- response extraction / formatting ----

    #[test]
    fn extract_markdown_and_prompt_surface_service_errors() {
        assert_eq!(
            extract_markdown(r##"{"ok":true,"data":{"markdown":"# MD"}}"##).unwrap(),
            "# MD"
        );
        assert!(extract_markdown(r#"{"ok":false,"error":"boom"}"#)
            .unwrap_err()
            .to_string()
            .contains("boom"));
        assert!(extract_markdown(r#"{"ok":true}"#).is_err(), "缺 data 应报错");
        assert!(extract_markdown(r#"{"ok":true,"data":{}}"#).is_err(), "缺 markdown 应报错");
        assert_eq!(extract_prompt(r#"{"ok":true,"data":{"prompt":"P"}}"#).unwrap(), "P");
        assert!(extract_prompt(r#"{"ok":false}"#)
            .unwrap_err()
            .to_string()
            .contains("unknown error"));
    }

    #[test]
    fn format_violation_fills_defaults_for_missing_fields() {
        assert_eq!(
            format_violation(&json!({"severity":"error","rule_id":"r1","file":"a.php","line":12,"message":"m"})),
            "[error] r1 a.php:12 — m\n"
        );
        assert_eq!(format_violation(&json!({})), "[?] ? -:0 — \n");
    }

    #[test]
    fn format_violations_reports_empty_as_no_violations() {
        let (out, err) = format_violations(
            r#"{"ok":true,"data":[{"severity":"warning","rule_id":"x","file":"f","line":1,"message":"m"}]}"#,
        );
        assert!(!err);
        assert!(out.contains("Violations (1 in total)"));
        assert!(out.contains("[warning] x f:1 — m"));
        let (out, err) = format_violations(r#"{"ok":true,"data":null}"#);
        assert!(!err, "无违规不是错误");
        assert_eq!(out, "No violations");
        assert!(format_violations(r#"{"ok":false,"error":"e"}"#).1, "服务失败应标记为错误");
    }

    #[test]
    fn format_warmup_covers_all_three_states() {
        let (out, err) =
            format_warmup(r#"{"ok":true,"data":{"warmed":true,"warming":false,"done":10,"total":10}}"#);
        assert!(!err);
        assert!(out.contains("warm-up finished"));
        let (out, _) =
            format_warmup(r#"{"ok":true,"data":{"warmed":false,"warming":true,"done":2,"total":8}}"#);
        assert!(out.contains("2/8"));
        let (out, _) = format_warmup(r#"{"ok":true,"data":{"warmed":false,"warming":false}}"#);
        assert!(out.contains("not warmed"));
        let (out, err) = format_warmup(r#"{"ok":true}"#);
        assert!(!err);
        assert_eq!(out, "No warm-up status");
        assert!(format_warmup(r#"{"ok":false}"#).1, "服务失败应标记为错误");
    }

    #[test]
    fn format_check_summarizes_severity_and_violations() {
        let (out, err) = format_check(
            r#"{"ok":true,"data":{"rules_run":5,"duration_ms":42,"by_severity":{"error":2,"warning":1},"violations":[{"severity":"error","rule_id":"r","file":"f.php","line":3,"message":"m"}]}}"#,
        );
        assert!(!err);
        assert!(out.contains("ran 5 rules in 42ms"));
        assert!(out.contains("error=2"));
        assert!(out.contains("warning=1"));
        assert!(out.contains("critical=0"), "未出现的级别应记 0");
        assert!(out.contains("[error] r f.php:3 — m"));
        assert!(format_check(r#"{"ok":true}"#).1, "缺 data 应标记为错误");
        assert!(format_check(r#"{"ok":false}"#).1);
    }
}
