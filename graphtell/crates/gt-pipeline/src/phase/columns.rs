//! Settle the **authoritative schema's columns** into `Column` graph nodes.
//!
//! Kept only in the symbol table (the `schema` table, loaded by P3 from SQL install scripts / table-name call
//! sites), column information can only be "asked a question" by the `ColumnsMatch` predicate (e.g. CRMEB tagging
//! tables that contain phone numbers with PII); on the graph it is **neither visible nor traversable** — impact
//! analysis stops at the table and cannot drill down to field level.
//!
//! Here it is materialised onto the graph: `Table --HasColumn--> Column`.
//! It is **the same shape** as the columns built from TypeORM's `@Column` (a `Column` node + a `HasColumn` edge);
//! only the source differs: there it is the ORM's field declaration, here it is the authoritative DDL structure.
//!
//! Language / framework agnostic: it only looks at "a `Table` node plus whether the schema symbol table has columns
//! for it", so any project with an SQL install script (PHP or otherwise) gets field-level graph nodes automatically.
//! A table without a schema (e.g. only an ORM declaration, no DDL) produces no columns — better missing than guessed.

use gt_domain::model::{
    EdgeKind, IdentityKey, Language, NewEdge, NodeId, NodeKind, Phase, Span, SubProjectId,
};
use serde_json::json;

use crate::context::PipelineContext;
use crate::engine::schema_columns;
use crate::workspace::synthesized_node;

/// The phase that settling columns belongs to: attached to P6 (by then P5's `Table` nodes and P3's schema are both in place).
pub fn materialize(ctx: &mut PipelineContext) {
    let phase = Phase(Phase::ANNOTATE_POST.to_string());

    // Collect "table -> columns" first and only then touch the graph: both `add_edge` and
    // `get_or_create_synthesized` need &mut ws, so the graph cannot be mutated while iterating its nodes.
    let mut plan: Vec<(NodeId, String, Vec<String>, Option<SubProjectId>, Language)> = Vec::new();
    for id in ctx.ws.node_ids() {
        let Some(node) = ctx.ws.node(id) else {
            continue;
        };
        if node.kind.as_str() != NodeKind::TABLE {
            continue;
        }
        // `identity.value` is the normalised table name (the same source as the schema symbol table's keys), so prefer it.
        let name = node
            .identity
            .as_ref()
            .map(|i| i.value.clone())
            .unwrap_or_else(|| node.name.clone());
        if name.is_empty() {
            continue;
        }
        // The table name may be **plural**: Laravel's DDL writes `CREATE TABLE users`, while the table node's name
        // is `user` after `singularize` — a direct lookup misses, so the plural form is tried as well.
        let cols = schema_columns(&ctx.ws, "schema", &name)
            .or_else(|| schema_columns(&ctx.ws, "schema", &format!("{name}s")))
            .unwrap_or_default();
        if cols.is_empty() {
            continue;
        }
        plan.push((
            id,
            name,
            cols,
            node.sub_project_id,
            node.language.clone(),
        ));
    }

    for (table_id, tname, cols, sub, language) in plan {
        for col in cols {
            // A column's identity is scoped by table name: same-named columns (`id` / `created`) in different tables
            // must each count separately, otherwise they merge idempotently into one wrong node and link two tables'
            // impact surfaces together.
            let identity = IdentityKey::named(format!("{tname}.{col}"));
            // `Column` is a semantic kind declared by FKB (see `semantic_kinds` in `fkb/js/typeorm.yaml`);
            // it is not among the kernel constants, so the kind string is written directly.
            let mut new_node = synthesized_node(
                ctx.project.id,
                "Column",
                identity,
                sub,
                &phase,
                0.9,
                &language,
                Span::default(),
            );
            new_node.properties = json!({ "category": "Column", "sources": ["schema"], "column": col });
            let (col_id, _created) = ctx.ws.get_or_create_synthesized(new_node);
            ctx.ws.add_edge(NewEdge {
                project_id: ctx.project.id,
                kind: EdgeKind("HasColumn".to_string()),
                from_id: table_id,
                to_id: col_id,
                phase: phase.clone(),
                confidence: 0.9,
                properties: serde_json::Value::Null,
            });
        }
    }
}
