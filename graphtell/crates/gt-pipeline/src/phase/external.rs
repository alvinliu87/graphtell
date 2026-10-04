//! P12 External: **external system calls** (HTTP / SMS / email / RPC) inside a loop.
//!
//! # Motivation
//!
//! One network round trip costs an order of magnitude more than one database query. Putting it inside a loop
//! multiplies an endpoint's latency from "one remote call" into "N", and does so **serially** — it drags an
//! endpoint down even more reliably than N+1. Typical scenes: sending SMS to users in a loop, querying shipping
//! tracking in a loop, fetching remote images in a loop.
//!
//! The predicate is exactly isomorphic to N+1: a call site with `in_loop` whose callee hits FKB's
//! `external_calls`. The only difference is where the list comes from — the verb list lives in FKB, and the kernel
//! knows none of the names.
//!
//! # Why "is there a retry / timeout" is not attempted
//!
//! That would need the call arguments and SDK configuration, which the graph does not have; this only reports the
//! confirmed fact "a remote call happens inside a loop", and the fix is left to a human (batch API / merged
//! request / push it onto a queue).

use gt_domain::model::{AnnotationChannel, MergeStrategy, NewAnnotation, NodeId, Phase};
use serde_json::json;

use crate::context::PipelineContext;

/// The "external call inside a loop" annotation (rules match it via `has_annotation: ext-call-in-loop`).
const EXT_IN_LOOP: &str = "ext-call-in-loop";

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("External".to_string());
    let mut count = 0usize;
    let mut targets: Vec<(NodeId, String, u32, String)> = Vec::new();

    for call in ctx.ws.calls.iter() {
        // Language-agnostic: which callees count as external system calls comes entirely from FKB's
        // `external_calls` (merged globally, cross-framework small), so the kernel must not gate on a
        // specific language — otherwise a Java/JS/Python project that declares its external calls in FKB
        // would never be checked. The FKB list being empty already short-circuits via `is_external_call`.
        if !call.in_loop {
            continue;
        }
        if !is_external_call(ctx, &call.callee, call.method.as_deref()) {
            continue;
        }
        targets.push((call.node, call.file.clone(), call.span.start_line, call.callee.clone()));
    }

    for (node, file, line, callee) in targets {
        ctx.ws.annotate(NewAnnotation {
            node_id: node,
            channel: AnnotationChannel("External".to_string()),
            kind: EXT_IN_LOOP.to_string(),
            subkind: Some("NetworkInLoop".to_string()),
            confidence: 0.85,
            evidence: json!({ "file": file, "line": line, "callee": callee }),
            phase: phase.clone(),
            merge: MergeStrategy::Coexist,
        });
        count += 1;
    }

    tracing::info!("P12 external calls done: {} remote calls inside loops", count);
}

/// Whether a callee is an external system call: the full callee (`Http::get`) or the method name (`curl_exec`)
/// matches one of the `external_calls` declared by FKB.
fn is_external_call(ctx: &PipelineContext, callee: &str, method: Option<&str>) -> bool {
    if ctx.external_calls.is_empty() {
        return false;
    }
    if ctx
        .external_calls
        .iter()
        .any(|p| callee.eq_ignore_ascii_case(p))
    {
        return true;
    }
    method
        .map(|m| ctx.external_calls.iter().any(|p| m.eq_ignore_ascii_case(p)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PipelineContext;
    use crate::workspace::CallRecord;
    use gt_domain::model::{Language, NodeId, ProjectStatus, Span};

    fn ctx_with(calls: Vec<&str>) -> PipelineContext {
        let mut ctx = PipelineContext::new(gt_domain::model::Project {
            id: gt_domain::model::ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.external_calls = calls.into_iter().map(|s| s.to_string()).collect();
        ctx
    }

    #[test]
    fn matches_scoped_and_plain_callees() {
        let ctx = ctx_with(vec!["curl_exec", "Http::get"]);
        assert!(is_external_call(&ctx, "curl_exec", Some("curl_exec")));
        assert!(is_external_call(&ctx, "Http::get", Some("get")));
        assert!(!is_external_call(&ctx, "Db::name", Some("name")));
    }

    #[test]
    fn empty_fkb_list_matches_nothing() {
        let ctx = ctx_with(vec![]);
        assert!(!is_external_call(&ctx, "curl_exec", Some("curl_exec")));
    }

    /// The PHP-only gate was removed: in-loop external calls must be caught for **any** language whose
    /// FKB declares `external_calls`, not just PHP. This locks that behaviour so the restriction can't
    /// silently creep back.
    #[test]
    fn detects_in_loop_external_calls_across_languages() {
        let mut ctx = ctx_with(vec!["curl_exec", "fetch"]);
        let mk = |id: i64, lang: &str, callee: &str, method: &str, in_loop: bool| CallRecord {
            node: NodeId::new(id),
            owner: NodeId::new(0),
            owner_fqn: "C".into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: Some(method.into()),
            args: vec![],
            db_table: None,
            in_loop,
            entity: None,
            span: Span::default(),
            file: "src".into(),
            sub: None,
            language: Language::new(lang),
        };
        // In-loop external calls across three stacks — all must be flagged (no language gate).
        ctx.ws.calls.push(mk(1, "php", "curl_exec", "curl_exec", true));
        ctx.ws.calls.push(mk(2, "typescript", "fetch", "fetch", true));
        ctx.ws.calls.push(mk(3, "python", "fetch", "fetch", true));
        // Negative cases: not in a loop, and in-loop but not an external call.
        ctx.ws.calls.push(mk(4, "python", "fetch", "fetch", false));
        ctx.ws.calls.push(mk(5, "php", "Db::name", "name", true));

        run(&mut ctx);

        assert_eq!(
            ctx.ws.annotation_count(),
            3,
            "3 个跨语言、循环内的外部调用应被捕获"
        );
        assert!(ctx.ws.has_annotation(NodeId::new(1), EXT_IN_LOOP));
        assert!(ctx.ws.has_annotation(NodeId::new(2), EXT_IN_LOOP));
        assert!(ctx.ws.has_annotation(NodeId::new(3), EXT_IN_LOOP));
        // The two negative cases must NOT be annotated.
        assert!(!ctx.ws.has_annotation(NodeId::new(4), EXT_IN_LOOP));
        assert!(!ctx.ws.has_annotation(NodeId::new(5), EXT_IN_LOOP));
    }
}
