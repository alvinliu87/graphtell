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
    AnnotationChannel, EdgeKind, Language, MergeStrategy, NewAnnotation, NodeId, Phase,
};
use serde_json::json;

use crate::context::PipelineContext;

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
        if call.language.0 != Language::PHP {
            continue;
        }
        let entry = meta.entry(call.owner.get()).or_insert_with(|| Meta {
            owner_fqn: call.owner_fqn.clone(),
            file: call.file.clone(),
            line: call.span.start_line,
            in_tx: false,
        });
        if let Some(method) = call.method.as_deref() {
            if ctx
                .tx_calls
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
    use gt_domain::model::{Project, ProjectStatus};

    fn ctx_with(tx: Vec<&str>) -> PipelineContext {
        let mut ctx = PipelineContext::new(Project {
            id: gt_domain::model::ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.tx_calls = tx.into_iter().map(|s| s.to_string()).collect();
        ctx
    }

    #[test]
    fn tx_markers_are_case_insensitive() {
        let ctx = ctx_with(vec!["startTrans", "transaction"]);
        // The predicate itself lives in run(); this only pins the case-insensitive convention of the name list
        assert!(ctx.tx_calls.iter().any(|p| p.eq_ignore_ascii_case("STARTTRANS")));
        assert!(ctx.tx_calls.iter().any(|p| p.eq_ignore_ascii_case("Transaction")));
        assert!(!ctx.tx_calls.iter().any(|p| p.eq_ignore_ascii_case("save")));
    }
}
