//! P13 Tx: **writing across several tables inside one method without a recognised transaction boundary**.
//!
//! # Motivation
//!
//! A method that writes the order and also writes stock (or writes the main table and also a journal) leaves
//! **partially successful** dirty data behind if any step in between fails. This is the most common source of
//! consistency incidents in e-commerce / admin systems, and code review rarely catches it — the writes are usually
//! spread across several called services.
//!
//! # The predicate (deliberately conservative)
//!
//! * One method has direct `WritesDb` edges to **>= 2 different tables** ("direct" = an edge laid down by P7's verb
//!   classification, excluding the indirect edges propagated along the call chain by P8 — indirect edges would
//!   count "called two services" as well, which belongs to the caller's transaction boundary and makes the
//!   predicate lose control);
//! * and no transaction marker declared by `tx_calls` (`transaction` / `startTrans` / `commit` …) appears anywhere
//!   in that method.
//!
//! Why "table count" rather than "write-verb call-site count": the latter counts one table written once in each of
//! two `if/else` branches (`CartLogic::add`'s `update` / `insert`) as two writes — those are **mutually exclusive
//! branches**, where partial success is not a thing. Measured: after switching the threshold from ">= 2 write
//! verbs" to ">= 2 tables", one project's hit count dropped from 105 to a far more credible level and the false
//! positives on mutually exclusive branches disappeared naturally.
//!
//! The conservative direction is **better a false negative**: the transaction may be opened by an outer caller
//! (across procedures), which the graph cannot see, so the copy says "no transaction boundary recognised" rather
//! than "no transaction".
//!
//! The annotation lands on the **method node**: a transaction is a method-level boundary concern, not a concern of
//! one particular line.

use std::collections::{HashMap, HashSet};

use gt_domain::model::{
    AnnotationChannel, EdgeKind, MergeStrategy, NewAnnotation, NodeId, Phase, SubProjectId,
};
use serde_json::json;

use crate::context::PipelineContext;

/// The transaction markers to judge a method with: the sub-project's own declaration, else the fallback.
///
/// `None` / empty means this stack declares nothing, so P13 cannot tell a transaction from an ordinary call
/// and judges nothing — rather than reporting every multi-table write in a stack it knows nothing about.
fn markers_for(ctx: &PipelineContext, sub: Option<SubProjectId>) -> Option<&Vec<String>> {
    sub.and_then(|s| ctx.tx_calls.get(&s.get()))
        .or(Some(&ctx.tx_calls_default))
        .filter(|m| !m.is_empty())
}

/// The "multi-table write without a transaction" annotation (rules match it via `has_annotation: multi-write-without-tx`).
const MULTI_WRITE: &str = "multi-write-without-tx";

/// Trigger threshold: the number of **distinct tables** one method writes directly.
const MIN_TABLES: usize = 2;

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Tx".to_string());
    let mut count = 0usize;

    // (1) Which tables each method writes directly (only the direct edges P7 laid down).
    let mut tables_by_owner: HashMap<i64, HashSet<i64>> = HashMap::new();
    for e in ctx.ws.edges() {
        if e.kind.as_str() != EdgeKind::WRITES_DB {
            continue;
        }
        // P8 propagation edges carry `via: "propagate"`; keep direct writes only, so that "called two writing
        // services" does not count as "this method itself wrote two tables".
        if e.properties.get("via").is_some() {
            continue;
        }
        tables_by_owner
            .entry(e.from_id.get())
            .or_default()
            .insert(e.to_id.get());
    }

    // (2) Per method: whether a transaction marker appears, plus a representative location (take the earliest write, so it reads on).
    struct Meta {
        owner_fqn: String,
        file: String,
        line: u32,
        in_tx: bool,
    }
    let mut meta: HashMap<i64, Meta> = HashMap::new();
    for call in ctx.ws.calls.iter() {
        // Transaction markers are **this sub-project's** declared knowledge. A stack that declares none is
        // not judged at all: "no marker appeared" would otherwise be trivially true for every method, which
        // is a false-positive flood rather than a finding. (This replaced a hard-coded `language == php`.)
        let Some(markers) = markers_for(ctx, call.sub) else {
            continue;
        };
        if markers.is_empty() {
            continue;
        }
        let entry = meta.entry(call.owner.get()).or_insert_with(|| Meta {
            owner_fqn: call.owner_fqn.clone(),
            file: call.file.clone(),
            line: call.span.start_line,
            in_tx: false,
        });
        if let Some(method) = call.method.as_deref() {
            if markers
                .iter()
                .any(|p| method.eq_ignore_ascii_case(p) || call.callee.eq_ignore_ascii_case(p))
            {
                entry.in_tx = true;
            }
        }
        if call.span.start_line < entry.line {
            entry.line = call.span.start_line;
            entry.file = call.file.clone();
        }
    }

    // (3) A hit: writes to >= 2 tables and no transaction marker inside the method.
    let mut targets: Vec<(NodeId, String, u32, usize, String)> = Vec::new();
    for (owner, tables) in tables_by_owner {
        if tables.len() < MIN_TABLES {
            continue;
        }
        let Some(m) = meta.get(&owner) else { continue };
        if m.in_tx {
            continue;
        }
        targets.push((
            NodeId(owner),
            m.file.clone(),
            m.line,
            tables.len(),
            m.owner_fqn.clone(),
        ));
    }

    for (node, file, line, n_tables, owner_fqn) in targets {
        ctx.ws.annotate(NewAnnotation {
            node_id: node,
            channel: AnnotationChannel("Tx".to_string()),
            kind: MULTI_WRITE.to_string(),
            subkind: Some("PartialWriteRisk".to_string()),
            confidence: 0.75,
            evidence: json!({
                "file": file,
                "line": line,
                "owner_fqn": owner_fqn,
                "tables": n_tables,
            }),
            phase: phase.clone(),
            merge: MergeStrategy::Coexist,
        });
        count += 1;
    }

    tracing::info!("P13 transaction check done: {} multi-table writes with no transaction boundary", count);
}

#[cfg(test)]
mod tests {
    use crate::context::PipelineContext;
    use crate::workspace::CallRecord;
    use gt_domain::model::{
        Annotation, EdgeKind, Language, NewEdge, NodeId, Phase, Project, ProjectId, ProjectStatus,
        SubProjectId,
    };
    use serde_json::json;
    use super::MULTI_WRITE;

    const SUB: i64 = 1;

    /// A context whose sub-project declares `tx` as its transaction markers (empty = declares nothing).
    fn ctx_with(tx: Vec<&str>) -> PipelineContext {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let markers: Vec<String> = tx.into_iter().map(|s| s.to_string()).collect();
        ctx.tx_calls.insert(SUB, markers);
        ctx
    }

    /// One direct `WritesDb` edge: `method -> table`.
    fn write(ctx: &mut PipelineContext, method: i64, table: i64) {
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: EdgeKind(EdgeKind::WRITES_DB.to_string()),
            from_id: NodeId(method),
            to_id: NodeId(table),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
    }

    fn call(ctx: &mut PipelineContext, owner: i64, method: &str) {
        ctx.ws.calls.push(CallRecord {
            node: NodeId(owner * 100),
            owner: NodeId(owner),
            owner_fqn: format!("App\\Svc::run{owner}"),
            owner_class: None,
            callee: method.to_string(),
            receiver: None,
            method: Some(method.to_string()),
            args: Vec::new(),
            span: gt_domain::model::Span {
                start_line: 10,
                end_line: 10,
                start_byte: 0,
                end_byte: 0,
            },
            file: "app/Svc.php".to_string(),
            language: Language::new(Language::PHP),
            sub: Some(SubProjectId::new(SUB)),
            db_table: None,
            in_loop: false,
            entity: None,
        });
    }

    fn annotated(ctx: &PipelineContext, node: i64) -> bool {
        ctx.ws
            .annotations()
            .iter()
            .any(|a| a.node_id == NodeId(node) && a.kind == MULTI_WRITE)
    }

    #[test]
    fn multi_table_write_without_marker_is_annotated() {
        let mut ctx = ctx_with(vec!["transaction", "commit"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);
        assert!(annotated(&ctx, 10), "writing two tables with no transaction marker must be annotated");
    }

    #[test]
    fn marker_inside_the_method_suppresses_it() {
        let mut ctx = ctx_with(vec!["transaction", "commit"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save");
        call(&mut ctx, 10, "transaction");
        super::run(&mut ctx);
        assert!(!annotated(&ctx, 10), "a transaction marker inside the method means no annotation");
    }

    #[test]
    fn single_table_write_is_not_annotated() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);
        assert!(!annotated(&ctx, 10), "writing a single table is not a multi-table write");
    }

    /// The point of the whole refactor: a stack that declares **no** transaction markers is not judged.
    /// "No marker appeared" would be trivially true for every method there, i.e. a false-positive flood.
    /// This is what replaced the hard-coded `language == php` gate.
    #[test]
    fn stack_without_declared_markers_is_not_judged() {
        let mut ctx = ctx_with(vec![]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);
        assert!(
            !annotated(&ctx, 10),
            "a stack with no declared transaction marker must not be judged (otherwise every multi-table write is a false positive)"
        );
    }

    #[test]
    fn tx_markers_are_case_insensitive() {
        let ctx = ctx_with(vec!["startTrans", "transaction"]);
        let m = &ctx.tx_calls[&SUB];
        assert!(m.iter().any(|p| p.eq_ignore_ascii_case("STARTTRANS")));
        assert!(m.iter().any(|p| p.eq_ignore_ascii_case("Transaction")));
        assert!(!m.iter().any(|p| p.eq_ignore_ascii_case("save")));
    }

    // ===== Below: the gaps the original 5 tests left open — the `via: propagate` exclusion, the
    // `tx_calls_default` fallback, case-insensitive matching inside `run()`, and the annotation contract. =====

    /// A `WritesDb` edge flagged `via: propagate` (P8's call-chain propagation) must NOT be counted as a
    /// direct write — otherwise "called two writing services" would be mis-read as "this method wrote two
    /// tables". This is the documented core of the predicate.
    #[test]
    fn propagated_writes_do_not_count_as_direct() {
        let mut ctx = ctx_with(vec!["transaction"]);
        // One DIRECT write to table 20, plus one INDIRECT (propagated) write to table 21.
        write(&mut ctx, 10, 20);
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: EdgeKind(EdgeKind::WRITES_DB.to_string()),
            from_id: NodeId(10),
            to_id: NodeId(21),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: json!({ "via": "propagate" }),
        });
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);
        // Only ONE direct table -> below the MIN_TABLES(2) threshold -> must not be flagged.
        assert!(!annotated(&ctx, 10), "an indirectly propagated write edge must not count towards the direct-write table count");
    }

    /// Two direct writes plus an extra propagated write still count as a multi-table write.
    #[test]
    fn direct_writes_still_count_with_indirect_extra() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21); // two direct tables
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: EdgeKind(EdgeKind::WRITES_DB.to_string()),
            from_id: NodeId(10),
            to_id: NodeId(22),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: json!({ "via": "propagate" }),
        });
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);
        assert!(annotated(&ctx, 10), "two directly written tables + one indirect edge must still be annotated");
    }

    /// The global `tx_calls_default` is consulted when a sub-project declares nothing of its own.
    fn ctx_with_default(tx: Vec<&str>) -> PipelineContext {
        let mut ctx = ctx_with(vec![]); // no per-sub markers
        ctx.tx_calls.clear(); // drop the empty per-sub entry so the default is actually consulted
        ctx.tx_calls_default = tx.into_iter().map(|s| s.to_string()).collect();
        ctx
    }

    #[test]
    fn default_markers_judge_when_sub_declares_none() {
        let mut ctx = ctx_with_default(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);
        // default markers apply; no marker call -> still flagged
        assert!(annotated(&ctx, 10), "a sub-project with no declaration falls back to the default transaction-marker vocabulary");
    }

    #[test]
    fn default_marker_inside_method_suppresses_it() {
        let mut ctx = ctx_with_default(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "transaction");
        super::run(&mut ctx);
        assert!(!annotated(&ctx, 10), "a marker from the default vocabulary appearing suppresses the annotation");
    }

    /// `run()` matches transaction markers case-insensitively: the declaration may be `startTrans`, the call
    /// site spelled `STARTTRANS`.
    #[test]
    fn marker_match_is_case_insensitive_in_run() {
        let mut ctx = ctx_with(vec!["startTrans"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "STARTTRANS");
        super::run(&mut ctx);
        assert!(!annotated(&ctx, 10), "case-insensitive marker matching must take effect inside run()");
    }

    /// The annotation's contract: kind / subkind / channel / confidence / evidence are fixed.
    #[test]
    fn annotation_carries_partial_write_risk_contract() {
        let mut ctx = ctx_with(vec!["transaction", "commit"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);

        let a = ctx
            .ws
            .annotations()
            .iter()
            .find(|a| a.node_id == NodeId(10) && a.kind == MULTI_WRITE)
            .expect("expected a multi-write-without-tx annotation");
        assert_eq!(a.subkind.as_deref(), Some("PartialWriteRisk"));
        assert_eq!(a.channel.0, "Tx");
        assert!((a.confidence - 0.75).abs() < 1e-3);
        assert_eq!(a.evidence["tables"], json!(2));
        assert_eq!(a.evidence["owner_fqn"], json!("App\\Svc::run10"));
    }

    // ===== Below: the marker-lookup semantics (`markers_for`) and the remaining `run()` gates =====

    /// A call site the parser split into callee text + method, on an explicit line / file / sub-project.
    fn call_at(
        ctx: &mut PipelineContext,
        owner: i64,
        callee: &str,
        method: &str,
        line: u32,
        file: &str,
        sub: Option<i64>,
    ) {
        ctx.ws.calls.push(CallRecord {
            node: NodeId(owner * 100 + line as i64),
            owner: NodeId(owner),
            owner_fqn: format!("App\\Svc::run{owner}"),
            owner_class: None,
            callee: callee.to_string(),
            receiver: None,
            method: Some(method.to_string()),
            args: Vec::new(),
            span: gt_domain::model::Span {
                start_line: line,
                end_line: line,
                start_byte: 0,
                end_byte: 0,
            },
            file: file.to_string(),
            language: Language::new(Language::PHP),
            sub: sub.map(SubProjectId::new),
            db_table: None,
            in_loop: false,
            entity: None,
        });
    }

    fn annotation_of(ctx: &PipelineContext, node: i64) -> Option<&Annotation> {
        ctx.ws
            .annotations()
            .iter()
            .find(|a| a.node_id == NodeId(node) && a.kind == MULTI_WRITE)
    }

    /// `markers_for` **replaces** rather than merges: once the sub-project declares its own (non-empty) list,
    /// the global fallback stops participating.
    #[test]
    fn sub_markers_replace_the_default_rather_than_merging() {
        let mut ctx = ctx_with(vec!["transaction"]);
        ctx.tx_calls_default = vec!["commit".to_string()];
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        // `commit` is only in the default list, which this sub does not consult.
        call(&mut ctx, 10, "commit");

        super::run(&mut ctx);

        assert!(
            annotated(&ctx, 10),
            "when a sub-project has its own marker vocabulary the default one does not take part (no merging)"
        );
    }

    /// The subtle half of `markers_for`: a sub-project that is **present but empty** declares "this stack has
    /// no transaction markers", so it must NOT fall back to the global default — only a *missing* entry does
    /// (see `default_markers_judge_when_sub_declares_none`).
    #[test]
    fn an_empty_per_sub_declaration_does_not_fall_back_to_the_default() {
        let mut ctx = ctx_with(vec![]); // sub 1 present, but declares nothing
        ctx.tx_calls_default = vec!["transaction".to_string()];
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save");

        super::run(&mut ctx);

        assert!(
            !annotated(&ctx, 10),
            "a stack that declares an empty vocabulary does not fall back to the default: an empty declaration means 'this stack has no notion of transactions'"
        );
    }

    /// Call sites that carry no sub-project fall back to the global markers.
    #[test]
    fn call_sites_without_a_sub_use_the_default_markers() {
        let mut ctx = ctx_with_default(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call_at(&mut ctx, 10, "save", "save", 10, "app/Svc.php", None);

        super::run(&mut ctx);

        assert!(annotated(&ctx, 10), "a call site with no sub must be judged by the default vocabulary");
    }

    /// A declaration may name the **qualified** call (`Db::startTrans`); matching must consider the callee text
    /// and not only the split-off method name.
    #[test]
    fn marker_matches_the_qualified_callee_text() {
        let mut ctx = ctx_with(vec!["Db::startTrans"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        // method is `startTrans`, which does NOT equal the declared marker — only the callee text does.
        call_at(
            &mut ctx,
            10,
            "Db::startTrans",
            "startTrans",
            10,
            "app/Svc.php",
            Some(SUB),
        );

        super::run(&mut ctx);

        assert!(
            !annotated(&ctx, 10),
            "a fully qualified call text must match the transaction marker too"
        );
    }

    /// Only `WritesDb` builds the table set: reading two tables is not a partial-write risk.
    #[test]
    fn only_writes_db_edges_count_toward_the_threshold() {
        let mut ctx = ctx_with(vec!["transaction"]);
        for t in [20, 21] {
            ctx.ws.add_edge(NewEdge {
                project_id: ctx.project.id,
                kind: EdgeKind(EdgeKind::READS_DB.to_string()),
                from_id: NodeId(10),
                to_id: NodeId(t),
                phase: Phase("Test".to_string()),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            });
        }
        call(&mut ctx, 10, "save");

        super::run(&mut ctx);

        assert!(!annotated(&ctx, 10), "read-only is not a multi-table write");
    }

    /// With no call site at all the phase cannot know whether a marker appears, so it stays silent —
    /// the conservative direction the module documents.
    #[test]
    fn method_without_any_call_site_is_not_judged() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        // No `call(...)` at all.

        super::run(&mut ctx);

        assert!(
            !annotated(&ctx, 10),
            "with no call site there is no way to know whether a transaction marker appears, so it must conservatively skip"
        );
    }

    /// The judgement is per method: a marker in one method must not clear another method's finding.
    #[test]
    fn methods_are_judged_independently() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save"); // no marker -> hit
        write(&mut ctx, 11, 20);
        write(&mut ctx, 11, 21);
        call(&mut ctx, 11, "save");
        call(&mut ctx, 11, "transaction"); // marker -> suppressed

        super::run(&mut ctx);

        assert!(annotated(&ctx, 10), "a method with no marker must be annotated");
        assert!(!annotated(&ctx, 11), "a method with a marker must not be annotated (no bleeding between methods)");
    }

    /// The evidence points at the **earliest** call site of the method, wherever it was registered in the list.
    #[test]
    fn evidence_points_at_the_earliest_call_site() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call_at(&mut ctx, 10, "save", "save", 42, "app/later.php", Some(SUB));
        call_at(&mut ctx, 10, "check", "check", 7, "app/early.php", Some(SUB));

        super::run(&mut ctx);

        let a = annotation_of(&ctx, 10).expect("expected a multi-write-without-tx annotation");
        assert_eq!(a.evidence["line"], json!(7), "the line number must come from the earliest call site");
        assert_eq!(a.evidence["file"], json!("app/early.php"));
    }

    /// `tables` reports the real count, not the threshold.
    #[test]
    fn table_count_in_evidence_is_the_real_count() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        write(&mut ctx, 10, 22);
        call(&mut ctx, 10, "save");

        super::run(&mut ctx);

        let a = annotation_of(&ctx, 10).expect("expected a multi-write-without-tx annotation");
        assert_eq!(a.evidence["tables"], json!(3), "the evidence must record the real table count, not the threshold");
    }

    /// A method whose call sites all belong to a stack that declares no markers is not judged, even when
    /// another sub-project does declare them — the lookup is per call site's sub, not global.
    #[test]
    fn multi_write_is_skipped_when_the_call_sites_belong_to_a_stack_without_markers() {
        let mut ctx = ctx_with(vec!["transaction"]); // only sub 1 declares markers
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call_at(&mut ctx, 10, "save", "save", 10, "app/Svc.php", Some(2));

        super::run(&mut ctx);

        assert!(
            !annotated(&ctx, 10),
            "no judgement when the stack owning the call site declares no marker vocabulary"
        );
    }

    // ===== remaining branches =====

    /// A call site the parser could not split into a method name (a dynamic `$cb()`): `run` still registers
    /// the method from it, so a multi-table write there **is** judged — the marker check simply has nothing
    /// to match against. Pins the `if let Some(method)` arm.
    fn call_no_method(
        ctx: &mut PipelineContext,
        owner: i64,
        callee: &str,
        line: u32,
        file: &str,
        sub: Option<i64>,
    ) {
        ctx.ws.calls.push(CallRecord {
            node: NodeId(owner * 100 + line as i64),
            owner: NodeId(owner),
            owner_fqn: format!("App\\Svc::run{owner}"),
            owner_class: None,
            callee: callee.to_string(),
            receiver: None,
            method: None,
            args: Vec::new(),
            span: gt_domain::model::Span {
                start_line: line,
                end_line: line,
                start_byte: 0,
                end_byte: 0,
            },
            file: file.to_string(),
            language: Language::new(Language::PHP),
            sub: sub.map(SubProjectId::new),
            db_table: None,
            in_loop: false,
            entity: None,
        });
    }

    #[test]
    fn a_call_site_without_a_method_name_still_registers_the_method() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        // Only a method-less call site: the method still gets a location, hence is judged.
        call_no_method(&mut ctx, 10, "$cb()", 10, "app/Svc.php", Some(SUB));

        // The same writes plus a real marker call: the marker still suppresses.
        write(&mut ctx, 11, 20);
        write(&mut ctx, 11, 21);
        call_no_method(&mut ctx, 11, "$cb()", 10, "app/Svc.php", Some(SUB));
        call(&mut ctx, 11, "transaction");

        super::run(&mut ctx);

        assert!(
            annotated(&ctx, 10),
            "a call site with no method name still registers the method (its position is known), so a multi-table write must be judged"
        );
        assert!(
            !annotated(&ctx, 11),
            "a coexisting real transaction-marker call must still suppress"
        );
    }

    /// The marker lookup is **per call site's sub-project**: a `transaction` call recorded in a stack that
    /// declares no markers is not recognised, so it must not silence the finding. Complements
    /// `multi_write_is_skipped_when_the_call_sites_belong_to_a_stack_without_markers`.
    #[test]
    fn a_marker_call_in_a_stack_without_markers_does_not_suppress() {
        let mut ctx = ctx_with(vec!["transaction"]); // only sub 1 declares
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call_at(&mut ctx, 10, "save", "save", 10, "app/Svc.php", Some(SUB));
        call_at(
            &mut ctx,
            10,
            "transaction",
            "transaction",
            12,
            "app/Svc.php",
            Some(2), // a stack that declares nothing -> the call is not even consulted
        );

        super::run(&mut ctx);

        assert!(
            annotated(&ctx, 10),
            "a transaction call in a stack that declares no marker vocabulary must not suppress"
        );
    }

    /// The evidence location is the earliest call site **among those that could be judged** — a call site
    /// whose stack declares no markers is skipped entirely, so it contributes no location even when it is
    /// the earliest line in the method.
    #[test]
    fn evidence_points_at_the_earliest_eligible_call_site() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        // Earliest line, but its stack declares nothing -> skipped, contributes nothing.
        call_at(&mut ctx, 10, "a", "a", 5, "app/other_stack.php", Some(2));
        // Earliest *eligible* call site.
        call_at(&mut ctx, 10, "save", "save", 20, "app/later.php", Some(SUB));

        super::run(&mut ctx);

        let a = annotation_of(&ctx, 10).expect("expected a multi-write-without-tx annotation");
        assert_eq!(
            a.evidence["line"],
            json!(20),
            "the position must be the earliest among the 'judgeable' call sites, not the earliest of all"
        );
        assert_eq!(a.evidence["file"], json!("app/later.php"));
    }
}
