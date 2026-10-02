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

use gt_domain::model::{AnnotationChannel, Language, MergeStrategy, NewAnnotation, NodeId, Phase};
use serde_json::json;

use crate::context::PipelineContext;

/// The "external call inside a loop" annotation (rules match it via `has_annotation: ext-call-in-loop`).
const EXT_IN_LOOP: &str = "ext-call-in-loop";

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("External".to_string());
    let mut count = 0usize;
    let mut targets: Vec<(NodeId, String, u32, String)> = Vec::new();

    for call in ctx.ws.calls.iter() {
        if call.language.0 != Language::PHP || !call.in_loop {
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
    use gt_domain::model::ProjectStatus;

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
}
