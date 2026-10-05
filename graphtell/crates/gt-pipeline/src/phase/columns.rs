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

#[cfg(test)]
mod tests {
    use gt_domain::model::{
        IdentityKey, Language, NewNode, NodeId, NodeKind, Phase, Project, ProjectId, ProjectStatus,
        Span,
    };
    use serde_json::json;

    use crate::context::PipelineContext;
    use crate::workspace::synthesized_node;

    fn ctx_with_project() -> PipelineContext {
        PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        })
    }

    fn put_schema(ctx: &mut PipelineContext, table: &str, cols: &[&str]) {
        ctx.ws.put_symbol(
            ProjectId(1),
            "schema",
            table,
            json!({ "columns": cols }),
        );
    }

    fn add_table(ctx: &mut PipelineContext, identity_value: &str) -> NodeId {
        let phase = Phase(Phase::ANNOTATE_POST.to_string());
        let (id, _) = ctx.ws.get_or_create_synthesized(synthesized_node(
            ProjectId(1),
            "Table",
            IdentityKey::named(identity_value),
            None,
            &phase,
            0.9,
            &Language::new("php"),
            Span::default(),
        ));
        id
    }

    /// Identity values of every `Column` node in the graph.
    fn column_identities(ctx: &PipelineContext) -> Vec<String> {
        ctx.ws
            .node_ids()
            .iter()
            .filter_map(|id| {
                let n = ctx.ws.node(*id)?;
                if n.kind.as_str() == "Column" {
                    n.identity.as_ref().map(|i| i.value.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    fn has_column_count(ctx: &PipelineContext) -> usize {
        ctx.ws
            .edges()
            .iter()
            .filter(|e| e.kind.as_str() == "HasColumn")
            .count()
    }

    #[test]
    fn materialize_plural_fallback_finds_columns() {
        // Table node is named `user` but the DDL key is `users` — the plural fallback must still find columns.
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "users", &["id", "phone"]);
        super::materialize(&mut ctx);
        let cols = column_identities(&ctx);
        assert_eq!(cols.len(), 2, "user must pull 2 columns from plural `users`");
        assert!(cols.contains(&"user.id".to_string()));
        assert!(cols.contains(&"user.phone".to_string()));
        assert_eq!(has_column_count(&ctx), 2);
    }

    #[test]
    fn materialize_columns_scoped_per_table() {
        // The same column name (`id`) in two tables must stay two distinct nodes, not merge.
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        add_table(&mut ctx, "order");
        put_schema(&mut ctx, "users", &["id"]);
        put_schema(&mut ctx, "orders", &["id"]);
        super::materialize(&mut ctx);
        let cols = column_identities(&ctx);
        assert_eq!(cols.len(), 2, "same column name across tables must not merge");
        assert!(cols.contains(&"user.id".to_string()));
        assert!(cols.contains(&"order.id".to_string()));
        assert_eq!(has_column_count(&ctx), 2);
    }

    #[test]
    fn materialize_no_schema_no_columns() {
        // A table with no DDL/schema entry must produce no columns (better missing than guessed).
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "ghost");
        super::materialize(&mut ctx);
        assert_eq!(column_identities(&ctx).len(), 0);
        assert_eq!(has_column_count(&ctx), 0);
    }

    #[test]
    fn materialize_identity_value_preferred_over_name() {
        // Display name `UserModel` differs from the schema identity `user`; resolution must use `identity.value`.
        let mut ctx = ctx_with_project();
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(NodeKind::TABLE.to_string()),
            name: "UserModel".into(),
            fqn: None,
            identity: Some(IdentityKey::named("user")),
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase(Phase::ANNOTATE_POST.to_string()),
            confidence: 0.9,
            properties: serde_json::Value::Null,
        });
        // Only `users` exists; if `name` were used the lookup `UserModel`/`UserModels` would miss.
        put_schema(&mut ctx, "users", &["id"]);
        super::materialize(&mut ctx);
        let cols = column_identities(&ctx);
        assert_eq!(cols.len(), 1, "columns must resolve via identity.value, not display name");
        assert_eq!(cols[0], "user.id");
    }

    #[test]
    fn materialize_exact_singular_key_matches_without_fallback() {
        // Primary lookup path: the schema key equals the table identity exactly, so no plural fallback is needed.
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "user", &["id", "phone"]);
        super::materialize(&mut ctx);
        let cols = column_identities(&ctx);
        assert_eq!(cols.len(), 2, "singular key must match directly");
        assert!(cols.contains(&"user.id".to_string()));
        assert!(cols.contains(&"user.phone".to_string()));
        assert_eq!(has_column_count(&ctx), 2);
    }

    #[test]
    fn materialize_ignores_non_table_nodes() {
        // Only Table nodes are materialised; a Method (or any non-table) must be left alone even if a schema
        // entry exists for a same-named key.
        let mut ctx = ctx_with_project();
        let method = ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind::from("Method"),
            name: "user".into(),
            fqn: None,
            identity: Some(IdentityKey::named("user")),
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase(Phase::ANNOTATE_POST.to_string()),
            confidence: 0.9,
            properties: serde_json::Value::Null,
        });
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "users", &["id"]);
        super::materialize(&mut ctx);
        // Only the table produced a column; the Method produced none.
        assert_eq!(column_identities(&ctx), vec!["user.id".to_string()]);
        assert_eq!(has_column_count(&ctx), 1, "exactly one HasColumn edge (from the table)");
        // The Method node must not have gained a HasColumn edge.
        assert!(ctx.ws.out_edges_of(method).is_empty());
    }

    #[test]
    fn materialize_skips_table_with_empty_name() {
        // A Table node whose resolved name is empty (blank identity, no usable display name) cannot be
        // looked up against the schema; it must be skipped rather than panic or fabricate a column.
        let mut ctx = ctx_with_project();
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(NodeKind::TABLE.to_string()),
            name: "whatever".into(),
            fqn: None,
            identity: Some(IdentityKey::named("")),
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase(Phase::ANNOTATE_POST.to_string()),
            confidence: 0.9,
            properties: serde_json::Value::Null,
        });
        // Schema keys that would match if the (empty) name were mis-used.
        put_schema(&mut ctx, "", &["id"]);
        put_schema(&mut ctx, "whatever", &["id"]);
        super::materialize(&mut ctx);
        assert_eq!(column_identities(&ctx).len(), 0, "empty-name table must be skipped");
        assert_eq!(has_column_count(&ctx), 0);
    }

    #[test]
    fn materialize_column_node_carries_schema_properties() {
        // Each synthesised Column node records where it came from and its column name in `properties`
        // (the kernel cannot hard-code which semantic kinds exist, so `category`/`sources`/`column` are written).
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "users", &["phone"]);
        super::materialize(&mut ctx);
        let col_id = ctx
            .ws
            .node_ids()
            .into_iter()
            .find(|id| {
                ctx.ws.node(*id).map(|n| {
                    n.kind.as_str() == "Column"
                        && n.identity.as_ref().map(|i| i.value.as_str()) == Some("user.phone")
                }).unwrap_or(false)
            })
            .expect("phone column node");
        let props = ctx.ws.node(col_id).unwrap().properties.clone();
        assert_eq!(props.get("column").and_then(|v| v.as_str()), Some("phone"));
        assert_eq!(props.get("category").and_then(|v| v.as_str()), Some("Column"));
        let sources = props.get("sources").and_then(|v| v.as_array()).expect("sources array");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources.get(0).and_then(|v| v.as_str()), Some("schema"));
    }
}
