//! P10 Cors：检测「CORS 反射源站」—— 响应头 `Access-Control-Allow-Origin`
//! 被直接设为请求的 `Origin`（或整段 `Access-Control-Allow-Origin` 头由变量拼接）。
//!
//! # 动机
//!
//! 反射源站（Reflective CORS）是教科书级的高危跨域配置：把任意站点的请求都当成同源、
//! 再配合 `Access-Control-Allow-Credentials: true` 即可泄露用户凭证（Cookie / 鉴权头）。
//! 它和 SQL 注入一样，是「所有 PHP 工程通用」的代码坏味，却长期没有规则能拦。
//!
//! # 覆盖的形态
//!
//! * 形态一（数组赋值）：`$header['Access-Control-Allow-Origin'] = app()->request->header('origin');`
//!   —— 解析期在 `collect_call_sites` 捕获该赋值（[`gt_domain::model::syntax::HeaderAssignFact`]），
//!   本阶段确认右侧读取了请求 Origin，并在同行定位到 `->header('origin')` 调用点作为标注落点。
//! * 形态二（`header()` 调用）：`header("Access-Control-Allow-Origin: " . $origin)` /
//!   `$response->header("Access-Control-Allow-Origin: $origin")` —— 全局 `header()` 函数
//!   或响应对象的 `header()` 方法，其首个实参字符串里同时含有 `Access-Control-Allow-Origin`
//!   与一个变量（$var）。标注直接落在该调用点。
//!
//! 形态二里「整段头由变量拼接」用 `text_has_var` 判定，因此 `header("Access-Control-Allow-Origin: *")`
//! （通配，无变量）不会被误报——那属于另一类、更轻的配置问题，不在本规则范围内。

use gt_domain::model::{
    AnnotationChannel, FactValue, Language, MergeStrategy, NewAnnotation, NodeId, Phase,
};
use serde_json::json;

use crate::context::PipelineContext;

/// 反射源站标注的 kind（规则用 `has_annotation: cors_origin_reflect` 命中）。
const CORS_REFLECT: &str = "cors_origin_reflect";

pub fn run(ctx: &mut PipelineContext) {
    let mut count = 0usize;

    // 先收集待标注的调用点，避免 `ctx.ws` 的不可变借用与 `annotate` 的可变借用冲突。
    let mut targets: Vec<(NodeId, String, u32, String)> = Vec::new();

    // 形态一：解析期捕获的「Allow-Origin 头赋值」，需右侧读取请求 Origin。
    for ha in ctx.ws.header_assignments.iter() {
        if ha.key != "access-control-allow-origin" {
            continue;
        }
        if !rhs_is_request_origin(&ha.rhs_snippet) {
            continue;
        }
        // 在同文件、[start_line, end_line] 区间内定位 `->header('origin')` 调用点作为标注落点。
        if let Some(node) = find_origin_read_node(ctx, &ha.file, ha.span.start_line, ha.span.end_line) {
            targets.push((node, ha.file.clone(), ha.span.start_line, ha.rhs_snippet.clone()));
        }
    }

    // 形态二：直接 `header("Access-Control-Allow-Origin: ...")` 且含变量。
    for call in ctx.ws.calls.iter() {
        if call.language.0 != Language::PHP {
            continue;
        }
        let Some(method) = call.method.as_deref() else {
            continue;
        };
        if method != "header" {
            continue;
        }
        let Some(arg) = call.args.first() else {
            continue;
        };
        if arg_is_reflective_cors_header(arg) {
            targets.push((call.node, call.file.clone(), call.span.start_line, arg_text(arg)));
        }
    }

    for (node, file, line, evidence) in targets {
        annotate(ctx, node, &file, line, &evidence);
        count += 1;
    }

    tracing::info!("P10 CORS 完成：反射源站 {} 处", count);
}

/// 右侧是否读取了请求 Origin（反射源站的数据源）。
fn rhs_is_request_origin(rhs: &str) -> bool {
    let lower = rhs.to_ascii_lowercase();
    // 读取请求 Origin 的常见写法：
    //   app()->request->header('origin') / request()->header('origin') / $request->header('origin')
    //   $_SERVER['HTTP_ORIGIN'] / getallheaders()['origin']
    let reads_origin = (lower.contains("header(") && lower.contains("origin"))
        || lower.contains("http_origin")
        || (lower.contains("getallheaders") && lower.contains("origin"));
    reads_origin && lower.contains("origin")
}

/// 在给定 (文件, 行区间) 内找到「读取请求 origin」的 `header('origin')` 调用点节点。
fn find_origin_read_node(
    ctx: &PipelineContext,
    file: &str,
    start: u32,
    end: u32,
) -> Option<NodeId> {
    ctx.ws.calls.iter().find(|c| {
        c.file == file
            && c.span.start_line >= start
            && c.span.start_line <= end
            && c.method.as_deref() == Some("header")
            && c.args
                .first()
                .and_then(|a| a.as_str())
                .map(|s| s.eq_ignore_ascii_case("origin"))
                .unwrap_or(false)
    }).map(|c| c.node)
}

/// 首个实参是否为「含变量的 Access-Control-Allow-Origin 头串」（反射型）。
fn arg_is_reflective_cors_header(fv: &FactValue) -> bool {
    let t = match fv {
        FactValue::String(s) => s.as_str(),
        FactValue::Unknown(Some(t)) => t.as_str(),
        _ => return false,
    };
    let lower = t.to_ascii_lowercase();
    lower.contains("access-control-allow-origin") && text_has_var(t)
}

fn annotate(ctx: &mut PipelineContext, node: NodeId, file: &str, line: u32, evidence: &str) {
    ctx.ws.annotate(NewAnnotation {
        node_id: node,
        channel: AnnotationChannel("Cors".to_string()),
        kind: CORS_REFLECT.to_string(),
        subkind: Some("CorsReflection".to_string()),
        confidence: 0.9,
        evidence: json!({
            "file": file,
            "line": line,
            "snippet": evidence,
        }),
        phase: Phase("Cors".to_string()),
        merge: MergeStrategy::Coexist,
    });
}

/// 参数文本里是否含有变量引用（`$var` / `{$var}`），用于识别「头由变量拼接」。
fn text_has_var(t: &str) -> bool {
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            match bytes.get(i + 1) {
                Some(b'{') => return true,
                Some(c) if c.is_ascii_alphabetic() || *c == b'_' => return true,
                _ => {}
            }
        }
        i += 1;
    }
    false
}

fn arg_text(fv: &FactValue) -> String {
    match fv {
        FactValue::String(s) => s.clone(),
        FactValue::Unknown(Some(t)) => t.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_request_origin_reads() {
        assert!(rhs_is_request_origin("app()->request->header('origin')"));
        assert!(rhs_is_request_origin("request()->header('origin')"));
        assert!(rhs_is_request_origin("$request->header('origin')"));
        assert!(rhs_is_request_origin("$_SERVER['HTTP_ORIGIN']"));
    }

    #[test]
    fn ignores_non_origin_rhs() {
        assert!(!rhs_is_request_origin("*"));
        assert!(!rhs_is_request_origin("'https://trusted.example.com'"));
        assert!(!rhs_is_request_origin("$header['Content-Type'] = 'application/json'"));
    }

    #[test]
    fn detects_reflective_cors_header_arg() {
        assert!(arg_is_reflective_cors_header(&FactValue::Unknown(Some(
            "Access-Control-Allow-Origin: \" . $origin".to_string()
        ))));
        assert!(arg_is_reflective_cors_header(&FactValue::String(
            "Access-Control-Allow-Origin: $origin".to_string()
        )));
    }

    #[test]
    fn ignores_wildcard_cors_header_arg() {
        assert!(!arg_is_reflective_cors_header(&FactValue::String(
            "Access-Control-Allow-Origin: *".to_string()
        )));
    }
}
