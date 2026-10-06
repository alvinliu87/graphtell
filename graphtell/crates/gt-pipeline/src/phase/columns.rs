//! Settle the **authoritative schema's columns** into `Column` graph nodes.
//!
//! Kept only in the symbol table (the `schema` table, loaded by P3 from SQL install scripts / table-name call
//! sites), column information can only be "asked a question" by the `ColumnsMatch` predicate (e.g. sample_project tagging
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
        Span, SubProjectId,
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

    /// Put whatever shape into the `schema` table, so malformed entries can be exercised too.
    fn put_schema_raw(ctx: &mut PipelineContext, table: &str, value: serde_json::Value) {
        ctx.ws.put_symbol(ProjectId(1), "schema", table, value);
    }

    /// A `Table` node built directly (not through the synthesized-identity merge), so language /
    /// sub-project can differ from the `add_table` default and duplicates stay possible.
    fn add_table_node(
        ctx: &mut PipelineContext,
        name: &str,
        identity: Option<&str>,
        lang: &str,
        sub: Option<SubProjectId>,
    ) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: sub,
            kind: NodeKind(NodeKind::TABLE.to_string()),
            name: name.into(),
            fqn: None,
            identity: identity.map(IdentityKey::named),
            file_id: None,
            span: Span::default(),
            language: Language::new(lang),
            phase: Phase(Phase::ANNOTATE_POST.to_string()),
            confidence: 0.9,
            properties: serde_json::Value::Null,
        })
    }

    fn column_node(ctx: &PipelineContext, identity_value: &str) -> NodeId {
        ctx.ws
            .node_ids()
            .into_iter()
            .find(|id| {
                ctx.ws.node(*id).is_some_and(|n| {
                    n.kind.as_str() == "Column"
                        && n.identity.as_ref().is_some_and(|i| i.value == identity_value)
                })
            })
            .unwrap_or_else(|| panic!("column node `{identity_value}` should exist"))
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

    // ===================================================================
    // Negative / degradation coverage
    //
    // Above: the shapes that produce columns. Below: everything that must produce **nothing** —
    // malformed schema entries, the lookup precedence that is invisible when only one key exists,
    // and the "re-run must not duplicate" contract every incremental rebuild relies on.
    // ===================================================================

    #[test]
    fn materialize_schema_entry_without_usable_columns_yields_nothing() {
        // An entry that exists but carries no column list must be treated as "no schema", not as a panic
        // and not as a column named `null`.
        for value in [
            json!({ "columns": [] }),
            json!({ "table": "user" }),
            json!({ "columns": null }),
            json!({ "columns": "id,phone" }),
            json!({ "columns": { "0": "id" } }),
        ] {
            let mut ctx = ctx_with_project();
            add_table(&mut ctx, "user");
            put_schema_raw(&mut ctx, "user", value.clone());
            super::materialize(&mut ctx);
            assert_eq!(
                column_identities(&ctx).len(),
                0,
                "no column node for a schema entry shaped like {value}"
            );
            assert_eq!(has_column_count(&ctx), 0, "no edge either for {value}");
        }
    }

    #[test]
    fn materialize_non_string_column_entries_are_dropped() {
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        // `42` / `null` / `true` are not column names; only the real strings survive.
        put_schema_raw(&mut ctx, "users", json!({ "columns": [42, null, "id", true] }));
        super::materialize(&mut ctx);
        let cols = column_identities(&ctx);
        assert_eq!(cols, vec!["user.id".to_string()], "non-string entries must be filtered out");
        assert_eq!(has_column_count(&ctx), 1);
    }

    /// Known wart, pinned: `schema_columns` returns `Some(vec![])` for a key that exists but has no
    /// columns, and `Option::or_else` short-circuits on `Some`, so the plural fallback never runs. A
    /// project whose P3 wrote `user` (no columns) next to real columns under `users` silently loses
    /// those columns at this phase. Fixing it means treating an empty result as a miss.
    #[test]
    fn materialize_empty_schema_entry_suppresses_the_plural_fallback() {
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema_raw(&mut ctx, "user", json!({ "columns": [] }));
        put_schema(&mut ctx, "users", &["id", "phone"]);
        super::materialize(&mut ctx);
        assert_eq!(
            column_identities(&ctx).len(),
            0,
            "the empty `user` entry shadows the populated `users` entry"
        );

        // Control: remove the empty entry and the very same schema is found through the plural fallback.
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "users", &["id", "phone"]);
        super::materialize(&mut ctx);
        assert_eq!(column_identities(&ctx).len(), 2, "without the empty entry the fallback works");
    }

    #[test]
    fn materialize_exact_key_wins_over_the_plural_fallback() {
        // Both keys exist with **different** contents: the exact key must win outright — the two lists
        // must never be merged (that would conjure columns the table does not have).
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "user", &["id"]);
        put_schema(&mut ctx, "users", &["phone"]);
        super::materialize(&mut ctx);
        let cols = column_identities(&ctx);
        assert_eq!(cols, vec!["user.id".to_string()], "the plural key must not contribute");
        assert_eq!(has_column_count(&ctx), 1);
    }

    #[test]
    fn materialize_falls_back_to_the_node_name_without_an_identity() {
        // No identity key: the display name is the lookup key.
        let mut ctx = ctx_with_project();
        add_table_node(&mut ctx, "order", None, "php", None);
        put_schema(&mut ctx, "orders", &["id"]);
        super::materialize(&mut ctx);
        assert_eq!(
            column_identities(&ctx),
            vec!["order.id".to_string()],
            "the display name must be used when there is no identity"
        );

        // Nothing usable: no identity *and* an empty name -> skipped, same as the empty-identity case.
        let mut ctx = ctx_with_project();
        add_table_node(&mut ctx, "", None, "php", None);
        put_schema_raw(&mut ctx, "", json!({ "columns": ["id"] }));
        super::materialize(&mut ctx);
        assert_eq!(column_identities(&ctx).len(), 0, "an empty resolved name must be skipped");
    }

    #[test]
    fn materialize_resolves_a_prefixed_table_via_the_stripped_key() {
        // Table nodes keep the project's own prefix (`eb_store_order`) while the DDL key may be the bare
        // name — `schema_columns` tries the stripped form first.
        let mut ctx = ctx_with_project();
        ctx.ws.set_table_prefixes(vec!["eb_".to_string()]);
        add_table(&mut ctx, "eb_store_order");
        put_schema(&mut ctx, "store_order", &["id"]);
        super::materialize(&mut ctx);
        assert_eq!(
            column_identities(&ctx),
            vec!["eb_store_order.id".to_string()],
            "found through the stripped key, but the identity keeps the full name so it cannot collide with a bare `store_order` table"
        );

        // The plural fallback still runs on the **full** name, so `eb_store_orders` is found as well.
        let mut ctx = ctx_with_project();
        ctx.ws.set_table_prefixes(vec!["eb_".to_string()]);
        add_table(&mut ctx, "eb_store_order");
        put_schema(&mut ctx, "eb_store_orders", &["phone"]);
        super::materialize(&mut ctx);
        assert_eq!(column_identities(&ctx), vec!["eb_store_order.phone".to_string()]);
    }

    #[test]
    fn materialize_is_idempotent_across_reruns() {
        // The phase may be replayed (watch rebuild, idempotent re-annotate); a replay must not add a
        // second Column node nor a second HasColumn edge.
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "users", &["id", "phone"]);
        super::materialize(&mut ctx);
        let once = (column_identities(&ctx).len(), has_column_count(&ctx));
        assert_eq!(once, (2, 2));
        super::materialize(&mut ctx);
        assert_eq!(
            (column_identities(&ctx).len(), has_column_count(&ctx)),
            once,
            "replaying the phase must not duplicate nodes or edges"
        );
    }

    #[test]
    fn materialize_duplicate_column_names_collapse_to_one_node() {
        // A repeated column in one DDL list must not create two nodes or two parallel edges.
        let mut ctx = ctx_with_project();
        add_table(&mut ctx, "user");
        put_schema(&mut ctx, "users", &["id", "id", "phone"]);
        super::materialize(&mut ctx);
        assert_eq!(column_identities(&ctx).len(), 2, "two distinct columns");
        assert_eq!(has_column_count(&ctx), 2, "the repeated one contributes one edge only");
        column_node(&ctx, "user.id");
    }

    #[test]
    fn materialize_tables_sharing_a_name_share_the_column_node() {
        // Two Table nodes resolving to the same name converge on **one** Column node (identity merge)
        // while each keeps its own HasColumn edge.
        let mut ctx = ctx_with_project();
        add_table_node(&mut ctx, "user", Some("user"), "php", None);
        add_table_node(&mut ctx, "user", Some("user"), "php", None);
        put_schema(&mut ctx, "users", &["id"]);
        super::materialize(&mut ctx);
        assert_eq!(column_identities(&ctx), vec!["user.id".to_string()], "one shared node");
        assert_eq!(has_column_count(&ctx), 2, "one edge per table");
    }

    #[test]
    fn materialize_column_node_inherits_language_and_sub_project() {
        // Nothing here may be hard-coded PHP: the language / sub-project come from the Table node, so
        // a Java sub-project's columns are attributed to it.
        let mut ctx = ctx_with_project();
        add_table_node(&mut ctx, "order", Some("order"), "java", Some(SubProjectId(7)));
        put_schema(&mut ctx, "orders", &["id"]);
        super::materialize(&mut ctx);
        let col = column_node(&ctx, "order.id");
        let node = ctx.ws.node(col).expect("column node");
        assert_eq!(node.language.as_str(), "java", "language must come from the table");
        assert_eq!(node.sub_project_id, Some(SubProjectId(7)), "sub-project likewise");
        assert_eq!(node.phase.0, Phase::ANNOTATE_POST, "the settle phase owns these nodes");
        assert_eq!(node.confidence, 0.9);
    }
}
