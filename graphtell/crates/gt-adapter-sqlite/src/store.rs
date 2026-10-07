//! The SQLite repository implementation.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Mutex,
};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    Annotation, AnnotationChannel, Diagnostic, Edge, EdgeId, EdgeKind, FileId, GraphDelta,
    Language, NewProject, NewSourceFile, NewSubProject,
    Node, NodeId, NodeKind, Phase, Project, ProjectConfig, ProjectId, ProjectPatch, ProjectRuleConfig,
    ProjectStatus, Severity, SourceFile, Span, SubProject, SubProjectId, SymbolEntry,
};
use gt_domain::port::{
    DiagnosticSink, EdgeDirection, GraphQuery, GraphSink, GraphStats, NodeFilter, ProjectReader,
    ProjectWriter, RuleConfigStore, SymbolTableReader,
};
use gt_domain::model::graph::NodeSummary;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::schema::MIGRATIONS;

// The trait implementations for `SqliteStore` live in focused submodules, one per persistence facet,
// so this file stays a readable map of *what* the store does rather than *how* each facet is written.
mod graph;
mod projects;
mod rule_config;
mod symbol;

/// Upgrade tables that reference `nodes(id)` to a version with foreign keys (old databases have none).
///
/// # Why the table must be rebuilt
///
/// SQLite's `ALTER TABLE` cannot add a foreign key constraint, so the only route is:
/// create a new table -> copy the data -> drop the old one -> rename -> add indexes back.
/// The copy filters with `JOIN nodes`, which incidentally **drops dangling rows pointing at non-existent
/// nodes** — exactly the "edges/annotations/aliases left behind after a node was overwritten"; keeping them
/// would only make dirty data look like normal data.
///
/// `PRAGMA foreign_keys` cannot be toggled inside a transaction, so it is switched separately, outside one.
fn ensure_node_fks(conn: &Connection) -> Result<()> {
    for table in ["edges", "node_annotations", "aliases"] {
        let has_fk: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM pragma_foreign_key_list('{table}')"),
                [],
                |r| r.get(0),
            )
            .map_err(DomainError::infra)?;
        if has_fk > 0 {
            continue;
        }
        let before: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .map_err(DomainError::infra)?;

        let (create, copy, indexes) = rebuild_sql(table);
        conn.execute_batch("PRAGMA foreign_keys=OFF;")
            .map_err(DomainError::infra)?;
        conn.execute_batch(&format!(
            "BEGIN;
             {create}
             {copy}
             DROP TABLE {table};
             ALTER TABLE {table}__new RENAME TO {table};
             {indexes}
             COMMIT;"
        ))
        .map_err(DomainError::infra)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(DomainError::infra)?;

        let after: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .map_err(DomainError::infra)?;
        if before != after {
            warn!(
                "foreign-key migration: {table} dropped {} dangling records (pointing at non-existent nodes), kept {after}",
                before - after
            );
        }
    }
    Ok(())
}

/// Returns the three SQL fragments (create new table, copy data, add indexes). The new table carries `REFERENCES nodes(id)`.
fn rebuild_sql(table: &str) -> (&'static str, &'static str, &'static str) {
    match table {
        "edges" => (
            "CREATE TABLE edges__new (
                id          INTEGER PRIMARY KEY,
                project_id  INTEGER NOT NULL,
                kind        TEXT NOT NULL,
                from_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                to_id       INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                phase       TEXT NOT NULL DEFAULT '',
                confidence  REAL NOT NULL DEFAULT 1.0,
                properties  TEXT,
                UNIQUE(project_id, kind, from_id, to_id)
             );",
            "INSERT INTO edges__new (id, project_id, kind, from_id, to_id, phase, confidence, properties)
             SELECT e.id, e.project_id, e.kind, e.from_id, e.to_id, e.phase, e.confidence, e.properties
             FROM edges e
             JOIN nodes nf ON nf.id = e.from_id
             JOIN nodes nt ON nt.id = e.to_id;",
            "CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_id);
             CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_id);
             CREATE INDEX IF NOT EXISTS idx_edges_project ON edges(project_id);
             CREATE INDEX IF NOT EXISTS idx_edges_proj_to ON edges(project_id, to_id);
             CREATE INDEX IF NOT EXISTS idx_edges_proj_from ON edges(project_id, from_id);",
        ),
        "node_annotations" => (
            "CREATE TABLE node_annotations__new (
                id          INTEGER PRIMARY KEY,
                node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                channel     TEXT NOT NULL,
                kind        TEXT NOT NULL,
                subkind     TEXT,
                confidence  REAL NOT NULL DEFAULT 1.0,
                evidence    TEXT,
                phase       TEXT NOT NULL DEFAULT ''
             );",
            "INSERT INTO node_annotations__new (id, node_id, channel, kind, subkind, confidence, evidence, phase)
             SELECT a.id, a.node_id, a.channel, a.kind, a.subkind, a.confidence, a.evidence, a.phase
             FROM node_annotations a
             JOIN nodes n ON n.id = a.node_id;",
            "CREATE INDEX IF NOT EXISTS idx_annotations_node ON node_annotations(node_id);
             CREATE INDEX IF NOT EXISTS idx_annotations_kind ON node_annotations(kind);",
        ),
        _ => (
            "CREATE TABLE aliases__new (
                id          INTEGER PRIMARY KEY,
                project_id  INTEGER NOT NULL,
                namespace   TEXT NOT NULL,
                key         TEXT NOT NULL,
                qualifier   TEXT NOT NULL DEFAULT '',
                node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                confidence  REAL NOT NULL DEFAULT 1.0,
                evidence    TEXT,
                UNIQUE(project_id, namespace, key, qualifier)
             );",
            "INSERT INTO aliases__new (id, project_id, namespace, key, qualifier, node_id, confidence, evidence)
             SELECT a.id, a.project_id, a.namespace, a.key, a.qualifier, a.node_id, a.confidence, a.evidence
             FROM aliases a
             JOIN nodes n ON n.id = a.node_id;",
            "CREATE INDEX IF NOT EXISTS idx_alias_lookup ON aliases(project_id, namespace, key);",
        ),
    }
}

/// Add a `project_id` column to `node_annotations` and back-fill it.
///
/// The table originally had only `node_id`, so cleaning by project could only be written as
/// `node_id IN (SELECT id FROM nodes WHERE project_id=?)`; when the nodes were cleared first the sub-query
/// came back empty and the annotations could never be deleted (historical databases accumulated nearly ten
/// thousand of them). With the column added, deleting or querying by project is a single direct lookup and no
/// longer depends on "the node still exists".
fn ensure_annotation_project(conn: &Connection) -> Result<()> {
    let has_col: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('node_annotations') WHERE name = 'project_id'",
            [],
            |r| r.get(0),
        )
        .map_err(DomainError::infra)?;
    if has_col == 0 {
        conn.execute_batch(
            "ALTER TABLE node_annotations ADD COLUMN project_id INTEGER NOT NULL DEFAULT 0;",
        )
        .map_err(DomainError::infra)?;
    }
    // Back-fill: foreign keys are already in effect here (`ensure_node_fks` ran first), so every annotation can find its project.
    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_annotations WHERE project_id = 0",
            [],
            |r| r.get(0),
        )
        .map_err(DomainError::infra)?;
    if stale > 0 {
        let backfilled = conn
            .execute(
                "UPDATE node_annotations
                 SET project_id = (SELECT project_id FROM nodes WHERE id = node_id)
                 WHERE project_id = 0",
                [],
            )
            .map_err(DomainError::infra)?;
        warn!("backfilling project_id on annotations: filled in {backfilled} historical annotations");
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_annotations_project ON node_annotations(project_id);",
    )
    .map_err(DomainError::infra)?;
    Ok(())
}

/// The SQLite repository.
///
/// `Connection` is not `Sync`, so it is wrapped in a `Mutex` to satisfy the port's `Send + Sync` bound.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl SqliteStore {
    /// Collect the set of projects owning a batch of nodes (deduped).
    ///
    /// # Why this must be computed **only once, at the start of a batch query**
    ///
    /// Written as a sub-query in each chunk's SQL —
    /// `project_id IN (SELECT project_id FROM nodes WHERE id IN (<the chunk's 400 ids>))` —
    /// the chunk count grows linearly with size, so that sub-query would run **N/400 times over**; worse, SQLite
    /// rebuilds a temporary table for `IN (SELECT …)` on every execution, a cost proportional to the size of
    /// the sub-query result — measured, this degrades batch edge fetching to about **N^1.5~1.7**
    /// (16k nodes: in-edges 156ms / out-edges 131ms, 4~5 times `query_nodes`).
    ///
    /// Callers always pass ids from a single project, so the project set is resolved once here and then sent
    /// down to each chunk as a very short constant list (usually a single value).
    /// The cost is "one query in total" instead of "one sub-query per chunk".
    fn project_ids_of(&self, ids: &[NodeId]) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut out: Vec<i64> = Vec::new();
        // 900 is a conservative value below SQLite's default variable limit (999 by default, leaving some headroom)
        for chunk in ids.chunks(900) {
            let ph = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT DISTINCT project_id FROM nodes WHERE id IN ({ph})");
            let mut stmt = conn.prepare(&sql).map_err(DomainError::infra)?;
            let rows = stmt
                .query_map(
                    rusqlite::params_from_iter(chunk.iter().map(|n| n.get())),
                    |r| r.get::<_, i64>(0),
                )
                .map_err(DomainError::infra)?;
            for p in rows {
                let p = p.map_err(DomainError::infra)?;
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        Ok(out)
    }

    /// Open (or create) the database and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(DomainError::infra)?;
        }
        let conn = Connection::open(&path).map_err(DomainError::infra)?;
        // busy_timeout: graph building can run concurrently (an HTTP background thread + the CLI), and the CLI and
        // the server are two processes sharing one SQLite file. Without it a write conflict returns SQLITE_BUSY
        // immediately instead of waiting for the lock.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )
        .map_err(DomainError::infra)?;
        for sql in MIGRATIONS {
            conn.execute_batch(sql).map_err(DomainError::infra)?;
        }
        // Old-database compatibility: the diagnostics table may exist from before the `sub_project_id` column;
        // use pragma_table_info to detect the missing column and ALTER it in if needed (idempotent).
        let has_sub_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('diagnostics') WHERE name = 'sub_project_id'",
                [],
                |r| r.get(0),
            )
            .map_err(DomainError::infra)?;
        if has_sub_col == 0 {
            conn.execute_batch("ALTER TABLE diagnostics ADD COLUMN sub_project_id INTEGER;")
                .map_err(DomainError::infra)?;
        }
        // Once the column is added, the index has to be added too (new databases create it in MIGRATIONS; this is the idempotent fallback).
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_diag_sub ON diagnostics(sub_project_id);")
            .map_err(DomainError::infra)?;
        // Add the missing foreign keys to old databases (edges / node_annotations / aliases -> nodes).
        ensure_node_fks(&conn)?;
        // Then add `project_id` to annotations: the back-fill relies on `node_id -> nodes`, so it must run after
        // the foreign keys are added (dangling records were already dropped above).
        ensure_annotation_project(&conn)?;
        info!("SQLite opened: {}", path.display());
        Ok(Self { conn: Mutex::new(conn), path })
    }

    /// An in-memory database (for tests).
    ///
    /// `foreign_keys` is enabled here too: dirty data written by a test should **fail on the spot**, not flow
    /// silently into a real database later.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(DomainError::infra)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(DomainError::infra)?;
        for sql in MIGRATIONS {
            conn.execute_batch(sql).map_err(DomainError::infra)?;
        }
        let has_sub_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('diagnostics') WHERE name = 'sub_project_id'",
                [],
                |r| r.get(0),
            )
            .map_err(DomainError::infra)?;
        if has_sub_col == 0 {
            conn.execute_batch("ALTER TABLE diagnostics ADD COLUMN sub_project_id INTEGER;")
                .map_err(DomainError::infra)?;
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_diag_sub ON diagnostics(sub_project_id);")
            .map_err(DomainError::infra)?;
        Ok(Self { conn: Mutex::new(conn), path: PathBuf::from(":memory:") })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}




/// Escape `LIKE` wildcards so `%` / `_` inside a rule id are not treated as wildcards.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// `Severity` -> the lowercase label used for storage / API (consistent with serde's `snake_case`).
fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

// ---------------------------------------------------------------- helpers

fn row_to_node(row: &rusqlite::Row) -> rusqlite::Result<Node> {
    Ok(Node {
        id: NodeId(row.get(0)?),
        project_id: ProjectId(row.get(1)?),
        sub_project_id: row.get::<_, Option<i64>>(2)?.map(SubProjectId),
        kind: NodeKind(row.get(3)?),
        name: row.get(4)?,
        fqn: row.get(5)?,
        identity: row
            .get::<_, Option<String>>(6)?
            .and_then(|s| identity_from_key(&s)),
        file_id: row.get::<_, Option<i64>>(7)?.map(FileId),
        span: Span {
            start_line: row.get(8)?,
            end_line: row.get(9)?,
            start_byte: row.get(10)?,
            end_byte: row.get(11)?,
        },
        language: Language::new(row.get::<_, String>(12)?),
        phase: Phase(row.get(13)?),
        confidence: row.get::<_, f64>(14)? as f32,
        properties: row
            .get::<_, Option<String>>(15)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null),
    })
}

fn identity_from_key(raw: &str) -> Option<gt_domain::model::IdentityKey> {
    // New format: full JSON (including `scope`).
    if let Ok(k) = serde_json::from_str::<gt_domain::model::IdentityKey>(raw) {
        return Some(k);
    }
    // Old-format compatibility: flat `kind:value` (or a degraded handling of `kind:scope:value`).
    match raw.split_once(':') {
        Some((kind, value)) => Some(gt_domain::model::IdentityKey {
            kind: gt_domain::model::SynthesizedKind(kind.to_string()),
            value: value.to_string(),
            scope: None,
        }),
        None => Some(gt_domain::model::IdentityKey::named(raw)),
    }
}

fn row_to_edge(row: &rusqlite::Row) -> rusqlite::Result<Edge> {
    Ok(Edge {
        id: EdgeId(row.get(0)?),
        project_id: ProjectId(row.get(1)?),
        kind: EdgeKind(row.get(2)?),
        from_id: NodeId(row.get(3)?),
        to_id: NodeId(row.get(4)?),
        phase: Phase(row.get(5)?),
        confidence: row.get::<_, f64>(6)? as f32,
        properties: row
            .get::<_, Option<String>>(7)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null),
    })
}

fn parse_json<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_str(s).ok()
}

fn parse_status(s: &str) -> ProjectStatus {
    match s {
        "indexing" => ProjectStatus::Indexing,
        "ready" => ProjectStatus::Ready,
        "failed" => ProjectStatus::Failed,
        _ => ProjectStatus::Created,
    }
}

fn merge_into(base: &mut Value, patch: &Value) {
    match (base.as_object_mut(), patch.as_object()) {
        (Some(b), Some(p)) => {
            for (k, v) in p {
                match (b.get(k), v.as_object()) {
                    (Some(Value::Object(_)), Some(_)) => {
                        let mut cur = b.get(k).cloned().unwrap_or(Value::Null);
                        merge_into(&mut cur, v);
                        b.insert(k.clone(), cur);
                    }
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => {
            if !patch.is_null() {
                *base = patch.clone();
            }
        }
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use gt_domain::model::{NewAnnotation, EdgeKind, NewEdge, NewNode, NodeKind};
    use gt_domain::port::{
        DiagnosticSink, EdgeDirection, GraphQuery, GraphSink, ProjectReader, ProjectWriter,
        RuleConfigStore, SymbolTableReader,
    };

    fn store() -> SqliteStore {
        SqliteStore::in_memory().expect("in-memory store")
    }

    // ---- pure helpers ----

    /// `name_contains` is matched with `LIKE`, so `%` / `_` / `\` in a user query must be escaped — otherwise
    /// searching `a_b` silently matches `axb`. The backslash is replaced **first**, or the escapes themselves
    /// would be doubled.
    #[test]
    fn like_escape_neutralises_wildcards() {
        assert_eq!(like_escape("a_b"), "a\\_b");
        assert_eq!(like_escape("100%"), "100\\%");
        assert_eq!(like_escape("a\\b"), "a\\\\b", "backslashes must be escaped first, otherwise another layer is added");
        assert_eq!(like_escape("plain"), "plain");
    }

    /// Config patches are merged **deeply**: nested objects merge key by key, while scalars and arrays replace.
    /// A `null` patch is a no-op — that is what makes "patch with nothing" safe.
    #[test]
    fn merge_into_merges_nested_objects_but_replaces_scalars() {
        let mut base = serde_json::json!({ "a": 1, "nested": { "x": 1, "y": 2 }, "list": [1, 2] });
        merge_into(&mut base, &serde_json::json!({ "a": 9, "nested": { "y": 20, "z": 30 }, "list": [3] }));
        assert_eq!(base["a"], 9, "a scalar must be overwritten");
        assert_eq!(base["nested"]["x"], 1, "nested objects are merged key by key, keys not mentioned are kept");
        assert_eq!(base["nested"]["y"], 20);
        assert_eq!(base["nested"]["z"], 30, "a new key must be added");
        assert_eq!(base["list"], serde_json::json!([3]), "an array is replaced wholesale, never merged");

        // null is a no-op; a non-object patch replaces the base outright.
        let mut b2 = serde_json::json!({ "a": 1 });
        merge_into(&mut b2, &Value::Null);
        assert_eq!(b2, serde_json::json!({ "a": 1 }), "a null patch must be a no-op");
        merge_into(&mut b2, &serde_json::json!(5));
        assert_eq!(b2, serde_json::json!(5));
    }

    /// Two on-disk spellings coexist: the new full-JSON form (with `scope`) and the legacy flat `kind:value`.
    #[test]
    fn identity_from_key_reads_both_spellings() {
        let json = identity_from_key(r#"{"kind":"table","value":"users","scope":"db1"}"#)
            .expect("the JSON form must parse");
        assert_eq!(json.value, "users");
        assert_eq!(json.scope.as_deref(), Some("db1"), "the JSON form must preserve scope");

        let flat = identity_from_key("table:users").expect("the old format must parse");
        assert_eq!(flat.kind.as_str(), "table");
        assert_eq!(flat.value, "users");
        assert!(flat.scope.is_none(), "the old format has no scope");

        let named = identity_from_key("SomeClass").expect("a bare name must parse");
        assert_eq!(named.value, "SomeClass");
    }

    #[test]
    fn severity_label_and_status_parse_are_total() {
        assert_eq!(severity_label(Severity::Critical), "critical");
        assert_eq!(severity_label(Severity::Error), "error");
        assert_eq!(severity_label(Severity::Warning), "warning");
        assert_eq!(severity_label(Severity::Info), "info");

        assert!(matches!(parse_status("indexing"), ProjectStatus::Indexing));
        assert!(matches!(parse_status("ready"), ProjectStatus::Ready));
        assert!(matches!(parse_status("failed"), ProjectStatus::Failed));
        assert!(
            matches!(parse_status("who-knows"), ProjectStatus::Created),
            "an unknown status must degrade to Created, not error"
        );
    }

    // ---- database invariants ----

    fn node(pid: ProjectId, id: i64, name: &str) -> NewNode {
        NewNode {
            id: Some(NodeId(id)),
            ..NewNode::new(pid, NodeKind::new("Class"), name)
        }
    }

    /// Re-running the pipeline (`reset_project`) must **replace** the graph, not append — this is the contract
    /// behind the watch-driven whole-DB rebuild.
    #[test]
    fn reset_project_replaces_graph_data() {
        let s = store();
        let pid = ProjectId::new(1);
        let mut first = GraphDelta::new(pid);
        first.nodes.push(node(pid, 1, "A"));
        first.nodes.push(node(pid, 2, "B"));
        s.apply(&first).unwrap();
        assert_eq!(s.stats(pid).unwrap().nodes, 2);

        let mut second = GraphDelta::new(pid);
        second.reset_project = true;
        second.nodes.push(node(pid, 3, "C"));
        s.apply(&second).unwrap();
        let stats = s.stats(pid).unwrap();
        assert_eq!(stats.nodes, 1, "after a rebuild only the new node may remain, nothing accumulates: {stats:?}");
    }

    /// The foreign keys to `nodes` must really cascade: without them "the node is gone but the edge remains" is
    /// undetectable (a concurrent build once left 36k edges with 0 nodes, which still looked like a success).
    #[test]
    fn edges_cascade_when_their_nodes_are_deleted() {
        let s = store();
        let pid = ProjectId::new(1);
        let mut d = GraphDelta::new(pid);
        d.nodes.push(node(pid, 1, "A"));
        d.nodes.push(node(pid, 2, "B"));
        d.edges.push(NewEdge::new(pid, EdgeKind::new("Calls"), NodeId(1), NodeId(2)));
        s.apply(&d).unwrap();
        let before = s.stats(pid).unwrap();
        assert_eq!((before.nodes, before.edges), (2, 1), "{before:?}");

        // A rebuild clears the nodes; the edges must go with them.
        let mut reset = GraphDelta::new(pid);
        reset.reset_project = true;
        s.apply(&reset).unwrap();
        let after = s.stats(pid).unwrap();
        assert_eq!(after.nodes, 0);
        assert_eq!(after.edges, 0, "edges must be cascade-deleted with their node, otherwise undetectable dirty data is left behind: {after:?}");
    }

    // ---- helpers for the added coverage ----
    fn node_named(pid: ProjectId, id: i64, name: &str, kind: &str) -> NewNode {
        let mut n = node(pid, id, name);
        n.kind = NodeKind::new(kind);
        n
    }

    fn new_project(name: &str) -> NewProject {
        NewProject {
            name: name.to_string(),
            root_path: PathBuf::from(format!("/data/{name}")),
            description: Some("desc".into()),
            config: None,
        }
    }

    // ---------------------------------------------------------- project lifecycle
    #[test]
    fn project_create_and_list_roundtrip() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        assert_eq!(p.name, "alpha");
        assert_eq!(p.status, ProjectStatus::Created);
        assert_eq!(p.root_path, PathBuf::from("/data/alpha"));
        let got = s.get_project(p.id).unwrap().expect("it must exist");
        assert_eq!(got.description.as_deref(), Some("desc"));
        assert_eq!(s.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn update_project_patches_fields() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let mut cfg = ProjectConfig::default();
        cfg.table_prefixes = vec!["eb_".into()];
        let updated = s
            .update_project(
                p.id,
                ProjectPatch {
                    name: Some("beta".into()),
                    description: None,
                    root_path: None,
                    config: Some(cfg.clone()),
                },
            )
            .unwrap();
        assert_eq!(updated.name, "beta");
        assert_eq!(updated.config.table_prefixes, vec!["eb_".to_string()]);
    }

    #[test]
    fn set_project_status_transitions() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        s.set_project_status(p.id, ProjectStatus::Ready).unwrap();
        assert_eq!(s.get_project(p.id).unwrap().unwrap().status, ProjectStatus::Ready);
    }

    #[test]
    fn delete_project_cleans_graph_and_meta() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Calls"), NodeId(1), NodeId(1)));
        s.apply(&d).unwrap();
        assert_eq!(s.stats(p.id).unwrap().nodes, 1);
        s.delete_project(p.id).unwrap();
        assert!(s.get_project(p.id).unwrap().is_none());
        assert_eq!(s.stats(p.id).unwrap().nodes, 0);
    }

    #[test]
    fn sub_projects_replace_and_list() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let subs = s
            .replace_sub_projects(
                p.id,
                vec![NewSubProject {
                    project_id: p.id,
                    name: "be".into(),
                    root_path: PathBuf::from("/data/alpha/backend"),
                    language: Language::new("php"),
                    role: "backend".into(),
                    detected_by: "composer.json".into(),
                    frameworks: vec!["thinkphp".into()],
                    facts: serde_json::json!({ "app_root": "/x" }),
                }],
            )
            .unwrap();
        assert_eq!(subs.len(), 1);
        let listed = s.list_sub_projects(p.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].language.as_str(), "php");
        assert_eq!(listed[0].role, "backend");
        assert_eq!(listed[0].frameworks, vec!["thinkphp".to_string()]);
    }

    #[test]
    fn list_files_respects_sub_project_filter() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let sub = s
            .replace_sub_projects(
                p.id,
                vec![NewSubProject {
                    project_id: p.id,
                    name: "be".into(),
                    root_path: PathBuf::from("/be"),
                    language: Language::new("php"),
                    role: "backend".into(),
                    detected_by: "x".into(),
                    frameworks: vec![],
                    facts: Value::Null,
                }],
            )
            .unwrap()[0]
            .id;
        s.replace_files(
            p.id,
            vec![NewSourceFile {
                project_id: p.id,
                sub_project_id: Some(sub),
                path: "a.php".into(),
                language: Language::new("php"),
                size_bytes: 10,
                content_hash: "h".into(),
            }],
        )
        .unwrap();
        assert_eq!(s.list_files(p.id, None).unwrap().len(), 1);
        assert_eq!(s.list_files(p.id, Some(sub)).unwrap().len(), 1);
        assert_eq!(s.list_files(p.id, Some(SubProjectId(999))).unwrap().len(), 0);
    }

    // ---------------------------------------------------------- graph queries
    #[test]
    fn query_nodes_filters_by_kind_and_name() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node_named(p.id, 1, "User", "Class"));
        d.nodes.push(node_named(p.id, 2, "OrderController", "Class"));
        d.nodes.push(node_named(p.id, 3, "Payment", "Service"));
        s.apply(&d).unwrap();

        assert_eq!(s.query_nodes(&NodeFilter { project_id: p.id, kind: Some(NodeKind::new("Service")), ..Default::default() }).unwrap().len(), 1);
        let by_name = s.query_nodes(&NodeFilter { project_id: p.id, name_contains: Some("Controller".into()), ..Default::default() }).unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "OrderController");
        assert_eq!(s.query_nodes(&NodeFilter { project_id: p.id, ..Default::default() }).unwrap().len(), 3);
    }

    #[test]
    fn get_node_and_get_nodes_batch() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 10, "A"));
        d.nodes.push(node(p.id, 20, "B"));
        s.apply(&d).unwrap();
        assert_eq!(s.get_node(NodeId(10)).unwrap().unwrap().name, "A");
        assert!(s.get_node(NodeId(999)).unwrap().is_none());
        let many = s.get_nodes(&[NodeId(10), NodeId(20), NodeId(999)]).unwrap();
        assert_eq!(many.len(), 2);
        assert!(many.contains_key(&10));
        assert!(many.contains_key(&20));
    }

    #[test]
    fn nodes_summary_shape() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.nodes.push(node(p.id, 2, "B"));
        s.apply(&d).unwrap();
        let sum = s.nodes_summary(p.id).unwrap();
        assert_eq!(sum.len(), 2);
        assert_eq!(sum[&1].name, "A");
        assert_eq!(sum[&2].name, "B");
    }

    /// `edges_of` must filter by the project owning the endpoint node, so a "leftover" edge recorded under a
    /// different project (the historical dirty-data case) never leaks into the current project's views.
    #[test]
    fn edges_of_filters_by_endpoint_project_and_direction() {
        let s = store();
        let pa = s.create_project(new_project("a")).unwrap();
        let pb = s.create_project(new_project("b")).unwrap();
        let mut da = GraphDelta::new(pa.id);
        da.nodes.push(node(pa.id, 1, "A1"));
        da.nodes.push(node(pa.id, 2, "A2"));
        s.apply(&da).unwrap();
        let mut db = GraphDelta::new(pb.id);
        db.nodes.push(node(pb.id, 3, "B1"));
        s.apply(&db).unwrap();
        // a clean edge: project A, A1 -> A2
        let mut e = GraphDelta::new(pa.id);
        e.edges.push(NewEdge::new(pa.id, EdgeKind::new("Calls"), NodeId(1), NodeId(2)));
        // dirty edge: references A1 but recorded under project B
        let mut dirty = GraphDelta::new(pb.id);
        dirty.edges.push(NewEdge::new(pb.id, EdgeKind::new("Calls"), NodeId(1), NodeId(3)));
        s.apply(&e).unwrap();
        s.apply(&dirty).unwrap();

        let out = s.edges_of(NodeId(1), EdgeDirection::Outgoing).unwrap();
        assert_eq!(out.len(), 1, "only the edges of project A must be returned: {out:?}");
        assert_eq!(out[0].to_id, NodeId(2));
        assert_eq!(s.edges_of(NodeId(2), EdgeDirection::Incoming).unwrap().len(), 1);
        assert_eq!(s.edges_of(NodeId(1), EdgeDirection::Both).unwrap().len(), 1);
    }

    #[test]
    fn edges_outgoing_incoming_batch_map() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.nodes.push(node(p.id, 2, "B"));
        d.nodes.push(node(p.id, 3, "C"));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Calls"), NodeId(1), NodeId(2)));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Calls"), NodeId(1), NodeId(3)));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Calls"), NodeId(2), NodeId(3)));
        s.apply(&d).unwrap();
        let out = s.edges_outgoing(&[NodeId(1), NodeId(2)]).unwrap();
        assert_eq!(out[&1].len(), 2);
        assert_eq!(out[&2].len(), 1);
        assert_eq!(s.edges_incoming(&[NodeId(3)]).unwrap()[&3].len(), 2);
    }

    #[test]
    fn kinds_and_annotations_are_queryable() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.nodes.push(node(p.id, 2, "B"));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Calls"), NodeId(1), NodeId(2)));
        d.annotations.push(NewAnnotation {
            node_id: NodeId(1),
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            kind: "Pii".into(),
            subkind: None,
            confidence: 0.9,
            evidence: Value::Null,
            phase: Phase(Phase::CF_AST.to_string()),
            merge: Default::default(),
        });
        s.apply(&d).unwrap();
        assert_eq!(s.node_kinds(p.id).unwrap(), vec!["Class".to_string()]);
        assert_eq!(s.edge_kinds(p.id).unwrap(), vec!["Calls".to_string()]);
        assert_eq!(
            s.annotation_kinds(p.id).unwrap(),
            vec![(AnnotationChannel::FKB_MARK.to_string(), "Pii".to_string())]
        );
        assert_eq!(s.annotations_of(NodeId(1)).unwrap().len(), 1);
        assert_eq!(s.annotations_of_project(p.id).unwrap()[&1].len(), 1);
    }

    #[test]
    fn find_edge_and_file_paths() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Calls"), NodeId(1), NodeId(1)));
        s.apply(&d).unwrap();
        let e = s.find_edge(EdgeId(1)).unwrap().expect("it must exist");
        assert_eq!(e.kind.as_str(), "Calls");
        assert!(s.find_edge(EdgeId(999)).unwrap().is_none());
    }

    #[test]
    fn count_nodes_supports_kind_and_side_filters() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.nodes.push(node(p.id, 2, "B"));
        s.apply(&d).unwrap();
        assert_eq!(s.count_nodes(p.id, None, None).unwrap(), 2);
        assert_eq!(s.count_nodes(p.id, Some(&NodeKind::new("Class")), None).unwrap(), 2);

        let mut d2 = GraphDelta::new(p.id);
        let mut n = node(p.id, 3, "C");
        n.properties = serde_json::json!({ "side": "frontend" });
        d2.nodes.push(n);
        s.apply(&d2).unwrap();
        assert_eq!(s.count_nodes(p.id, None, Some("frontend")).unwrap(), 1);
        assert_eq!(s.count_nodes(p.id, None, Some("backend")).unwrap(), 0);
    }

    // ---------------------------------------------------------- symbol tables
    #[test]
    fn symbol_table_roundtrip() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.symbols.push(SymbolEntry {
            project_id: p.id,
            table: "schema".into(),
            key: "users".into(),
            value: serde_json::json!({ "cols": ["id", "name"] }),
        });
        d.symbols.push(SymbolEntry {
            project_id: p.id,
            table: "schema".into(),
            key: "orders".into(),
            value: serde_json::json!({ "cols": ["id"] }),
        });
        s.apply(&d).unwrap();
        assert_eq!(
            s.get_symbol(p.id, "schema", "users").unwrap().unwrap(),
            serde_json::json!({ "cols": ["id", "name"] })
        );
        assert_eq!(s.list_symbols(p.id, "schema").unwrap().len(), 2);
    }

    // ---------------------------------------------------------- diagnostics
    #[test]
    fn diagnostics_push_list_exclude_and_clear() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |code: &str, sev: Severity| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: code.into(),
            severity: sev,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk("rule:missing", Severity::Error), mk("rule:missing", Severity::Warning), mk("rule:other", Severity::Info)]).unwrap();
        assert_eq!(s.list_diagnostics(p.id, 100).unwrap().len(), 3);
        let excluded = s.list_diagnostics_excluding(p.id, "rule:missing", 100).unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].code, "rule:other");
        let cleared = s.clear_diagnostics(p.id, "rule:missing").unwrap();
        assert_eq!(cleared, 2);
        assert_eq!(s.list_diagnostics(p.id, 100).unwrap().len(), 1);
    }

    /// Under a small `LIMIT`, the result must be **severity-first** (critical/error before info), otherwise a
    /// truncation by write order could drop the entire critical tier — the documented "996 violations, 59 critical,
    /// but LIMIT 500 left critical at 0" bug.
    #[test]
    fn diagnostics_by_code_orders_by_severity_under_limit() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |sev: Severity| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: "rule:x".into(),
            severity: sev,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk(Severity::Info), mk(Severity::Error), mk(Severity::Warning)]).unwrap();
        let limited = s.list_diagnostics_by_code(p.id, "rule:", None, 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert!(
            matches!(limited[0].severity, Severity::Error),
            "under LIMIT the highest severity must be returned first: {limited:?}"
        );
    }

    /// A `sub_project_id IS NULL` diagnostic is "shared" and must survive any sub-project filter.
    #[test]
    fn diagnostics_by_code_keeps_shared_entries() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |sub: Option<SubProjectId>| Diagnostic {
            project_id: p.id,
            sub_project_id: sub,
            phase: Phase(Phase::CF_AST.to_string()),
            code: "rule:x".into(),
            severity: Severity::Error,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk(Some(SubProjectId(1))), mk(None)]).unwrap();
        let filtered = s.list_diagnostics_by_code(p.id, "rule:", Some(&[SubProjectId(1)]), 100).unwrap();
        assert_eq!(filtered.len(), 2, "a shared diagnostic (NULL sub_project_id) must pass the filter: {filtered:?}");
    }

    #[test]
    fn diagnostics_count_by_code_and_excluding() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |code: &str, sev: Severity| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: code.into(),
            severity: sev,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk("rule:x", Severity::Critical), mk("rule:x", Severity::Info), mk("lint:z", Severity::Warning)]).unwrap();
        let counts = s.count_diagnostics_by_code(p.id, "rule:", None).unwrap();
        let map: std::collections::HashMap<&str, u64> =
            counts.iter().map(|(s, c)| (s.as_str(), *c)).collect();
        assert_eq!(map.get("critical"), Some(&1));
        assert_eq!(map.get("info"), Some(&1));
        // exclude the "rule:" prefix -> only the lint:z entry remains
        let excl = s.count_diagnostics_excluding(p.id, "rule:").unwrap();
        let excl_map: std::collections::HashMap<&str, u64> =
            excl.iter().map(|(s, c)| (s.as_str(), *c)).collect();
        assert_eq!(excl_map.get("warning"), Some(&1));
    }

    // ---------------------------------------------------------- rule config
    #[test]
    fn rule_config_set_get_delete_roundtrip() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "no-missing-link".into(),
            enabled: Some(true),
            options: serde_json::json!({ "k": "v" }),
        })
        .unwrap();
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got["no-missing-link"].enabled, Some(true));
        assert_eq!(got["no-missing-link"].options, serde_json::json!({ "k": "v" }));
        s.delete_rule_config(p.id, "no-missing-link").unwrap();
        assert!(s.get_rule_configs(p.id).unwrap().is_empty());
    }

    /// An "empty override" (enabled = None **and** options empty) means "back to inherited", so the row is deleted
    /// rather than left around as a no-op override.
    #[test]
    fn rule_config_empty_override_deletes_row() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: Some(false),
            options: serde_json::json!({ "x": 1 }),
        })
        .unwrap();
        assert_eq!(s.get_rule_configs(p.id).unwrap().len(), 1);
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: None,
            options: Value::Object(Default::default()),
        })
        .unwrap();
        assert!(
            s.get_rule_configs(p.id).unwrap().is_empty(),
            "an empty override (enabled=None with empty options) must delete the row"
        );
    }

    // ---------------------------------------------------------- apply sub-features
    #[test]
    fn apply_property_and_kind_patches() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        s.apply(&d).unwrap();

        let mut d2 = GraphDelta::new(p.id);
        d2.property_patches.push((NodeId(1), serde_json::json!({ "meta": { "a": 1 } })));
        s.apply(&d2).unwrap();
        assert_eq!(
            s.get_node(NodeId(1)).unwrap().unwrap().properties,
            serde_json::json!({ "meta": { "a": 1 } })
        );

        let mut d3 = GraphDelta::new(p.id);
        d3.kind_patches.push((NodeId(1), NodeKind::new("Middleware")));
        s.apply(&d3).unwrap();
        let n = s.get_node(NodeId(1)).unwrap().unwrap();
        assert_eq!(n.kind.as_str(), "Middleware", "kind_patches must promote the kind in place, without creating a node");
        assert_eq!(s.stats(p.id).unwrap().nodes, 1, "there must still be only one node after the promotion");
    }

    #[test]
    fn apply_empty_delta_is_a_noop() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        // No data, reset false -> is_empty() true -> early return, must not error.
        s.apply(&GraphDelta::new(p.id)).unwrap();
        assert_eq!(s.stats(p.id).unwrap().nodes, 0);
    }

    /// `chain_adjacency` buckets edges three ways:
    /// - a syntax chain edge (`Calls`) feeds `out`/`inc` (used for BFS discovery) but NOT the semantic in-edge tally;
    /// - a semantic edge (`Triggers`) feeds all three (`out`/`inc`/`sem_inc`);
    /// - a plain structural edge (`Contains`) is ignored entirely.
    /// The fetch uses light integer adjacency (no `properties` column) — this also exercises that fast path.
    #[test]
    fn chain_adjacency_buckets_chain_and_semantic_edges() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.nodes.push(node(p.id, 2, "B"));
        d.nodes.push(node(p.id, 3, "C"));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Calls"), NodeId(1), NodeId(2)));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Triggers"), NodeId(2), NodeId(3)));
        d.edges.push(NewEdge::new(p.id, EdgeKind::new("Contains"), NodeId(1), NodeId(3)));
        s.apply(&d).unwrap();

        let (out, inc, sem_inc) = s.chain_adjacency(p.id).unwrap();

        // both the syntax `Calls` and the semantic `Triggers` edge populate the call-chain adjacency
        assert_eq!(out.len(), 2, "chain out-edges must come from Calls and Triggers: {out:?}");
        assert_eq!(out[&1], vec![2]);
        assert_eq!(out[&2], vec![3]);
        assert_eq!(inc.len(), 2, "chain in-edges must come from Calls and Triggers: {inc:?}");
        assert_eq!(inc[&2], vec![1]);
        assert_eq!(inc[&3], vec![2]);

        // only the semantic edge should land in the semantic in-edge tally (`Calls` is syntax-only)
        assert_eq!(sem_inc.len(), 1, "semantic in-edges must contain only Triggers: {sem_inc:?}");
        assert_eq!(sem_inc[&3], vec![2]);

        // the plain `Contains` edge (1->3) must appear in none of the three maps: the exact-value asserts above
        // already guarantee it was dropped (no trailing 3 in out[1], no 1 in inc[3], and no key 1/2 in sem_inc).
        assert!(!sem_inc.contains_key(&1) && !sem_inc.contains_key(&2));
    }
}

// ---------------------------------------------------------------- project-level rule config

