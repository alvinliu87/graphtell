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
//! verbs" to ">= 2 tables", likeshop's hit count dropped from 105 to a far more credible level and the false
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
        EdgeKind, Language, NewEdge, NodeId, Phase, Project, ProjectId, ProjectStatus, SubProjectId,
    };
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
        assert!(annotated(&ctx, 10), "写两张表且无事务标记应被标注");
    }

    #[test]
    fn marker_inside_the_method_suppresses_it() {
        let mut ctx = ctx_with(vec!["transaction", "commit"]);
        write(&mut ctx, 10, 20);
        write(&mut ctx, 10, 21);
        call(&mut ctx, 10, "save");
        call(&mut ctx, 10, "transaction");
        super::run(&mut ctx);
        assert!(!annotated(&ctx, 10), "方法内出现事务标记则不应标注");
    }

    #[test]
    fn single_table_write_is_not_annotated() {
        let mut ctx = ctx_with(vec!["transaction"]);
        write(&mut ctx, 10, 20);
        call(&mut ctx, 10, "save");
        super::run(&mut ctx);
        assert!(!annotated(&ctx, 10), "只写一张表不算多表写");
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
            "未声明事务标记的栈不应被判定（否则每个多表写都误报）"
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
}
