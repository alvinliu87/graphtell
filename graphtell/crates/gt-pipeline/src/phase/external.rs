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
            "the 3 cross-language external calls inside the loop must be captured"
        );
        assert!(ctx.ws.has_annotation(NodeId::new(1), EXT_IN_LOOP));
        assert!(ctx.ws.has_annotation(NodeId::new(2), EXT_IN_LOOP));
        assert!(ctx.ws.has_annotation(NodeId::new(3), EXT_IN_LOOP));
        // The two negative cases must NOT be annotated.
        assert!(!ctx.ws.has_annotation(NodeId::new(4), EXT_IN_LOOP));
        assert!(!ctx.ws.has_annotation(NodeId::new(5), EXT_IN_LOOP));
    }

    // ---- `is_external_call` branch tests ----
    // The integration test above never reaches the `method`-only path or the case-insensitive match,
    // yet both are the documented headline behaviour (full callee *or* method name, ascii-case-insensitively).

    /// A call counts as external when the *method* name matches FKB even though the full callee does not
    /// (e.g. `Foo::curl_exec` with method `curl_exec`). This is the path the cross-language test skips.
    #[test]
    fn method_name_matches_when_callee_differs() {
        let ctx = ctx_with(vec!["curl_exec", "send"]);
        // method matches a FKB entry, even though the full callee does not.
        assert!(is_external_call(&ctx, "Foo::curl_exec", Some("curl_exec")));
        assert!(is_external_call(&ctx, "Mailer::send", Some("send")));
        // callee matches a FKB entry fully, independent of method.
        assert!(is_external_call(&ctx, "curl_exec", Some("whatever")));
    }

    /// Matching is ascii-case-insensitive for both the full callee and the method name. Someone turning
    /// `eq_ignore_ascii_case` into `==` would silently stop catching these — this pins it.
    #[test]
    fn matching_is_case_insensitive() {
        let ctx = ctx_with(vec!["curl_exec"]);
        assert!(is_external_call(&ctx, "CURL_EXEC", None));
        assert!(is_external_call(&ctx, "Curl_Exec", Some("Curl_Exec")));

        let ctx = ctx_with(vec!["Http::get"]);
        assert!(is_external_call(&ctx, "http::GET", None));
        assert!(is_external_call(&ctx, "HTTP::get", Some("GET")));
    }

    /// When neither the callee nor the method matches — including a `None` method — the call is not external.
    #[test]
    fn no_match_when_method_absent_or_unrelated() {
        let ctx = ctx_with(vec!["curl_exec"]);
        assert!(!is_external_call(&ctx, "Foo::x", None));
        assert!(!is_external_call(&ctx, "Foo::x", Some("y")));
    }

    /// Build a `CallRecord` with explicit file/line so the produced annotation's evidence can be checked.
    fn mk_call(
        id: i64,
        callee: &str,
        method: Option<&str>,
        in_loop: bool,
        file: &str,
        line: u32,
    ) -> CallRecord {
        CallRecord {
            node: NodeId::new(id),
            owner: NodeId::new(0),
            owner_fqn: "C".into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: method.map(|m| m.to_string()),
            args: vec![],
            db_table: None,
            in_loop,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: 0, end_byte: 0 },
            file: file.into(),
            sub: None,
            language: Language::new("php"),
        }
    }

    /// The produced annotation must carry the exact channel / kind / subkind / confidence and, crucially,
    /// an `evidence` map that preserves the originating `file`, `line`, and `callee`. The cross-language
    /// integration test only checks `has_annotation` / `annotation_count`, so a bug that dropped or swapped
    /// any of these fields would slip past it.
    #[test]
    fn annotation_records_file_line_callee_and_metadata() {
        let mut ctx = ctx_with(vec!["curl_exec"]);
        ctx.ws.calls.push(mk_call(1, "curl_exec", Some("curl_exec"), true, "app/loop.php", 42));
        run(&mut ctx);

        let anns = ctx.ws.annotations_of(NodeId::new(1));
        assert_eq!(anns.len(), 1, "one in-loop external call => one annotation");
        let a = anns[0];
        assert_eq!(a.channel.0, "External");
        assert_eq!(a.kind, "ext-call-in-loop");
        assert_eq!(a.subkind.as_deref(), Some("NetworkInLoop"));
        assert!((a.confidence - 0.85).abs() < f32::EPSILON);
        assert_eq!(a.evidence.get("file").and_then(|v| v.as_str()), Some("app/loop.php"));
        assert_eq!(a.evidence.get("line").and_then(|v| v.as_i64()), Some(42));
        assert_eq!(a.evidence.get("callee").and_then(|v| v.as_str()), Some("curl_exec"));
    }

    /// An empty `external_calls` list must short-circuit `run` to produce zero annotations even when an
    /// in-loop call is present — pins the "FKB list empty => nothing" contract end-to-end (the empty-list
    /// branch was only unit-tested on `is_external_call` before).
    #[test]
    fn empty_fkb_produces_no_annotations() {
        let mut ctx = ctx_with(vec![]);
        ctx.ws.calls.push(mk_call(1, "curl_exec", Some("curl_exec"), true, "src", 1));
        run(&mut ctx);
        assert_eq!(ctx.ws.annotation_count(), 0);
        assert!(!ctx.ws.has_annotation(NodeId::new(1), EXT_IN_LOOP));
    }

    /// `run` annotates with `MergeStrategy::Coexist`, so two in-loop external calls from the *same* node
    /// both survive (no per-(node,kind) dedupe). A regression that switched the merge strategy to `Replace`
    /// would silently collapse them to one — this pins the Coexist contract at the `run` layer.
    #[test]
    fn coexist_merge_keeps_multiple_calls_from_same_node() {
        let mut ctx = ctx_with(vec!["curl_exec", "fetch"]);
        // Two distinct in-loop external calls, same owning node.
        ctx.ws.calls.push(mk_call(1, "curl_exec", Some("curl_exec"), true, "a.php", 10));
        ctx.ws.calls.push(mk_call(1, "fetch", Some("fetch"), true, "b.php", 20));
        run(&mut ctx);
        assert_eq!(ctx.ws.annotations_of(NodeId::new(1)).len(), 2);
        // And the evidence of each is preserved independently.
        let mut callees: Vec<&str> = ctx
            .ws
            .annotations_of(NodeId::new(1))
            .iter()
            .map(|a| a.evidence.get("callee").and_then(|v| v.as_str()).unwrap())
            .collect();
        callees.sort_unstable();
        assert_eq!(callees, vec!["curl_exec", "fetch"]);
    }

    /// The annotation's `phase` says which phase produced it, and nothing else here pins it (the
    /// metadata test above stops at channel / kind / subkind / confidence / evidence). A wrong or
    /// renamed phase would let every consumer that filters by phase miss these annotations.
    #[test]
    fn annotation_is_tagged_with_the_external_phase() {
        let mut ctx = ctx_with(vec!["curl_exec"]);
        ctx.ws
            .calls
            .push(mk_call(1, "curl_exec", Some("curl_exec"), true, "app/loop.php", 7));
        run(&mut ctx);
        let a = ctx.ws.annotations_of(NodeId::new(1))[0];
        assert_eq!(a.phase.0, "External");
    }

    /// `evidence.line` is the **start** line of the call site: the span may cover a multi-line call, and
    /// a human reading the report wants where the call begins.
    #[test]
    fn evidence_line_is_the_call_start_line() {
        let mut ctx = ctx_with(vec!["fetch"]);
        let mut call = mk_call(1, "fetch", Some("fetch"), true, "app/loop.ts", 10);
        call.span = Span {
            start_line: 10,
            end_line: 25,
            start_byte: 0,
            end_byte: 0,
        };
        ctx.ws.calls.push(call);
        run(&mut ctx);
        let a = ctx.ws.annotations_of(NodeId::new(1))[0];
        assert_eq!(a.evidence.get("line").and_then(|v| v.as_i64()), Some(10));
    }

    /// No call records at all: nothing to iterate, no annotation, no panic (the workspace may legitimately
    /// have zero calls — a config-only project, or everything filtered out before P12).
    #[test]
    fn run_without_call_records_produces_nothing() {
        let mut ctx = ctx_with(vec!["curl_exec"]);
        run(&mut ctx);
        assert_eq!(ctx.ws.annotation_count(), 0);
    }

    /// Matching is **whole-name equality** (case aside) — never prefix, suffix or substring. Loosening it
    /// would turn `curl` into "every curl_* helper" and `Http::get` into `Http::getToken`, flooding the
    /// N+1-style report with false positives.
    #[test]
    fn matching_is_exact_not_prefix_or_substring() {
        let ctx = ctx_with(vec!["curl", "Http::get", "send"]);
        assert!(!is_external_call(&ctx, "curl_exec", Some("curl_exec")), "a longer callee must not match a shorter entry");
        assert!(!is_external_call(&ctx, "Http::getStatus", Some("getStatus")), "nor a callee that merely starts with the entry");
        assert!(!is_external_call(&ctx, "MyCurlExecutor", Some("execute")), "no substring matching either");
        // No trimming: comparison is verbatim, so a padded FKB entry / padded callee simply never matches.
        assert!(!is_external_call(&ctx, " curl ", None));
        assert!(!is_external_call(&ctx, " Http::get", None));
    }

    /// A blank entry in `external_calls` is not a wildcard. Known wart, pinned: `is_empty()` only checks
    /// the *list*, so a malformed FKB entry survives the short-circuit and matches blank names — which
    /// never reach here from a parser, hence harmless but worth knowing.
    #[test]
    fn blank_entry_in_the_list_is_not_a_wildcard() {
        let ctx = ctx_with(vec![""]);
        assert!(!is_external_call(&ctx, "curl_exec", Some("curl_exec")));
        assert!(!is_external_call(&ctx, "Foo::x", None));
        assert!(is_external_call(&ctx, "", Some("")), "a blank pattern matches a blank name");
    }

    /// `MergeStrategy::Coexist` appends unconditionally — even two byte-identical annotations from the
    /// same node survive. Pinned because the alternative (dedupe by node+kind) would silently under-report
    /// a loop that calls the external API twice.
    #[test]
    fn coexist_keeps_even_identical_annotations() {
        let mut ctx = ctx_with(vec!["curl_exec"]);
        ctx.ws
            .calls
            .push(mk_call(1, "curl_exec", Some("curl_exec"), true, "a.php", 10));
        ctx.ws
            .calls
            .push(mk_call(1, "curl_exec", Some("curl_exec"), true, "a.php", 10));
        run(&mut ctx);
        assert_eq!(
            ctx.ws.annotations_of(NodeId::new(1)).len(),
            2,
            "two recorded loop iterations => two annotations"
        );
    }
}
