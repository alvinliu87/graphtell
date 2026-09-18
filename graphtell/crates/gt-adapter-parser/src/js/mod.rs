//! JS/TS 前端解析器（tree-sitter 全量调用图）。
//!
//! 与 PHP / Java 走同一套 **tree-sitter → [`SyntaxFacts`]** 管线：把函数 / 方法 / 类
//! 声明翻成 `Declaration`，把每个调用点翻成 `CallSiteFact`，从而让上层流水线
//! （P2 CfAst / P7 调用链解析）像对待后端一样，为前端建立起**第一类语义节点**
//! （函数 / 方法）+ 调用链（`Calls` 边 + `CallSite` 节点）。
//!
//! 这样前端在图里就与后端同构：
//! * `Component`(方法) →`Calls`→ `api/agent.js:agentGet`(函数) →`CallsHttp`→ `HttpContract`
//! * 折叠视图默认只显示语义节点（`HttpContract`），`agentGet` 等语法跳被收进边的 `via`
//!   链，点击边在 drawer 里逐跳看到调用处 —— 与后端 `Method → CallSite → HttpContract` 一致。
//!
//! 之所以选 `tree-sitter-typescript` 一个 crate：TS 语法是 JS 的超集，
//! `LANGUAGE_TYPESCRIPT` 直接解析 `.js/.ts`，`LANGUAGE_TSX` 解析 `.jsx/.tsx`；
//! `.vue` 则抽取 `<script>` 块后按 TS 解析（行号对齐）。
//!
//! HTTP 调用（`uni.request` / `request` / `http.request` / `axios*` / `fetch`）仍被识别，
//! 其 `args[0]` 规整为 `{ url, method }` 对象，喂给 FKB 的 `frontend-http-contract` 规则
//! 合成 `HttpContract` 并挂 `CallsHttp`（与后端 `HandledBy` 在同一 `ContractId` 节点汇聚）。

use std::cell::RefCell;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    CallSiteFact, Declaration, FactValue, ImportFact, Language, NodeKind, Span, SyntaxFacts,
};
use gt_domain::port::LanguageParser;
use serde_json::json;
use tree_sitter::{Language as TsLanguage, Node, Parser};
use tree_sitter_typescript::{LANGUAGE_TSX, LANGUAGE_TYPESCRIPT};

/// JS/TS 前端解析适配器（同一实现服务 javascript / typescript / vue / jsx / tsx）。
pub struct JsFrontendParser;

impl JsFrontendParser {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

impl LanguageParser for JsFrontendParser {
    fn language(&self) -> Language {
        Language::new(Language::JAVASCRIPT)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["js", "ts", "jsx", "tsx", "mjs", "cjs", "vue"]
    }

    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    fn member_separator(&self) -> &'static str {
        "."
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["package.json"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &[
            "node_modules",
            "dist",
            "build",
            ".output",
            "unpackage",
            "static",
            ".nuxt",
            ".next",
            ".svelte-kit",
        ]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let lower = path.to_ascii_lowercase();
        let owned_vue: String;
        // `.vue`：抽取 `<script>` 块并把行号对齐到原文件，再按 TS 解析。
        let (ts_lang, src): (TsLanguage, &str) = if lower.ends_with(".vue") {
            match extract_vue_script(source) {
                Some(s) => {
                    owned_vue = s;
                    (LANGUAGE_TYPESCRIPT.into(), &owned_vue)
                }
                None => return Ok(SyntaxFacts::default()),
            }
        } else if lower.ends_with(".tsx") || lower.ends_with(".jsx") {
            (LANGUAGE_TSX.into(), source)
        } else {
            (LANGUAGE_TYPESCRIPT.into(), source)
        };

        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                p.set_language(&ts_lang).expect("tree-sitter-typescript language");
                p
            });
            parser.parse(src, None)
        });
        let tree = tree.ok_or_else(|| DomainError::Parse {
            file: path.into(),
            reason: "tree-sitter 返回 None".into(),
        })?;

        let mut facts = SyntaxFacts::default();
        let mut ctx = Ctx {
            src,
            facts: &mut facts,
            path,
        };
        let root = tree.root_node();
        let file_owner = ctx.path;
        // 把 program 节点交给 walk，由它遍历顶层声明（class/function/import…）作为子节点匹配，
        // 与 PHP 的 walk_scope 同构；若直接遍历 root.named_children 再 walk(child)，
        // 则 class_declaration 自身成了 node、其分支永远匹配不到。
        walk(root, &mut ctx, file_owner, None);
        Ok(facts)
    }
}

/// 解析上下文。
struct Ctx<'a> {
    src: &'a str,
    facts: &'a mut SyntaxFacts,
    path: &'a str,
}

/// HTTP 调用的形态（决定 url / method 如何从句柄里解出）。
enum HttpStyle {
    /// `uni.request({ url, method })` / `request({...})` / `http.request({...})`。
    Obj,
    /// `axios.get(...)`（method 来自成员名）/ `axios({ url, method })`（对象式）。
    Axios(Option<String>),
    /// `fetch(url, { method })`。
    Fetch,
    /// 非 HTTP 调用（普通函数 / 组件方法调用）。
    Other,
}

impl HttpStyle {
    fn is_http(&self) -> bool {
        !matches!(self, HttpStyle::Other)
    }
}

/// 递归游走语法树，按节点种类产出声明 / 导入 / 调用点。
///
/// * `owner`：当前所处"调用归属"节点的 FQN。顶层语句归属文件（`ctx.path`），
///   进入函数 / 方法后改为该函数 / 方法的 FQN —— 这正是 `CallSiteFact.owner_fqn`，
///   让 P2 把 `CallSite` 节点挂到**函数**（而非 `File`）之下，从而 `CallsHttp` 也从
///   前端函数发出。
/// * `class`：当前所处类的 FQN（方法归属用）。
fn walk(node: Node, ctx: &mut Ctx, owner: &str, class: Option<&str>) {
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        match child.kind() {
            "import_statement" => collect_imports(child, ctx),
            // `export function/const/class ...`：拆掉 export 外壳继续走内部声明。
            "export_statement" => walk(child, ctx, owner, class),
            "class_declaration" => collect_class(child, ctx),
            "function_declaration" | "generator_function_declaration" => {
                collect_named_function(child, ctx);
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut d = child.walk();
                for dec in child.named_children(&mut d) {
                    if dec.kind() == "variable_declarator" {
                        collect_declarator(dec, ctx, owner, class);
                    } else {
                        walk(dec, ctx, owner, class);
                    }
                }
            }
            "call_expression" => {
                collect_invocation(child, ctx, owner, false);
                // 继续下钻，捕获实参里的嵌套调用（如 `axios.post(buildUrl())`）。
                walk(child, ctx, owner, class);
            }
            "new_expression" => {
                collect_invocation(child, ctx, owner, true);
                walk(child, ctx, owner, class);
            }
            _ => walk(child, ctx, owner, class),
        }
    }
}

/// 顶层/类内具名函数声明：`function foo() {}` / `export function foo() {}`。
fn collect_named_function(node: Node, ctx: &mut Ctx) {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src).to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return; // 匿名（如 `export default () => {}`）：不建节点，其体内调用归属外层。
    }
    let fqn = name.clone();
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::FUNCTION.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: None,
        span: span_of(node, ctx.src),
        extra: json!({}),
    });
    if let Some(body) = node.child_by_field_name("body") {
        walk(body, ctx, &fqn, None);
    }
}

/// `const foo = () => {}` / `const foo = function() {}` 这类赋值式函数。
fn collect_declarator(dec: Node, ctx: &mut Ctx, owner: &str, class: Option<&str>) {
    let name = dec
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src).to_string());
    let value = dec.child_by_field_name("value");
    match (name, value) {
        (Some(name), Some(v))
            if v.kind() == "arrow_function"
                || v.kind() == "function_expression"
                || v.kind() == "generator_function" =>
        {
            let fqn = name.clone();
            ctx.facts.declarations.push(Declaration {
                kind: NodeKind(NodeKind::FUNCTION.to_string()),
                name: name.clone(),
                fqn: fqn.clone(),
                parent_fqn: None,
                span: span_of(dec, ctx.src),
                extra: json!({}),
            });
            if let Some(body) = v.child_by_field_name("body") {
                walk(body, ctx, &fqn, class);
            }
        }
        (_, Some(v)) => walk(v, ctx, owner, class),
        _ => {}
    }
}

/// 类声明：建 `Class` 节点，并逐个处理成员方法 / 字段方法。
fn collect_class(node: Node, ctx: &mut Ctx) {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src).to_string())
        .unwrap_or_else(|| "<anonymous>".to_string());
    let cfqn = name.clone();
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::CLASS.to_string()),
        name: name.clone(),
        fqn: cfqn.clone(),
        parent_fqn: None,
        span: span_of(node, ctx.src),
        extra: json!({}),
    });

    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut c = body.walk();
    for member in body.named_children(&mut c) {
        match member.kind() {
            "method_definition" | "constructor" => {
                let mname = member
                    .child_by_field_name("name")
                    .map(|n| text(n, ctx.src).to_string())
                    .unwrap_or_else(|| member.kind().to_string());
                let mfqn = format!("{cfqn}.{mname}");
                ctx.facts.declarations.push(Declaration {
                    kind: NodeKind(NodeKind::METHOD.to_string()),
                    name: mname.clone(),
                    fqn: mfqn.clone(),
                    parent_fqn: Some(cfqn.clone()),
                    span: span_of(member, ctx.src),
                    extra: json!({}),
                });
                if let Some(b) = member.child_by_field_name("body") {
                    walk(b, ctx, &mfqn, Some(&cfqn));
                }
            }
            "public_field_definition" => {
                let fname = member
                    .child_by_field_name("name")
                    .map(|n| text(n, ctx.src).to_string())
                    .unwrap_or_default();
                let value = member.child_by_field_name("value");
                if let Some(v) = value {
                    if v.kind() == "arrow_function"
                        || v.kind() == "function_expression"
                        || v.kind() == "generator_function"
                    {
                        let mfqn = format!("{cfqn}.{fname}");
                        ctx.facts.declarations.push(Declaration {
                            kind: NodeKind(NodeKind::METHOD.to_string()),
                            name: fname.clone(),
                            fqn: mfqn.clone(),
                            parent_fqn: Some(cfqn.clone()),
                            span: span_of(member, ctx.src),
                            extra: json!({}),
                        });
                        if let Some(b) = v.child_by_field_name("body") {
                            walk(b, ctx, &mfqn, Some(&cfqn));
                        }
                        continue;
                    }
                }
                walk(member, ctx, ctx.path, Some(&cfqn));
            }
            _ => {}
        }
    }
}

/// 导入语句：`import axios from 'axios'` / `import { agentGet } from './api'`。
fn collect_imports(node: Node, ctx: &mut Ctx) {
    let Some(clause) = node.child_by_field_name("clause") else {
        return;
    };
    let mut c = clause.walk();
    for imp in clause.named_children(&mut c) {
        match imp.kind() {
            // 默认导入：`import axios from 'axios'`
            "identifier" => {
                ctx.facts.imports.push(ImportFact {
                    alias: None,
                    name: text(imp, ctx.src).to_string(),
                    span: span_of(imp, ctx.src),
                });
            }
            // 具名导入：`import { a, b as c } from '...'`
            "named_imports" => {
                let mut g = imp.walk();
                for spec in imp.named_children(&mut g) {
                    if spec.kind() == "import_specifier" {
                        let name = spec
                            .child_by_field_name("name")
                            .map(|n| text(n, ctx.src).to_string())
                            .unwrap_or_default();
                        let alias = spec
                            .child_by_field_name("alias")
                            .map(|a| text(a, ctx.src).to_string());
                        ctx.facts.imports.push(ImportFact {
                            alias,
                            name,
                            span: span_of(spec, ctx.src),
                        });
                    }
                }
            }
            // 命名空间导入：`import * as ns from '...'`
            "namespace_import" => {
                if let Some(n) = imp.named_child(0) {
                    ctx.facts.imports.push(ImportFact {
                        alias: None,
                        name: text(n, ctx.src).to_string(),
                        span: span_of(imp, ctx.src),
                    });
                }
            }
            _ => {}
        }
    }
}

/// 收集一次调用（或 `new`）调用点。
fn collect_invocation(call: Node, ctx: &mut Ctx, owner: &str, is_new: bool) {
    let func = call.child_by_field_name("function");
    let ctor = if is_new {
        call.child_by_field_name("constructor")
    } else {
        func
    };

    let (callee_text, receiver, method_field, style) = match ctor {
        Some(f) if f.kind() == "member_expression" => {
            let obj = f
                .child_by_field_name("object")
                .map(|o| text(o, ctx.src).to_string());
            let prop = f
                .child_by_field_name("property")
                .map(|p| text(p, ctx.src).to_string())
                .unwrap_or_default();
            match obj.as_deref() {
                Some("axios") => {
                    let verb = if is_http_verb(&prop) {
                        Some(prop.to_ascii_uppercase())
                    } else {
                        None
                    };
                    (
                        "axios".to_string(),
                        Some("axios".to_string()),
                        verb.clone(),
                        HttpStyle::Axios(verb.clone()),
                    )
                }
                Some(o) if format!("{o}.{prop}") == "uni.request"
                    || format!("{o}.{prop}") == "http.request" =>
                {
                    (
                        format!("{o}.{prop}"),
                        Some(o.to_string()),
                        Some(prop.clone()),
                        HttpStyle::Obj,
                    )
                }
                _ => (
                    format!("{}.{}", obj.as_deref().unwrap_or(""), prop),
                    obj.clone(),
                    Some(prop),
                    HttpStyle::Other,
                ),
            }
        }
        Some(f) => {
            let t = text(f, ctx.src).to_string();
            if t == "fetch" {
                ("fetch".to_string(), None, Some("fetch".to_string()), HttpStyle::Fetch)
            } else if t == "request" {
                ("request".to_string(), None, Some("request".to_string()), HttpStyle::Obj)
            } else if t == "axios" {
                // `axios({ url, method })` 对象式：method 来自实参，无成员名。
                ("axios".to_string(), Some("axios".to_string()), None, HttpStyle::Axios(None))
            } else if is_new {
                (format!("new {t}"), None, Some(t), HttpStyle::Other)
            } else {
                (t.clone(), None, Some(t), HttpStyle::Other)
            }
        }
        None => return,
    };

    let args_node = call.child_by_field_name("arguments");
    let args = if style.is_http() {
        let args_text = args_node.map(|a| text(a, ctx.src)).unwrap_or("");
        let (url, method) = extract_http_args(&style, args_text);
        let url_fv = match url {
            Some(u) => FactValue::String(u),
            // url 未知（变量 / 含插值模板串）给 `Unknown`，FKB 取不到 path 自然不合成，
            // 避免 `GET /<dynamic-url>` 这类垃圾契约污染路由视角。
            None => FactValue::Unknown(Some("<dynamic-url>".to_string())),
        };
        vec![FactValue::Array(vec![
            ("url".to_string(), url_fv),
            ("method".to_string(), FactValue::String(method)),
        ])]
    } else {
        // 非 HTTP 调用：本阶段不依赖实参（P7 按被调名解析），留空即可。
        vec![]
    };

    ctx.facts.call_sites.push(CallSiteFact {
        owner_fqn: owner.to_string(),
        owner_class: None,
        callee_text,
        snippet: line_snippet(ctx.src, call.start_byte()),
        receiver,
        method: method_field,
        args,
        span: span_of(call, ctx.src),
    });
}

/// 按 HTTP 形态从实参子串解出 (url, method)。
fn extract_http_args(style: &HttpStyle, args: &str) -> (Option<String>, String) {
    match style {
        HttpStyle::Obj => {
            let method = field_string(args, "method")
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            (field_string(args, "url"), method)
        }
        HttpStyle::Axios(Some(verb)) => {
            let url = first_string(args).or_else(|| field_string(args, "url"));
            (url, verb.clone())
        }
        HttpStyle::Axios(None) => {
            let method = field_string(args, "method")
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            (field_string(args, "url"), method)
        }
        HttpStyle::Fetch => {
            let url = first_string(args);
            let method = field_string(args, "method")
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            (url, method)
        }
        HttpStyle::Other => (None, "GET".to_string()),
    }
}

fn is_http_verb(s: &str) -> bool {
    matches!(s, "get" | "post" | "put" | "delete" | "patch" | "head" | "options")
}

// ---------------------------------------------------------------- 通用小工具

fn text<'a>(node: Node<'a>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// 字节下标 → (行号, 列号)。
fn line_col(src: &str, byte: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut col = 1u32;
    for (i, ch) in src.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn span_of(node: Node, src: &str) -> Span {
    let (sl, _) = line_col(src, node.start_byte());
    let (el, _) = line_col(src, node.end_byte());
    Span {
        start_line: sl,
        end_line: el,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
    }
}

/// 取调用点所在整行源码，供 UI 在链路的"调用处"直接显示。
fn line_snippet(src: &str, start: usize) -> Option<String> {
    let line_start = src[..start.min(src.len())].rfind('\n').map(|i| i + 1).unwrap_or(0);
    src[line_start..].lines().next().map(|l| l.trim().to_string())
}

/// 抽取 Vue 单文件组件的 `<script>` 块内容，并在前面补齐换行使行号与原文件对齐。
fn extract_vue_script(src: &str) -> Option<String> {
    let open = src.find("<script")?;
    let gt_rel = src[open..].find('>')?;
    let close_tag = open + gt_rel;
    let after = close_tag + 1;
    let end_rel = src[after..].find("</script>")?;
    let end = after + end_rel;
    let inner = &src[after..end];
    let lines_before = src[..open].matches('\n').count();
    let mut s = String::new();
    for _ in 0..lines_before {
        s.push('\n');
    }
    s.push_str(inner);
    Some(s)
}

/// 字符串字面量（含模板串）匹配。
fn str_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`"#).unwrap())
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// 去掉引号并做最小转义还原；模板串含插值则返回 `None`（无法静态求值）。
fn decode_string(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let quote = bytes[0] as char;
    if quote == '`' && raw.contains("${") {
        return None;
    }
    let inner = &raw[1..raw.len() - 1];
    let decoded = inner
        .replace("\\\"", "\"")
        .replace("\\'", "'")
        .replace("\\\\", "\\")
        .replace("\\n", "\n")
        .replace("\\t", "\t");
    if decoded.is_empty() {
        return None;
    }
    Some(decoded)
}

/// 在子串里找首个字符串字面量；模板串含插值 `${` 时视为无法静态求值。
fn first_string(args: &str) -> Option<String> {
    let m = str_re().find(args)?;
    decode_string(m.as_str())
}

/// 在子串里找 `key: <字符串字面量>`。
///
/// 两条硬约束（真实前端代码踩过的坑）：
/// * 值必须紧跟在 `key:` 之后 —— `url: url`（变量）时绝不越本键抓后续 `method: 'get'`；
/// * `key` 前不能是标识符字符 —— `base_url:` 不被当成 `url:`。
fn field_string(args: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:");
    let bytes = args.as_bytes();
    let mut from = 0;
    let idx = loop {
        let pos = args[from..].find(&needle)?;
        let abs = from + pos;
        let prev_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        if prev_ok {
            break abs;
        }
        from = abs + 1;
    };
    let rest = args[idx + needle.len()..].trim_start();
    let first = rest.as_bytes().first().copied()?;
    if first != b'"' && first != b'\'' && first != b'`' {
        return None;
    }
    first_string(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析一段源码，返回全部语法事实。
    fn parse_src(src: &str) -> SyntaxFacts {
        JsFrontendParser::new()
            .unwrap()
            .parse("sample.js", src)
            .expect("解析应成功")
    }

    /// 取首个 HTTP 调用点的 (callee_text, url, method)，便于断言。
    fn first_http(src: &str) -> (String, Option<String>, String) {
        let facts = parse_src(src);
        let c = facts
            .call_sites
            .iter()
            .find(|c| c.args.iter().any(|a| matches!(a, FactValue::Array(_))))
            .expect("应提取到一个 HTTP 调用");
        let url = c
            .args
            .first()
            .and_then(|a| match a {
                FactValue::Array(items) => items
                    .iter()
                    .find(|(k, _)| k == "url")
                    .and_then(|(_, v)| v.as_str().map(|s| s.to_string())),
                _ => None,
            });
        let method = c
            .args
            .first()
            .and_then(|a| match a {
                FactValue::Array(items) => items
                    .iter()
                    .find(|(k, _)| k == "method")
                    .and_then(|(_, v)| v.as_str().map(|s| s.to_string())),
                _ => None,
            })
            .unwrap_or_default();
        (c.callee_text.clone(), url, method)
    }

    #[test]
    fn uni_request_object() {
        let (callee, url, method) = first_http("uni.request({ url: '/api/delete', method: 'POST' })");
        assert_eq!(callee, "uni.request");
        assert_eq!(url.as_deref(), Some("/api/delete"));
        assert_eq!(method, "POST");
    }

    #[test]
    fn axios_member_verb() {
        let (callee, url, method) = first_http("axios.post('/api/list')");
        assert_eq!(callee, "axios");
        assert_eq!(url.as_deref(), Some("/api/list"));
        assert_eq!(method, "POST");
    }

    #[test]
    fn fetch_with_options() {
        let (callee, url, method) = first_http("fetch('/api/x', { method: 'PUT' })");
        assert_eq!(callee, "fetch");
        assert_eq!(url.as_deref(), Some("/api/x"));
        assert_eq!(method, "PUT");
    }

    #[test]
    fn bare_request_defaults_get() {
        let (callee, url, method) = first_http("request({ url: '/y' })");
        assert_eq!(callee, "request");
        assert_eq!(url.as_deref(), Some("/y"));
        assert_eq!(method, "GET");
    }

    #[test]
    fn axios_object_form() {
        let (callee, url, method) =
            first_http("axios({ url: '/obj', method: 'delete' })");
        assert_eq!(callee, "axios");
        assert_eq!(url.as_deref(), Some("/obj"));
        assert_eq!(method, "DELETE");
    }

    #[test]
    fn non_http_calls_ignored() {
        // 普通函数调用不应被当成 HTTP 调用（其 args 应为空，不携带 url/method）。
        let facts = parse_src("doWork(a, b);\nif (x) { foo() }");
        assert!(facts
            .call_sites
            .iter()
            .all(|c| c.args.is_empty()), "非 HTTP 调用不应携带 url/method 实参");
        assert!(!facts.call_sites.is_empty(), "应仍收集到普通调用点");
    }

    #[test]
    fn variable_url_is_unknown() {
        // 含插值的模板串无法静态求值，url 必须是 Unknown（不合成幽灵契约）。
        let facts = parse_src("axios.get(`/api/${id}`)");
        let c = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "axios")
            .expect("应收集到 axios 调用");
        let url_unknown = matches!(
            c.args.first(),
            Some(FactValue::Array(items)) if items
                .iter()
                .any(|(k, v)| k == "url" && matches!(v, FactValue::Unknown(_)))
        );
        assert!(url_unknown, "模板串 url 必须是 Unknown");
    }

    #[test]
    fn variable_url_const_ref() {
        let (_, url, method) = first_http(
            "const IndexUrl = '/home/index';\nrequest({ url: IndexUrl, method: 'get' })",
        );
        assert!(url.is_none(), "常量引用 url 不能被解析成字面量");
        assert_eq!(method, "GET");
    }

    #[test]
    fn url_key_not_confused_with_base_url() {
        let (_, url, _) = first_http("request({ base_url: '/x', url: '/y' })");
        assert_eq!(url.as_deref(), Some("/y"));
    }

    #[test]
    fn multiline_object_url_still_resolved() {
        let (_, url, method) = first_http(
            "request({\n    url: 'agent/statistics',\n    method: 'get',\n    params,\n  });",
        );
        assert_eq!(url.as_deref(), Some("agent/statistics"));
        assert_eq!(method, "GET");
    }

    #[test]
    fn function_declaration_is_semantic_node() {
        // `export function agentGet() { request({ url }) }` 应建出 FUNCTION 节点，
        // 且 HTTP 调用点的 owner_fqn 指向该函数（而非文件）。
        let src = "export function agentGet() {\n  return request({ url: '/api/get', method: 'get' });\n}";
        let facts = parse_src(src);
        let fn_decl = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::FUNCTION && d.name == "agentGet");
        assert!(fn_decl.is_some(), "应建出 agentGet 函数节点");
        let http = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "request")
            .expect("应收集到 request 调用");
        assert_eq!(
            http.owner_fqn, "agentGet",
            "HTTP 调用的 owner 应是函数 agentGet，而非文件"
        );
    }

    #[test]
    fn component_calls_api_function() {
        // 组件方法调用 api 函数：应收集到 `loadList` 函数节点 + 一次 `agentGet` 调用，
        // 其 owner 是 `loadList`（跨文件调用链的"发起方"语义节点）。
        let src = "export function loadList() {\n  return agentGet();\n}";
        let facts = parse_src(src);
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.kind.as_str() == NodeKind::FUNCTION && d.name == "loadList"),
            "应建出 loadList 函数节点"
        );
        let call = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "agentGet")
            .expect("应收集到对 agentGet 的调用");
        assert_eq!(call.owner_fqn, "loadList");
    }

    #[test]
    fn class_method_owner_is_method_fqn() {
        let src = "class Api { getX() { return request({ url: '/x' }); } }";
        let facts = parse_src(src);
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.kind.as_str() == NodeKind::CLASS && d.name == "Api"),
            "应建出 Api 类节点"
        );
        let m = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::METHOD && d.name == "getX");
        assert!(m.is_some(), "应建出 getX 方法节点");
        let call = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "request")
            .expect("应收集到 request 调用");
        assert_eq!(call.owner_fqn, "Api.getX", "方法内调用的 owner 应是 Api.getX");
    }

    #[test]
    fn vue_script_is_parsed_with_aligned_lines() {
        // `.vue`：抽取 <script> 块，函数声明落在原文件行号（第 4 行），而非脚本内第 1 行。
        let src = "<template>\n  <div></div>\n</template>\n<script>\nfunction foo() {}\n</script>\n";
        let facts = JsFrontendParser::new()
            .unwrap()
            .parse("Comp.vue", src)
            .expect("解析应成功");
        let foo = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::FUNCTION && d.name == "foo")
            .expect("应解析到 vue 内的 foo 函数");
        assert_eq!(foo.span.start_line, 5, "vue 内函数行号应对齐到原文件（<script> 在第 4 行，函数在其后第 5 行）");
    }
}
