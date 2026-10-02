//! P10 Cors: detect "CORS reflected origin" — the response header `Access-Control-Allow-Origin` is set straight
//! from the request's `Origin` (or the whole `Access-Control-Allow-Origin` header is concatenated from a variable).
//!
//! # Motivation
//!
//! A reflective CORS origin is a textbook high-risk cross-origin configuration: treating any site's request as
//! same-origin and combining it with `Access-Control-Allow-Credentials: true` leaks user credentials (cookies /
//! auth headers). Like SQL injection it is a code smell "common to every PHP project", yet for a long time no rule
//! could catch it.
//!
//! # Forms covered
//!
//! * Form one (array assignment): `$header['Access-Control-Allow-Origin'] = app()->request->header('origin');`
//!   — the assignment is captured at parse time in `collect_call_sites`
//!   ([`gt_domain::model::syntax::HeaderAssignFact`]); this phase confirms that the right-hand side reads the
//!   request Origin and locates the `->header('origin')` call site on the same line as the annotation landing point.
//! * Form two (a `header()` call): `header("Access-Control-Allow-Origin: " . $origin)` /
//!   `$response->header("Access-Control-Allow-Origin: $origin")` — the global `header()` function or the response
//!   object's `header()` method, whose first argument string contains both `Access-Control-Allow-Origin` and a
//!   variable ($var). The annotation lands directly on that call site.
//!
//! In form two, "the whole header is concatenated from a variable" is judged by `text_has_var`, so
//! `header("Access-Control-Allow-Origin: *")` (a wildcard, no variable) is not a false positive — that is a
//! different, lighter configuration issue and outside this rule's scope.

use gt_domain::model::{
    AnnotationChannel, FactValue, Language, MergeStrategy, NewAnnotation, NodeId, Phase,
};
use serde_json::json;

use crate::context::PipelineContext;

/// The kind of the reflected-origin annotation (rules match it via `has_annotation: cors_origin_reflect`).
const CORS_REFLECT: &str = "cors_origin_reflect";

pub fn run(ctx: &mut PipelineContext) {
    let mut count = 0usize;

    // Collect the call sites to annotate first, to avoid a conflict between the immutable borrow of `ctx.ws` and the mutable borrow in `annotate`.
    let mut targets: Vec<(NodeId, String, u32, String)> = Vec::new();

    // Form one: the "Allow-Origin header assignment" captured at parse time; the right-hand side must read the request Origin.
    for ha in ctx.ws.header_assignments.iter() {
        if ha.key != "access-control-allow-origin" {
            continue;
        }
        if !rhs_is_request_origin(&ha.rhs_snippet) {
            continue;
        }
        // Locate the `->header('origin')` call site within the same file and [start_line, end_line] range as the annotation landing point.
        if let Some(node) = find_origin_read_node(ctx, &ha.file, ha.span.start_line, ha.span.end_line) {
            targets.push((node, ha.file.clone(), ha.span.start_line, ha.rhs_snippet.clone()));
        }
    }

    // Form two: a direct `header("Access-Control-Allow-Origin: ...")` containing a variable.
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

    tracing::info!("P10 CORS done: {} reflected origins", count);
}

/// Whether the right-hand side reads the request Origin (the data source of a reflected origin).
fn rhs_is_request_origin(rhs: &str) -> bool {
    let lower = rhs.to_ascii_lowercase();
    // Common ways of reading the request Origin:
    //   app()->request->header('origin') / request()->header('origin') / $request->header('origin')
    //   $_SERVER['HTTP_ORIGIN'] / getallheaders()['origin']
    let reads_origin = (lower.contains("header(") && lower.contains("origin"))
        || lower.contains("http_origin")
        || (lower.contains("getallheaders") && lower.contains("origin"));
    reads_origin && lower.contains("origin")
}

/// Find, within a given (file, line range), the `header('origin')` call-site node that "reads the request origin".
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

/// Whether the first argument is an "Access-Control-Allow-Origin header string containing a variable" (reflective).
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

/// Whether the argument text contains a variable reference (`$var` / `{$var}`), used to recognise "the header is concatenated from a variable".
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
