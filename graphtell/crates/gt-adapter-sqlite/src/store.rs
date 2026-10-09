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
use gt_domain::model::graph::{
    CoverageReport, CoverageTotals, NodeSummary, SubCoverage, UncoveredCall,
};
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
    use gt_domain::model::{
        AliasEntry, EdgeKind, FileId, IdentityKey, Language, NewAnnotation, NewEdge, NewNode,
        NewSourceFile, NodeKind, SourceLocation,
    };
    use gt_domain::port::{
        DiagnosticSink, EdgeDirection, GraphQuery, GraphSink, ProjectReader, ProjectWriter,
        RuleConfigStore, SymbolTableReader,
    };
    use rusqlite::params;

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

    // ---------------------------------------------------------- sub-project fact / framework writers (previously untested)

    /// `update_sub_project_facts` rewrites the `facts` JSON of a sub-project (written by P3 to persist discovered
    /// facts); reading it back via `list_sub_projects` must reflect the new value.
    #[test]
    fn update_sub_project_facts_roundtrip() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let subs = s
            .replace_sub_projects(p.id, vec![NewSubProject {
                project_id: p.id,
                name: "be".into(),
                root_path: PathBuf::from("/be"),
                language: Language::new("php"),
                role: "backend".into(),
                detected_by: "x".into(),
                frameworks: vec![],
                facts: Value::Null,
            }])
            .unwrap();
        let sub = subs[0].id;

        s.update_sub_project_facts(sub, serde_json::json!({ "app_root": "/x", "ns": "App" }))
            .unwrap();
        let got = s.list_sub_projects(p.id).unwrap();
        assert_eq!(
            got[0].facts,
            serde_json::json!({ "app_root": "/x", "ns": "App" }),
            "facts must be updated and re-readable"
        );

        // A non-existent sub-project id must not error: the UPDATE simply matches 0 rows.
        s.update_sub_project_facts(SubProjectId(99999), Value::Null).unwrap();
    }

    /// `set_sub_project_frameworks` rewrites the `frameworks` array; reading it back must reflect the new list.
    #[test]
    fn set_sub_project_frameworks_roundtrip() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let subs = s
            .replace_sub_projects(p.id, vec![NewSubProject {
                project_id: p.id,
                name: "be".into(),
                root_path: PathBuf::from("/be"),
                language: Language::new("php"),
                role: "backend".into(),
                detected_by: "x".into(),
                frameworks: vec!["thinkphp".into()],
                facts: Value::Null,
            }])
            .unwrap();
        let sub = subs[0].id;

        s.set_sub_project_frameworks(sub, vec!["laravel".into(), "symfony".into()])
            .unwrap();
        let got = s.list_sub_projects(p.id).unwrap();
        assert_eq!(
            got[0].frameworks,
            vec!["laravel".to_string(), "symfony".to_string()],
            "frameworks must be updated and re-readable"
        );

        // A non-existent sub-project id must not error: the UPDATE simply matches 0 rows.
        s.set_sub_project_frameworks(SubProjectId(99999), vec![]).unwrap();
    }

    /// `update_project` on a project that does not exist must fail with `NotFound`, not silently upsert a row —
    /// the `ok_or_else(NotFound)` guard is the only thing standing between a typo'd id and a phantom project.
    #[test]
    fn update_project_missing_id_is_not_found() {
        let s = store();
        let err = s
            .update_project(
                ProjectId::new(424242),
                ProjectPatch {
                    name: Some("ghost".into()),
                    description: None,
                    root_path: None,
                    config: None,
                },
            )
            .expect_err("updating a non-existent project must error");
        assert!(matches!(err, DomainError::NotFound(_)), "actual error: {err:?}");
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

    /// `get_symbol` must return `None` for a missing key/table, and must stay isolated across projects — fetching a
    /// symbol that exists under another project must not leak.
    #[test]
    fn get_symbol_missing_returns_none() {
        let s = store();
        let a = s.create_project(new_project("a")).unwrap();
        let b = s.create_project(new_project("b")).unwrap();
        let mut d = GraphDelta::new(a.id);
        d.symbols.push(SymbolEntry {
            project_id: a.id,
            table: "schema".into(),
            key: "users".into(),
            value: serde_json::json!({ "cols": 1 }),
        });
        s.apply(&d).unwrap();
        assert_eq!(s.get_symbol(a.id, "schema", "users").unwrap(), Some(serde_json::json!({ "cols": 1 })));
        assert_eq!(s.get_symbol(a.id, "schema", "nope").unwrap(), None, "missing key -> None");
        assert_eq!(s.get_symbol(a.id, "other", "users").unwrap(), None, "missing table -> None");
        assert_eq!(s.get_symbol(b.id, "schema", "users").unwrap(), None, "another project must not see the symbol");
    }

    /// A `value` column holding non-JSON (e.g. corrupted externally) must yield `None` rather than error, via the
    /// `serde_json::from_str(...).ok()` fallback in `get_symbol`.
    #[test]
    fn get_symbol_corrupt_value_falls_back_to_none() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO symbol_tables (project_id, table_name, key, value) VALUES (?1, 't', 'k', 'not json at all')",
                params![p.id.get()],
            )
            .unwrap();
        assert_eq!(
            s.get_symbol(p.id, "t", "k").unwrap(),
            None,
            "corrupt JSON in the value column must yield None, not error"
        );
    }

    /// `list_symbols` on a table with no entries must return an empty vector, not error.
    #[test]
    fn list_symbols_empty_table_is_empty() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        assert!(s.list_symbols(p.id, "nope").unwrap().is_empty());
    }

    /// A non-JSON `value` in `list_symbols` must fall back to `Value::Null` per entry (the `parse_json(...).unwrap_or(Value::Null)`).
    #[test]
    fn list_symbols_corrupt_value_falls_back_to_null() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO symbol_tables (project_id, table_name, key, value) VALUES (?1, 't', 'k', 'not json')",
                params![p.id.get()],
            )
            .unwrap();
        let got = s.list_symbols(p.id, "t").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "k");
        assert_eq!(got[0].value, Value::Null, "corrupt JSON value must fall back to Value::Null");
    }

    /// `list_diagnostics` must tolerate a `NULL` `payload` column (the column is nullable) and fall back to
    /// `Value::Null` rather than erroring — via the `unwrap_or_else(|_| "null")` branch.
    #[test]
    fn list_diagnostics_tolerates_null_payload() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO diagnostics (project_id, sub_project_id, phase, code, severity, message, location, payload)
                 VALUES (?1, NULL, 'cf_ast', 'rule:x', '\"info\"', 'm', NULL, NULL)",
                params![p.id.get()],
            )
            .unwrap();
        let got = s.list_diagnostics(p.id, 100).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].payload,
            Value::Null,
            "a NULL payload column must fall back to Value::Null, not error"
        );
    }

    /// `list_diagnostics` returns newest-first (`ORDER BY id DESC`); a small `LIMIT` must keep the most recent row.
    #[test]
    fn list_diagnostics_ordered_by_id_desc() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |msg: &str| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: "rule:x".into(),
            severity: Severity::Info,
            message: msg.into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk("first"), mk("second"), mk("third")]).unwrap();
        let got = s.list_diagnostics(p.id, 100).unwrap();
        assert_eq!(got[0].message, "third");
        assert_eq!(got[2].message, "first");
        let limited = s.list_diagnostics(p.id, 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].message, "third", "LIMIT must retain the newest row (id DESC)");
    }

    /// `list_diagnostics_by_code` with an **empty** sub-project slice must NOT append an `IN ()` clause (invalid SQL);
    /// it should match every diagnostic under the code prefix.
    #[test]
    fn list_diagnostics_by_code_empty_sub_project_filter_returns_all() {
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
        s.push_diagnostics(&[mk(Some(SubProjectId(1))), mk(Some(SubProjectId(2)))]).unwrap();
        let all = s.list_diagnostics_by_code(p.id, "rule:", Some(&[]), 100).unwrap();
        assert_eq!(
            all.len(),
            2,
            "empty sub-project filter must match all (no IN clause injected): {all:?}"
        );
    }

    /// `count_diagnostics_by_code` with a non-empty sub-project filter must apply the `IN` clause and still keep the
    /// shared (NULL) diagnostic.
    #[test]
    fn count_diagnostics_by_code_filters_by_sub_project() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |sub: Option<SubProjectId>, sev: Severity| Diagnostic {
            project_id: p.id,
            sub_project_id: sub,
            phase: Phase(Phase::CF_AST.to_string()),
            code: "rule:x".into(),
            severity: sev,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[
            mk(Some(SubProjectId(1)), Severity::Error),
            mk(Some(SubProjectId(2)), Severity::Warning),
            mk(None, Severity::Info),
        ])
        .unwrap();
        let counts = s.count_diagnostics_by_code(p.id, "rule:", Some(&[SubProjectId(1)])).unwrap();
        let map: std::collections::HashMap<&str, u64> =
            counts.iter().map(|(s, c)| (s.as_str(), *c)).collect();
        assert_eq!(map.get("error"), Some(&1));
        assert_eq!(map.get("warning"), None, "the other sub-project must be excluded");
        assert_eq!(
            map.get("info"),
            Some(&1),
            "the shared (NULL sub_project_id) diagnostic must survive the filter"
        );
    }

    /// `clear_diagnostics` with a prefix that matches nothing must remove zero rows and return 0 (no error).
    #[test]
    fn clear_diagnostics_non_matching_prefix_is_noop() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |code: &str| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: code.into(),
            severity: Severity::Error,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk("rule:x")]).unwrap();
        let n = s.clear_diagnostics(p.id, "lint:").unwrap();
        assert_eq!(n, 0, "clearing a non-matching prefix must remove nothing and return 0");
        assert_eq!(s.list_diagnostics(p.id, 100).unwrap().len(), 1, "the diagnostic must remain");
    }

    /// A `severity` column holding non-JSON text must fall back to `Severity::Info` — this exercises the
    /// `parse_json::<Severity>(...).unwrap_or(Severity::Info)` arm.
    #[test]
    fn list_diagnostics_corrupt_severity_falls_back_to_info() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO diagnostics (project_id, sub_project_id, phase, code, severity, message, location, payload)
                 VALUES (?1, NULL, 'cf_ast', 'rule:x', 'not-a-json-string', 'm', NULL, NULL)",
                params![p.id.get()],
            )
            .unwrap();
        let got = s.list_diagnostics(p.id, 100).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].severity,
            Severity::Info,
            "a corrupt severity string must fall back to Info"
        );
    }

    /// `list_diagnostics_by_code` must order by severity tier (critical → error → warning → info) with a large `LIMIT`
    /// where no truncation occurs — this pins the full `ORDER BY CASE ... END, id DESC` clause, not just the LIMIT-1
    /// highest-severity case covered elsewhere.
    #[test]
    fn list_diagnostics_by_code_severity_ordering_full() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |sev: Severity| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: "rule:x".into(),
            severity: sev,
            message: severity_label(sev).into(),
            location: None,
            payload: Value::Null,
        };
        // Insert in a deliberately scrambled write order.
        s.push_diagnostics(&[
            mk(Severity::Info),
            mk(Severity::Error),
            mk(Severity::Warning),
            mk(Severity::Critical),
        ])
        .unwrap();
        let got = s.list_diagnostics_by_code(p.id, "rule:", None, 100).unwrap();
        assert_eq!(got.len(), 4);
        let order: Vec<Severity> = got.iter().map(|d| d.severity).collect();
        assert_eq!(
            order,
            vec![Severity::Critical, Severity::Error, Severity::Warning, Severity::Info],
            "severity tiers must be ordered critical→error→warning→info: {order:?}"
        );
    }

    /// `list_diagnostics_by_code` with a code prefix matching nothing must return an empty vector.
    #[test]
    fn list_diagnostics_by_code_non_matching_prefix_is_empty() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |code: &str| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: code.into(),
            severity: Severity::Error,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk("rule:x")]).unwrap();
        assert!(
            s.list_diagnostics_by_code(p.id, "lint:", None, 100).unwrap().is_empty(),
            "a non-matching code prefix must yield no diagnostics"
        );
    }

    /// `list_diagnostics_excluding` with an empty exclude prefix produces the pattern `%`, which matches every code, so
    /// the `NOT LIKE` filter drops everything and the call returns an empty vector (current behavior: an empty prefix
    /// means "exclude all").
    #[test]
    fn list_diagnostics_excluding_empty_prefix_returns_empty() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |code: &str| Diagnostic {
            project_id: p.id,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: code.into(),
            severity: Severity::Error,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk("rule:x"), mk("lint:z")]).unwrap();
        assert!(
            s.list_diagnostics_excluding(p.id, "", 100).unwrap().is_empty(),
            "empty exclude prefix matches all codes, so the result is empty"
        );
    }

    /// `count_diagnostics_excluding` must fold the per-code rows into a single severity bucket: two distinct codes of
    /// the same severity that are NOT excluded must be summed together, while the excluded code is dropped.
    #[test]
    fn count_diagnostics_excluding_merges_distinct_codes_by_severity() {
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
        s.push_diagnostics(&[
            mk("keep:a", Severity::Error),
            mk("keep:b", Severity::Error),
            mk("drop:c", Severity::Warning),
        ])
        .unwrap();
        let excl = s.count_diagnostics_excluding(p.id, "drop:").unwrap();
        let map: std::collections::HashMap<&str, u64> =
            excl.iter().map(|(st, c)| (st.as_str(), *c)).collect();
        assert_eq!(
            map.get("error"),
            Some(&2),
            "two distinct error codes must be merged into one bucket"
        );
        assert_eq!(map.get("warning"), None, "the excluded code must be dropped");
    }

    /// Diagnostics are scoped per project: pushing diagnostics into one project must not appear when listing another
    /// (both `list_diagnostics` and `list_diagnostics_by_code`).
    #[test]
    fn list_diagnostics_isolated_across_projects() {
        let s = store();
        let a = s.create_project(new_project("a")).unwrap();
        let b = s.create_project(new_project("b")).unwrap();
        let mk = |pid: ProjectId, code: &str| Diagnostic {
            project_id: pid,
            sub_project_id: None,
            phase: Phase(Phase::CF_AST.to_string()),
            code: code.into(),
            severity: Severity::Error,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[mk(a.id, "rule:x")]).unwrap();
        assert_eq!(s.list_diagnostics(a.id, 100).unwrap().len(), 1);
        assert!(
            s.list_diagnostics(b.id, 100).unwrap().is_empty(),
            "project B must not see project A's diagnostics"
        );
        assert!(s.list_diagnostics_by_code(b.id, "rule:", None, 100).unwrap().is_empty());
    }

    /// `count_diagnostics_by_code` with an **empty** sub-project slice must behave like "no filter" (no `IN ()` clause
    /// is injected), counting every diagnostic under the code prefix regardless of sub-project.
    #[test]
    fn count_diagnostics_by_code_empty_sub_project_filter_returns_all() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mk = |sub: Option<SubProjectId>, sev: Severity| Diagnostic {
            project_id: p.id,
            sub_project_id: sub,
            phase: Phase(Phase::CF_AST.to_string()),
            code: "rule:x".into(),
            severity: sev,
            message: "m".into(),
            location: None,
            payload: Value::Null,
        };
        s.push_diagnostics(&[
            mk(Some(SubProjectId(1)), Severity::Error),
            mk(Some(SubProjectId(2)), Severity::Warning),
        ])
        .unwrap();
        let counts = s.count_diagnostics_by_code(p.id, "rule:", Some(&[])).unwrap();
        let map: std::collections::HashMap<&str, u64> =
            counts.iter().map(|(st, c)| (st.as_str(), *c)).collect();
        assert_eq!(map.get("error"), Some(&1));
        assert_eq!(
            map.get("warning"),
            Some(&1),
            "empty sub-project filter must count all sub-projects"
        );
    }

    /// `list_symbols` returns entries ordered by `key` (the SQL `ORDER BY key`), so scrambled inserts come out sorted.
    #[test]
    fn list_symbols_ordered_by_key() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        for k in ["zeta", "alpha", "middle"] {
            d.symbols.push(SymbolEntry {
                project_id: p.id,
                table: "t".into(),
                key: k.into(),
                value: Value::Null,
            });
        }
        s.apply(&d).unwrap();
        let got = s.list_symbols(p.id, "t").unwrap();
        let keys: Vec<&str> = got.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "middle", "zeta"], "list_symbols must be ordered by key");
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

    // ---------------------------------------------------------- rule_config negative / branch coverage

    /// A "disable" override (`enabled = Some(false)`, no options) is a *real* override, not "back to inherited" —
    /// so it must be stored, and `enabled` must round-trip as `false` (the `e != 0` mapping). The empty-override
    /// test only covers the delete path, so this pins the keep path and the boolean mapping.
    #[test]
    fn rule_config_disabled_override_persists() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: Some(false),
            options: Value::Object(Default::default()),
        })
        .unwrap();
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(got.len(), 1, "a disabled override must be stored, not deleted");
        assert_eq!(
            got["r"].enabled,
            Some(false),
            "enabled=false must round-trip as false, not be dropped or flipped to true"
        );
    }

    /// `enabled = None` with non-empty `options` is still a real override (only options are pinned) — it must be
    /// stored, not deleted by the `is_empty` check (which requires *both* enabled-None and empty options).
    #[test]
    fn rule_config_options_only_override_persists() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: None,
            options: serde_json::json!({ "k": "v" }),
        })
        .unwrap();
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(got.len(), 1, "an options-only override must be stored, not deleted");
        assert_eq!(got["r"].enabled, None, "enabled=None must round-trip as None");
        assert_eq!(got["r"].options, serde_json::json!({ "k": "v" }));
    }

    /// Setting the same `rule_id` twice must *upsert* (ON CONFLICT DO UPDATE), not append a second row — this is
    /// what makes repeated pipeline runs replace the config instead of accumulating duplicates.
    #[test]
    fn set_rule_config_upserts_existing_row() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: Some(true),
            options: serde_json::json!({ "a": 1 }),
        })
        .unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: Some(false),
            options: serde_json::json!({ "a": 2 }),
        })
        .unwrap();
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(got.len(), 1, "the second set must upsert, not add a second row");
        assert_eq!(got["r"].enabled, Some(false));
        assert_eq!(
            got["r"].options,
            serde_json::json!({ "a": 2 }),
            "the override must reflect the latest write"
        );
    }

    /// Deleting a rule config that does not exist is a harmless no-op (the DELETE matches 0 rows) — it must not error.
    #[test]
    fn delete_rule_config_missing_is_ok() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.delete_rule_config(p.id, "no-such-rule").unwrap();
        assert!(
            s.get_rule_configs(p.id).unwrap().is_empty(),
            "deleting a non-existent rule must leave the config map empty, not error"
        );
    }

    /// `get_rule_configs` must tolerate corrupt `options` JSON in the column (the `unwrap_or` fallback) rather than
    /// erroring or panicking — the row is still returned with an empty-object options.
    #[test]
    fn get_rule_configs_tolerates_corrupt_options_json() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        // Inject a row whose `options` is not valid JSON (the writer never produces this, so it must be guarded against).
        s.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO project_rule_config(project_id, rule_id, enabled, options) VALUES (?1, ?2, ?3, ?4)",
                params![p.id.get(), "r", 1i64, "{not valid json"],
            )
            .unwrap();
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(got.len(), 1, "the row must still be returned");
        assert_eq!(got["r"].enabled, Some(true));
        assert_eq!(
            got["r"].options,
            Value::Object(Default::default()),
            "corrupt options must fall back to an empty object, not error/panic"
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

    // ---------------------------------------------------------- graph.rs untested branches & negative cases

    /// `location_patches` appends co-occurrence locations into `properties.locations` on an **existing** node, and
    /// the merge caps the array at 50 (`if arr.len() < 50`). A patch for a node that does not exist must be a silent
    /// no-op (the `UPDATE` affects 0 rows) — not an error, and it must not create a phantom properties blob.
    #[test]
    fn apply_location_patches_append_cap_and_skip_missing() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        let mut a = node(p.id, 1, "A");
        // `location_patches` only attaches when the node already carries an object `properties` (a `Null`
        // properties yields `as_object_mut() == None` and the patch is skipped), so seed it first.
        a.properties = serde_json::json!({});
        d.nodes.push(a);
        s.apply(&d).unwrap();

        let loc = |i: u32| SourceLocation {
            file: format!("f{i}.php"),
            line: i,
            symbol: None,
            note: None,
            snippet: None,
        };

        let mut one = GraphDelta::new(p.id);
        one.location_patches.push((NodeId(1), loc(1)));
        s.apply(&one).unwrap();
        let props = s.get_node(NodeId(1)).unwrap().unwrap().properties;
        assert_eq!(
            props["locations"].as_array().unwrap().len(),
            1,
            "exactly one location should be appended: {props}"
        );

        // Missing node: must not error; nothing to update.
        let mut missing = GraphDelta::new(p.id);
        missing.location_patches.push((NodeId(999), loc(999)));
        s.apply(&missing).unwrap();
        assert!(
            s.get_node(NodeId(999)).unwrap().is_none(),
            "a location patch for a non-existent node must not materialise anything"
        );

        // Boundary: 51 distinct locations must be capped to 50 (the `< 50` guard).
        let mut many = GraphDelta::new(p.id);
        for i in 1..=51 {
            many.location_patches.push((NodeId(1), loc(i)));
        }
        s.apply(&many).unwrap();
        let props = s.get_node(NodeId(1)).unwrap().unwrap().properties;
        assert_eq!(
            props["locations"].as_array().unwrap().len(),
            50,
            "locations must be capped at 50: {props}"
        );
    }

    /// `apply` writes `aliases` rows. The table's `UNIQUE(project_id, namespace, key, qualifier)` means a later
    /// `INSERT OR REPLACE` with the same composite key **overwrites** rather than errors or duplicates — asserted
    /// here against the raw table (there is no alias reader in the public API).
    #[test]
    fn apply_writes_aliases_and_overwrites_on_conflict() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let alias = |nid: i64| AliasEntry {
            project_id: p.id,
            namespace: "accessor".into(),
            key: "status_text".into(),
            qualifier: Some("app\\model\\order\\StoreOrder".into()),
            node_id: NodeId(nid),
            confidence: 1.0,
            evidence: Value::Null,
        };
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        d.aliases.push(alias(1));
        s.apply(&d).unwrap();
        let count: i64 = s
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM aliases WHERE project_id = ?1",
                params![p.id.get()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "exactly one alias row must be written");

        // Same composite key, different node -> INSERT OR REPLACE overwrites, count stays 1.
        let mut d2 = GraphDelta::new(p.id);
        d2.nodes.push(node(p.id, 2, "B"));
        d2.aliases.push(alias(2));
        s.apply(&d2).unwrap();
        let (count, node_id): (i64, i64) = s
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*), node_id FROM aliases WHERE project_id = ?1",
                params![p.id.get()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "INSERT OR REPLACE must overwrite, not duplicate");
        assert_eq!(node_id, 2, "the alias must now point at the new node");
    }

    /// `file_path` / `file_paths` read the `source_files` table populated by `replace_files`. A missing file id or a
    /// project with no files must return `None` / an empty map, never an error.
    #[test]
    fn file_paths_and_file_path_roundtrip_and_missing() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        // Negative first: nothing has been written yet.
        assert!(
            s.file_path(FileId(1)).unwrap().is_none(),
            "a missing file id must return None"
        );
        assert!(
            s.file_paths(p.id).unwrap().is_empty(),
            "a project with no files must yield an empty map"
        );

        s.replace_files(
            p.id,
            vec![NewSourceFile {
                project_id: p.id,
                sub_project_id: None,
                path: "src/main.php".into(),
                language: Language::new("php"),
                size_bytes: 10,
                content_hash: "h".into(),
            }],
        )
        .unwrap();
        let paths = s.file_paths(p.id).unwrap();
        assert_eq!(paths.len(), 1, "exactly the one written file must be present");
        let (id, path) = paths.into_iter().next().unwrap();
        assert_eq!(path, "src/main.php");
        assert_eq!(
            s.file_path(FileId(id)).unwrap().as_deref(),
            Some("src/main.php"),
            "file_path must round-trip the written path"
        );

        // Negative: a file id that was never written returns None.
        assert!(s.file_path(FileId(9999)).unwrap().is_none());
    }

    /// Annotation queries must return empty collections (not error) for nodes / projects that carry no annotations.
    #[test]
    fn annotations_of_without_any_returns_empty() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        s.apply(&d).unwrap();
        assert!(
            s.annotations_of(NodeId(1)).unwrap().is_empty(),
            "a node with no annotations must yield an empty vec"
        );
        assert!(
            s.annotations_of_project(p.id).unwrap().is_empty(),
            "a project with no annotations must yield an empty map"
        );
    }

    /// `query_nodes` with a filter that matches nothing (or a project that has no nodes) must return an empty vec,
    /// not error — the filter string is still bound and the query still runs.
    #[test]
    fn query_nodes_with_no_match_returns_empty() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "User"));
        s.apply(&d).unwrap();

        let none = s
            .query_nodes(&NodeFilter {
                project_id: p.id,
                name_contains: Some("Nope".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            none.is_empty(),
            "a non-matching filter must return an empty vec, not error: {none:?}"
        );
        let other = s
            .query_nodes(&NodeFilter {
                project_id: ProjectId::new(999),
                ..Default::default()
            })
            .unwrap();
        assert!(
            other.is_empty(),
            "a project with no nodes must return an empty vec: {other:?}"
        );
    }

    // ---- database-upgrade compatibility (the functions `open`/`in_memory` fall back to on old databases) ----

    /// `ensure_node_fks` rebuilds the old tables (no FK) into FK-bearing ones, and in doing so **drops rows that
    /// point at nodes which no longer exist** — the exact fix for "edges/annotations/aliases left behind after a
    /// node was overwritten". The rebuilt tables must both keep the valid rows and now reject orphan references.
    #[test]
    fn ensure_node_fks_adds_constraints_and_drops_dangling_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // Old schema: same columns but WITHOUT the `REFERENCES nodes` foreign keys.
        conn.execute_batch(
            "CREATE TABLE nodes (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, kind TEXT NOT NULL, name TEXT NOT NULL);
             CREATE TABLE edges (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, kind TEXT NOT NULL, from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, phase TEXT NOT NULL DEFAULT '', confidence REAL NOT NULL DEFAULT 1.0, properties TEXT);
             CREATE TABLE node_annotations (id INTEGER PRIMARY KEY, node_id INTEGER NOT NULL, channel TEXT NOT NULL, kind TEXT NOT NULL, subkind TEXT, confidence REAL NOT NULL DEFAULT 1.0, evidence TEXT, phase TEXT NOT NULL DEFAULT '');
             CREATE TABLE aliases (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, namespace TEXT NOT NULL, key TEXT NOT NULL, qualifier TEXT NOT NULL DEFAULT '', node_id INTEGER NOT NULL, confidence REAL NOT NULL DEFAULT 1.0, evidence TEXT);",
        )
        .unwrap();
        conn.execute("INSERT INTO nodes (id, project_id, kind, name) VALUES (1, 1, 'Class', 'C')", [])
            .unwrap();
        // One valid row and one dangling row per table (dangling points at the non-existent node 999).
        conn.execute("INSERT INTO edges (id, project_id, kind, from_id, to_id) VALUES (10, 1, 'Calls', 1, 1)", [])
            .unwrap();
        conn.execute("INSERT INTO edges (id, project_id, kind, from_id, to_id) VALUES (11, 1, 'Calls', 999, 999)", [])
            .unwrap();
        conn.execute("INSERT INTO node_annotations (id, node_id, channel, kind) VALUES (20, 1, 'c', 'k')", [])
            .unwrap();
        conn.execute("INSERT INTO node_annotations (id, node_id, channel, kind) VALUES (21, 999, 'c', 'k')", [])
            .unwrap();
        conn.execute("INSERT INTO aliases (id, project_id, namespace, key, node_id) VALUES (30, 1, 'ns', 'k', 1)", [])
            .unwrap();
        conn.execute("INSERT INTO aliases (id, project_id, namespace, key, node_id) VALUES (31, 1, 'ns', 'k2', 999)", [])
            .unwrap();

        ensure_node_fks(&conn).unwrap();

        let edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0)).unwrap();
        let anns: i64 = conn.query_row("SELECT COUNT(*) FROM node_annotations", [], |r| r.get(0)).unwrap();
        let aliases: i64 = conn.query_row("SELECT COUNT(*) FROM aliases", [], |r| r.get(0)).unwrap();
        assert_eq!(edges, 1, "the dangling edge must be dropped");
        assert_eq!(anns, 1, "the dangling annotation must be dropped");
        assert_eq!(aliases, 1, "the dangling alias must be dropped");

        // The rebuilt tables must now actually carry the foreign key to nodes.
        for t in ["edges", "node_annotations", "aliases"] {
            let fk: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM pragma_foreign_key_list('{t}')"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(fk > 0, "{t} must now have a foreign key to nodes");
        }
        // And the FK must now be enforced.
        let orphan = conn.execute(
            "INSERT INTO edges (id, project_id, kind, from_id, to_id) VALUES (99, 1, 'Calls', 999, 999)",
            [],
        );
        assert!(
            orphan.is_err(),
            "after rebuild the FK must reject an edge to a missing node: {orphan:?}"
        );
    }

    /// `ensure_annotation_project` adds the `project_id` column to an old `node_annotations` (which had only
    /// `node_id`), back-fills it from `nodes`, and creates the project index — otherwise per-project annotation
    /// cleanup could only be written via a sub-query that breaks once the nodes are cleared first.
    #[test]
    fn ensure_annotation_project_adds_column_and_backfills() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // Old `node_annotations`: no `project_id` column.
        conn.execute_batch(
            "CREATE TABLE nodes (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, kind TEXT NOT NULL, name TEXT NOT NULL);
             CREATE TABLE node_annotations (id INTEGER PRIMARY KEY, node_id INTEGER NOT NULL, channel TEXT NOT NULL, kind TEXT NOT NULL, subkind TEXT, confidence REAL NOT NULL DEFAULT 1.0, evidence TEXT, phase TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        conn.execute("INSERT INTO nodes (id, project_id, kind, name) VALUES (1, 7, 'Class', 'C')", [])
            .unwrap();
        conn.execute("INSERT INTO node_annotations (id, node_id, channel, kind) VALUES (20, 1, 'c', 'k')", [])
            .unwrap();

        ensure_annotation_project(&conn).unwrap();

        let pid: i64 = conn
            .query_row("SELECT project_id FROM node_annotations WHERE id = 20", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pid, 7, "project_id must be back-filled from the node");
        let has_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_annotations_project'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(has_idx > 0, "idx_annotations_project must be created");
    }

    /// `merge_into` must also handle a **non-object base**: when the base is a scalar/array, any non-null patch
    /// replaces it wholesale (the `_ =>` branch) — that is what lets a top-level scalar config be overwritten.
    #[test]
    fn merge_into_replaces_non_object_base() {
        let mut base = serde_json::json!(5);
        merge_into(&mut base, &serde_json::json!({ "a": 1 }));
        assert_eq!(base, serde_json::json!({ "a": 1 }), "a scalar base must be replaced by an object patch");

        let mut arr = serde_json::json!([1, 2]);
        merge_into(&mut arr, &serde_json::json!([3]));
        assert_eq!(arr, serde_json::json!([3]), "an array base must be replaced by an array patch");
    }

    /// `row_to_node`'s `properties` column is parsed defensively: corrupt JSON must fall back to `Value::Null`
    /// rather than erroring the whole read.
    #[test]
    fn get_node_corrupt_properties_falls_back_to_null() {
        let s = store();
        s.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO nodes (id, project_id, sub_project_id, kind, name, fqn, identity, file_id,
                                    start_line, end_line, start_byte, end_byte, language, phase, confidence, properties)
                 VALUES (1, 1, NULL, 'Class', 'C', NULL, NULL, NULL, 0, 0, 0, 0, 'rust', '', 1.0, 'not json')",
                [],
            )
            .unwrap();
        let n = s.get_node(NodeId(1)).unwrap().expect("node must be found");
        assert_eq!(
            n.properties,
            Value::Null,
            "corrupt properties JSON must fall back to Value::Null"
        );
    }

    // ---------------------------------------------------------- graph.rs apply / query branches

    /// `apply` must persist **every** node column that `row_to_node` reads back — `fqn`, `file_id`, `span`,
    /// `language`, `phase`, `confidence`, `properties`, and crucially `identity` **including `scope`**. The
    /// identity is written as full JSON (not just `key()`), so the `scope` that distinguishes same-named front/back
    /// nodes must survive a round-trip; this pins that contract.
    #[test]
    fn apply_persists_full_node_fields_and_identity_scope() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        let mut n = node(p.id, 1, "OrderService");
        n.fqn = Some("app\\services\\OrderService".into());
        n.identity = Some(IdentityKey::named_scoped("order_service", "backend"));
        n.file_id = Some(FileId(7));
        n.span = Span {
            start_line: 10,
            end_line: 20,
            start_byte: 100,
            end_byte: 300,
        };
        n.language = Language::new("php");
        n.phase = Phase("p14".into());
        n.confidence = 0.42;
        n.properties = serde_json::json!({ "category": "Service", "side": "backend" });
        d.nodes.push(n);
        s.apply(&d).unwrap();

        let got = s.get_node(NodeId(1)).unwrap().unwrap();
        assert_eq!(got.fqn.as_deref(), Some("app\\services\\OrderService"));
        assert_eq!(got.file_id, Some(FileId(7)));
        assert_eq!(got.span.start_line, 10);
        assert_eq!(got.span.end_line, 20);
        assert_eq!(got.span.start_byte, 100);
        assert_eq!(got.span.end_byte, 300);
        assert_eq!(got.language.as_str(), "php");
        assert_eq!(got.phase, Phase("p14".into()));
        assert!((got.confidence - 0.42).abs() < 1e-3, "confidence must round-trip: {}", got.confidence);
        assert_eq!(got.properties["category"], serde_json::json!("Service"));
        let id = got.identity.expect("identity must be persisted");
        assert_eq!(id.kind.as_str(), "Named");
        assert_eq!(id.value, "order_service");
        assert_eq!(id.scope.as_deref(), Some("backend"), "scope must survive the JSON round-trip");
    }

    /// `query_nodes`' `kind` filter is `kind = ? OR json_extract(properties, '$.category') = ?`. The fallback branch
    /// (a node whose `kind` column is something else but whose `properties.category` matches) is what lets callers
    /// group by category; it is exercised here with a node that has `kind=Class` but `category=Table`.
    #[test]
    fn query_nodes_kind_filter_matches_properties_category() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        let mut n = node_named(p.id, 1, "users", "Class");
        n.properties = serde_json::json!({ "category": "Table" });
        d.nodes.push(n);
        d.nodes.push(node_named(p.id, 2, "Order", "Class"));
        s.apply(&d).unwrap();

        let by_cat = s
            .query_nodes(&NodeFilter {
                project_id: p.id,
                kind: Some(NodeKind::new("Table")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_cat.len(), 1, "the json_extract category fallback must match: {by_cat:?}");
        assert_eq!(by_cat[0].name, "users");

        // The plain `kind` column path still works in the same query.
        let by_kind = s
            .query_nodes(&NodeFilter {
                project_id: p.id,
                kind: Some(NodeKind::new("Class")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_kind.len(), 2);
    }

    /// `count_nodes` shares the same `kind OR json_extract(properties, '$.category')` predicate; pin the
    /// category fallback here too (a different code path from `query_nodes`).
    #[test]
    fn count_nodes_kind_filter_matches_properties_category() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        let mut n = node_named(p.id, 1, "users", "Class");
        n.properties = serde_json::json!({ "category": "Table" });
        d.nodes.push(n);
        d.nodes.push(node_named(p.id, 2, "Order", "Class"));
        s.apply(&d).unwrap();
        assert_eq!(s.count_nodes(p.id, Some(&NodeKind::new("Table")), None).unwrap(), 1);
        assert_eq!(s.count_nodes(p.id, Some(&NodeKind::new("Class")), None).unwrap(), 2);
    }

    /// `query_nodes` appends `ORDER BY id LIMIT ? OFFSET ?` from `filter.limit` / `filter.offset`
    /// (defaulting to 100 / 0). Pagination must slice by id order.
    #[test]
    fn query_nodes_limit_and_offset_paginate() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        for i in 1..=5 {
            d.nodes.push(node_named(p.id, i, &format!("N{i}"), "Class"));
        }
        s.apply(&d).unwrap();

        let page = s
            .query_nodes(&NodeFilter {
                project_id: p.id,
                limit: Some(2),
                offset: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.len(), 2, "LIMIT 2 must return exactly two rows");
        assert_eq!(page[0].id, NodeId(2), "OFFSET 1 must skip id 1");
        assert_eq!(page[1].id, NodeId(3));

        // Default limit (100) returns everything.
        assert_eq!(
            s.query_nodes(&NodeFilter { project_id: p.id, ..Default::default() }).unwrap().len(),
            5
        );
    }

    /// `stats` computes both `by_kind` (from the `kind` column) and `by_category` (from `properties.category`).
    /// The latter is a separate SQL branch that has no assertion elsewhere; this pins that grouping.
    #[test]
    fn stats_aggregates_node_counts_by_category() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        let mut a = node_named(p.id, 1, "users", "Class");
        a.properties = serde_json::json!({ "category": "Table" });
        let mut b = node_named(p.id, 2, "Order", "Class");
        b.properties = serde_json::json!({ "category": "Table" });
        d.nodes.push(a);
        d.nodes.push(b);
        let mut c = node_named(p.id, 3, "Svc", "Service");
        c.properties = serde_json::json!({ "category": "Service" });
        d.nodes.push(c);
        s.apply(&d).unwrap();

        let stats = s.stats(p.id).unwrap();
        assert_eq!(stats.nodes, 3);
        assert_eq!(stats.by_kind.get("Class"), Some(&2));
        assert_eq!(stats.by_kind.get("Service"), Some(&1));
        // by_category groups by properties.category, folding the two Table nodes together.
        assert_eq!(
            stats.by_category.get("Table"),
            Some(&2),
            "by_category must group by properties.category: {:?}",
            stats.by_category
        );
        assert_eq!(stats.by_category.get("Service"), Some(&1));
    }

    /// `get_nodes` / `edges_outgoing` / `edges_incoming` all short-circuit on an empty id slice (the
    /// `if ids.is_empty()` guards) so they never build a degenerate `IN ()` query. And a node with no edges must
    /// yield empty maps from the batch readers rather than error.
    #[test]
    fn empty_id_collections_return_empty_maps() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        assert!(s.get_nodes(&[]).unwrap().is_empty(), "get_nodes([]) must short-circuit to empty");
        assert!(s.edges_outgoing(&[]).unwrap().is_empty(), "edges_outgoing([]) must short-circuit");
        assert!(s.edges_incoming(&[]).unwrap().is_empty(), "edges_incoming([]) must short-circuit");

        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        s.apply(&d).unwrap();
        assert!(s.edges_outgoing(&[NodeId(1)]).unwrap().is_empty());
        assert!(s.edges_incoming(&[NodeId(1)]).unwrap().is_empty());
    }

    /// `reset_project` only wipes data when `delta.project_id` is `Some` — the deletion block is guarded by
    /// `if let Some(pid) = delta.project_id`. A delta with `reset_project = true` but no `project_id` must therefore
    /// **not** delete the existing graph; this pins the guard so a future refactor cannot silently turn it into a wipe.
    #[test]
    fn reset_project_without_project_id_skips_deletion() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut first = GraphDelta::new(p.id);
        first.nodes.push(node(p.id, 1, "A"));
        s.apply(&first).unwrap();
        assert_eq!(s.stats(p.id).unwrap().nodes, 1);

        let mut reset = GraphDelta::new(p.id);
        reset.project_id = None;
        reset.reset_project = true;
        reset.nodes.push(node(p.id, 2, "B"));
        s.apply(&reset).unwrap();

        assert_eq!(
            s.stats(p.id).unwrap().nodes,
            2,
            "reset without project_id must not wipe existing nodes"
        );
    }

    /// Annotations are persisted with `COALESCE(?1, (SELECT project_id FROM nodes WHERE id = ?2))`: when the delta
    /// carries no `project_id`, the annotation must still be filed under the **owning node's** project (not dropped or
    /// mis-filed). This pins the fallback so a node's annotations survive a project-less annotation delta.
    #[test]
    fn annotation_without_delta_project_id_coalesces_node_project() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.nodes.push(node(p.id, 1, "A"));
        s.apply(&d).unwrap();

        let mut ann = GraphDelta::new(p.id);
        ann.project_id = None;
        ann.annotations.push(NewAnnotation {
            node_id: NodeId(1),
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            kind: "Pii".into(),
            subkind: None,
            confidence: 0.8,
            evidence: Value::Null,
            phase: Phase(Phase::CF_AST.to_string()),
            merge: Default::default(),
        });
        s.apply(&ann).unwrap();

        assert_eq!(s.annotations_of(NodeId(1)).unwrap().len(), 1);
        assert_eq!(
            s.annotations_of_project(p.id).unwrap()[&1].len(),
            1,
            "the annotation must be filed under the owning node's project, not lost"
        );
    }

    /// `property_patches` and `kind_patches` for a node that does not exist must be **silent no-ops** (the
    /// `UPDATE` affects 0 rows; `property_patches` reads `NULL` and merges into `Value::Null`). Neither must error nor
    /// create a phantom node.
    #[test]
    fn patches_on_missing_node_are_silent_noops() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let mut d = GraphDelta::new(p.id);
        d.property_patches.push((NodeId(999), serde_json::json!({ "x": 1 })));
        d.kind_patches.push((NodeId(999), NodeKind::new("Middleware")));
        s.apply(&d).unwrap();
        assert!(s.get_node(NodeId(999)).unwrap().is_none(), "a patch must not materialise a node");
        assert_eq!(s.stats(p.id).unwrap().nodes, 0);
    }

    /// `edges_outgoing` / `edges_incoming` restrict to the owning **project set** (computed once via
    /// `project_ids_of`), so an edge recorded under a *different* project that happens to reference this project's
    /// node ids (the historical leftover-edge case) must be excluded from the batch results.
    #[test]
    fn edges_outgoing_incoming_filter_leftover_project_edges() {
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

        // Clean edge in A: 1 -> 2. Dirty edge: references A2 but recorded under project B.
        let mut e = GraphDelta::new(pa.id);
        e.edges.push(NewEdge::new(pa.id, EdgeKind::new("Calls"), NodeId(1), NodeId(2)));
        let mut dirty = GraphDelta::new(pb.id);
        dirty.edges.push(NewEdge::new(pb.id, EdgeKind::new("Calls"), NodeId(3), NodeId(2)));
        s.apply(&e).unwrap();
        s.apply(&dirty).unwrap();

        let out = s.edges_outgoing(&[NodeId(1)]).unwrap();
        assert_eq!(out[&1].len(), 1, "batch outgoing must exclude the leftover edge from project B: {out:?}");
        assert_eq!(out[&1][0].to_id, NodeId(2));

        let inc = s.edges_incoming(&[NodeId(2)]).unwrap();
        assert_eq!(
            inc[&2].len(),
            1,
            "batch incoming must exclude the leftover edge from project B: {inc:?}"
        );
        assert_eq!(inc[&2][0].from_id, NodeId(1));
    }

    // ---------------------------------------------------------- projects.rs branch coverage

    /// `get_project` for an id that does not exist must return `None`, not error.
    #[test]
    fn get_project_missing_id_returns_none() {
        let s = store();
        assert!(s.get_project(ProjectId::new(404)).unwrap().is_none());
    }

    /// `list_projects` returns every project `ORDER BY id` — the order is by insertion id, not by name, so a
    /// name-ordered insert must still come back id-ordered.
    #[test]
    fn list_projects_orders_by_id_and_counts_all() {
        let s = store();
        s.create_project(new_project("c")).unwrap();
        s.create_project(new_project("a")).unwrap();
        s.create_project(new_project("b")).unwrap();
        let all = s.list_projects().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "c");
        assert_eq!(all[1].name, "a");
        assert_eq!(all[2].name, "b");
    }

    /// `create_project` must round-trip a non-default `config` (serialised by the writer) and a `None` description
    /// (the column is nullable and must read back as `None`, not as `"null"` / `Some("null")`).
    #[test]
    fn create_project_with_config_and_null_description_roundtrips() {
        let s = store();
        let mut cfg = ProjectConfig::default();
        cfg.table_prefixes = vec!["eb_".into(), "shop_".into()];
        let p = s
            .create_project(NewProject {
                name: "cfg".into(),
                root_path: PathBuf::from("/data/cfg"),
                description: None,
                config: Some(cfg),
            })
            .unwrap();
        let got = s.get_project(p.id).unwrap().expect("must exist");
        assert_eq!(got.description, None, "a None description must round-trip as None");
        assert_eq!(
            got.config.table_prefixes,
            vec!["eb_".to_string(), "shop_".to_string()],
            "config must round-trip through create_project"
        );
    }

    /// `update_project` only overwrites the fields present in the patch; the rest must keep their current value. The
    /// `unwrap_or(current)` arms for `description` / `root_path` / `config` are exactly this contract.
    #[test]
    fn update_project_preserves_untouched_fields() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let updated = s
            .update_project(
                p.id,
                ProjectPatch {
                    name: Some("renamed".into()),
                    description: None,
                    root_path: None,
                    config: None,
                },
            )
            .unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.description.as_deref(), Some("desc"), "untouched description must survive");
        assert_eq!(updated.root_path, PathBuf::from("/data/alpha"), "untouched root_path must survive");
        assert_eq!(
            updated.config.table_prefixes,
            ProjectConfig::default().table_prefixes,
            "untouched config must survive"
        );
    }

    /// A patch with every field `None` is a complete no-op: the project must be byte-for-byte unchanged (the
    /// `unwrap_or(current.*)` arms all hit the `current` branch).
    #[test]
    fn update_project_all_none_is_a_noop() {
        let s = store();
        let p = s.create_project(new_project("alpha")).unwrap();
        let before = s.get_project(p.id).unwrap().unwrap();
        let after = s
            .update_project(
                p.id,
                ProjectPatch {
                    name: None,
                    description: None,
                    root_path: None,
                    config: None,
                },
            )
            .unwrap();
        assert_eq!(after.name, before.name);
        assert_eq!(after.description, before.description);
        assert_eq!(after.root_path, before.root_path);
        assert_eq!(after.config.table_prefixes, before.config.table_prefixes);
        assert_eq!(after.status, before.status);
    }

    /// `get_project` parses `config` defensively (`unwrap_or_default`): a corrupt (non-JSON) `config` column — which
    /// the writer never produces, but which could exist in a hand-edited or legacy database — must yield the default
    /// config rather than error or panic.
    #[test]
    fn get_project_corrupt_config_falls_back_to_default() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE projects SET config = 'not json at all' WHERE id = ?1",
                params![p.id.get()],
            )
            .unwrap();
        let got = s.get_project(p.id).unwrap().expect("project must still be found");
        assert_eq!(
            got.config.table_prefixes,
            ProjectConfig::default().table_prefixes,
            "corrupt config JSON must fall back to default, not error"
        );
    }

    /// `list_sub_projects` is filtered by `project_id` and returns an empty vec (not error) when a project has none.
    /// A second project must not see the first project's sub-projects.
    #[test]
    fn list_sub_projects_empty_and_scoped_to_project() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        assert!(s.list_sub_projects(p.id).unwrap().is_empty());
        let other = s.create_project(new_project("b")).unwrap();
        s.replace_sub_projects(p.id, vec![NewSubProject {
            project_id: p.id,
            name: "be".into(),
            root_path: PathBuf::from("/be"),
            language: Language::new("php"),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: Value::Null,
        }])
        .unwrap();
        assert_eq!(
            s.list_sub_projects(other.id).unwrap().len(),
            0,
            "subs must be scoped to their own project"
        );
        assert_eq!(s.list_sub_projects(p.id).unwrap().len(), 1);
    }

    /// `list_files` uses `sub_project_id = ?2 OR ?2 IS NULL`. A file whose `sub_project_id` is `NULL` must appear in
    /// the **unfiltered** listing but be excluded when a concrete sub-project is requested (the `sub_project_id = ?2`
    /// branch rejects `NULL`).
    #[test]
    fn list_files_null_sub_project_only_in_unfiltered_query() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let sub = s
            .replace_sub_projects(p.id, vec![NewSubProject {
                project_id: p.id,
                name: "be".into(),
                root_path: PathBuf::from("/be"),
                language: Language::new("php"),
                role: "backend".into(),
                detected_by: "x".into(),
                frameworks: vec![],
                facts: Value::Null,
            }])
            .unwrap()[0]
            .id;
        s.replace_files(
            p.id,
            vec![
                NewSourceFile {
                    project_id: p.id,
                    sub_project_id: Some(sub),
                    path: "a.php".into(),
                    language: Language::new("php"),
                    size_bytes: 10,
                    content_hash: "h".into(),
                },
                NewSourceFile {
                    project_id: p.id,
                    sub_project_id: None,
                    path: "root.php".into(),
                    language: Language::new("php"),
                    size_bytes: 20,
                    content_hash: "h2".into(),
                },
            ],
        )
        .unwrap();
        // Unfiltered: both files, including the NULL-sub one.
        assert_eq!(s.list_files(p.id, None).unwrap().len(), 2);
        // Filtered by sub: only the sub-owned file; the NULL-sub file is excluded by `sub_project_id = ?2`.
        let only = s.list_files(p.id, Some(sub)).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].path, "a.php");
    }

    /// `replace_sub_projects` first `DELETE`s the existing rows, so replacing with an empty vec must clear them all
    /// and return an empty list (not leave stale subs behind).
    #[test]
    fn replace_sub_projects_empty_clears_all() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.replace_sub_projects(p.id, vec![NewSubProject {
            project_id: p.id,
            name: "be".into(),
            root_path: PathBuf::from("/be"),
            language: Language::new("php"),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: Value::Null,
        }])
        .unwrap();
        assert_eq!(s.list_sub_projects(p.id).unwrap().len(), 1);
        let cleared = s.replace_sub_projects(p.id, vec![]).unwrap();
        assert!(cleared.is_empty());
        assert_eq!(s.list_sub_projects(p.id).unwrap().len(), 0);
    }

    /// `replace_files` mirrors `replace_sub_projects`: a `DELETE` then re-insert, so an empty replacement clears all
    /// files (the watch-driven whole-DB refresh path depends on this).
    #[test]
    fn replace_files_empty_clears_all() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.replace_files(p.id, vec![NewSourceFile {
            project_id: p.id,
            sub_project_id: None,
            path: "a.php".into(),
            language: Language::new("php"),
            size_bytes: 10,
            content_hash: "h".into(),
        }])
        .unwrap();
        assert_eq!(s.list_files(p.id, None).unwrap().len(), 1);
        let cleared = s.replace_files(p.id, vec![]).unwrap();
        assert!(cleared.is_empty());
        assert_eq!(s.list_files(p.id, None).unwrap().len(), 0);
    }

    /// `delete_project` issues a `DELETE` that affects 0 rows when the id does not exist — it must be a harmless
    /// no-op (no error), not a "not found" failure.
    #[test]
    fn delete_project_missing_is_noop() {
        let s = store();
        s.delete_project(ProjectId::new(987654)).unwrap();
        assert!(s.list_projects().unwrap().is_empty());
    }

    /// `list_sub_projects` parses `facts` defensively (`unwrap_or(Value::Null)`): a corrupt `facts` column must yield
    /// `Value::Null` rather than error, so a hand-edited sub-project still reads.
    #[test]
    fn list_sub_projects_corrupt_facts_falls_back_to_null() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        let sub = s
            .replace_sub_projects(p.id, vec![NewSubProject {
                project_id: p.id,
                name: "be".into(),
                root_path: PathBuf::from("/be"),
                language: Language::new("php"),
                role: "backend".into(),
                detected_by: "x".into(),
                frameworks: vec![],
                facts: Value::Null,
            }])
            .unwrap()[0]
            .id;
        s.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sub_projects SET facts = 'not json' WHERE id = ?1",
                params![sub.get()],
            )
            .unwrap();
        let got = s.list_sub_projects(p.id).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].facts,
            Value::Null,
            "corrupt facts JSON must fall back to Value::Null"
        );
    }

    // ---------------------------------------------------------- rule_config.rs branch coverage

    /// `set_rule_config` treats a non-object options (here `Value::Null`) with no `enabled` as an empty override, so
    /// the `options.as_object().unwrap_or(true)` arm must treat it as "clear" — deleting any existing row rather than
    /// persisting a degenerate one. This is the defensive branch for an inherit-state / malformed write.
    #[test]
    fn set_rule_config_null_options_clears_existing() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: Some(true),
            options: serde_json::json!({ "a": 1 }),
        })
        .unwrap();
        assert_eq!(s.get_rule_configs(p.id).unwrap().len(), 1);
        // A null-options, no-enabled write must clear it (is_empty = true via the unwrap_or(true) arm).
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: None,
            options: Value::Null,
        })
        .unwrap();
        assert!(
            s.get_rule_configs(p.id).unwrap().is_empty(),
            "a null-options inherit write must delete the existing row"
        );
    }

    /// No explicit `enabled` but a **non-empty** options object is still a real override — `is_empty` requires BOTH
    /// `enabled` absent AND options empty. It must be persisted, and `enabled` must read back as `None` (column NULL).
    #[test]
    fn set_rule_config_null_enabled_with_options_persists() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: None,
            options: serde_json::json!({ "key": "val" }),
        })
        .unwrap();
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got["r"].enabled, None, "absent enabled must read back as None");
        assert_eq!(got["r"].options, serde_json::json!({ "key": "val" }));
    }

    /// An explicit `enabled` together with an empty options object is a deliberate enable, NOT an inherit — so it must
    /// be persisted (is_empty is false because `enabled` is present). This pins the `enabled.is_none()` guard so a
    /// rule that is explicitly turned on with no options is never accidentally deleted.
    #[test]
    fn set_rule_config_explicit_enable_empty_options_persists() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        s.set_rule_config(&ProjectRuleConfig {
            project_id: p.id,
            rule_id: "r".into(),
            enabled: Some(true),
            options: serde_json::json!({}),
        })
        .unwrap();
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(
            got.len(),
            1,
            "an explicit enable must not be deleted just because options is empty"
        );
        assert_eq!(got["r"].enabled, Some(true));
        assert_eq!(got["r"].options, serde_json::json!({}));
    }

    /// Several distinct `rule_id`s must all appear as separate keys in the returned HashMap — the reader accumulates
    /// one row per rule_id, not a single merged entry, and the per-entry `rule_id` field must match its key.
    #[test]
    fn get_rule_configs_returns_multiple_distinct_rules() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        for rid in ["alpha", "beta", "gamma"] {
            s.set_rule_config(&ProjectRuleConfig {
                project_id: p.id,
                rule_id: rid.into(),
                enabled: Some(true),
                options: serde_json::json!({ "id": rid }),
            })
            .unwrap();
        }
        let got = s.get_rule_configs(p.id).unwrap();
        assert_eq!(got.len(), 3, "three distinct rule_ids must yield three map entries");
        assert!(got.contains_key("alpha") && got.contains_key("beta") && got.contains_key("gamma"));
        assert_eq!(got["beta"].rule_id, "beta", "the entry's rule_id must match its map key");
    }

    /// A project that never had any rule config must yield an empty map, not error — the baseline negative case for
    /// the `SELECT ... WHERE project_id = ?` reader.
    #[test]
    fn get_rule_configs_empty_for_project_without_config() {
        let s = store();
        let p = s.create_project(new_project("a")).unwrap();
        assert!(
            s.get_rule_configs(p.id).unwrap().is_empty(),
            "a project with no rule configs must yield an empty map"
        );
    }

    /// `coverage` counts only call sites that carry a non-syntax edge (i.e. a rule fired on them).
    /// `HasCallSite` / `Calls` are pure syntax and must NOT count as "extracted".
    #[test]
    fn coverage_distinguishes_extracted_from_invisible_call_sites() {
        let s = store();
        let conn = s.conn.lock().unwrap();
        conn.execute("INSERT INTO projects (id, name, root_path) VALUES (1, 'p', '/p')", []).unwrap();
        conn.execute(
            "INSERT INTO sub_projects (id, project_id, name, root_path, language, role, detected_by, frameworks, facts) \
             VALUES (1, 1, 'app', '/app', 'php', 'backend', 'composer.json', '[\"thinkphp\"]', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_files (id, project_id, sub_project_id, path, language, size_bytes, content_hash) \
             VALUES (10, 1, 1, '/app/x.php', 'php', 0, 'h')",
            [],
        )
        .unwrap();
        // Covered call site: a semantic edge (ReadsCache) touches it.
        conn.execute(
            "INSERT INTO nodes (id, project_id, sub_project_id, kind, name, fqn, identity, file_id, \
             start_line, end_line, start_byte, end_byte, language, phase, confidence, properties) \
             VALUES (100, 1, 1, 'CallSite', 'Cache::get', NULL, NULL, 10, 10, 10, 0, 0, 'php', 'CfAst', 1.0, '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO nodes (id, project_id, sub_project_id, kind, name, fqn, identity, file_id, \
             start_line, end_line, start_byte, end_byte, language, phase, confidence, properties) \
             VALUES (200, 1, 1, 'Cache', 'token', NULL, NULL, NULL, 1, 1, 0, 0, 'php', 'Synthesize', 1.0, '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO edges (id, project_id, kind, from_id, to_id, phase, confidence, properties) \
             VALUES (1, 1, 'ReadsCache', 100, 200, 'Synthesize', 1.0, '{}')",
            [],
        )
        .unwrap();
        // Invisible call site: only a pure-syntax HasCallSite edge.
        conn.execute(
            "INSERT INTO nodes (id, project_id, sub_project_id, kind, name, fqn, identity, file_id, \
             start_line, end_line, start_byte, end_byte, language, phase, confidence, properties) \
             VALUES (101, 1, 1, 'CallSite', 'Route::get', NULL, NULL, 10, 20, 20, 0, 0, 'php', 'CfAst', 1.0, '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO nodes (id, project_id, sub_project_id, kind, name, fqn, identity, file_id, \
             start_line, end_line, start_byte, end_byte, language, phase, confidence, properties) \
             VALUES (300, 1, 1, 'Function', 'f', NULL, NULL, 10, 5, 30, 0, 0, 'php', 'CfAst', 1.0, '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO edges (id, project_id, kind, from_id, to_id, phase, confidence, properties) \
             VALUES (2, 1, 'HasCallSite', 300, 101, 'CfAst', 1.0, '{}')",
            [],
        )
        .unwrap();
        drop(conn);

        let report = s.coverage(ProjectId(1)).unwrap();
        assert_eq!(report.totals.total_calls, 2, "two call sites total");
        assert_eq!(report.totals.covered_calls, 1, "only the one with a semantic edge is covered");
        assert_eq!(report.totals.sub_projects_with_gaps, 0, "thinkphp+php carries no gap flags");
        let sub = report.sub_projects.iter().find(|x| x.sub_project_id == 1).unwrap();
        assert_eq!(sub.total_calls, 2);
        assert_eq!(sub.covered_calls, 1);
        assert!(sub.flags.is_empty(), "recognized framework + known language => no flags");
        assert!(
            sub.uncovered_samples.iter().any(|c| c.callee == "Route::get"),
            "the invisible callee should surface in the sample"
        );
    }

    /// A sub-project with an unknown language and no recognized framework must be flagged.
    #[test]
    fn coverage_flags_unknown_language_and_missing_framework() {
        let s = store();
        let conn = s.conn.lock().unwrap();
        conn.execute("INSERT INTO projects (id, name, root_path) VALUES (2, 'p', '/p')", []).unwrap();
        conn.execute(
            "INSERT INTO sub_projects (id, project_id, name, root_path, language, role, detected_by, frameworks, facts) \
             VALUES (2, 2, 'mystery', '/m', 'unknown', 'backend', 'unknown', '[]', '{}')",
            [],
        )
        .unwrap();
        // A lone call site with no edges at all.
        conn.execute(
            "INSERT INTO nodes (id, project_id, sub_project_id, kind, name, fqn, identity, file_id, \
             start_line, end_line, start_byte, end_byte, language, phase, confidence, properties) \
             VALUES (500, 2, 2, 'CallSite', 'doThing', NULL, NULL, NULL, 3, 3, 0, 0, 'unknown', 'CfAst', 1.0, '{}')",
            [],
        )
        .unwrap();
        drop(conn);

        let report = s.coverage(ProjectId(2)).unwrap();
        let sub = report.sub_projects.iter().find(|x| x.sub_project_id == 2).unwrap();
        assert!(sub.flags.contains(&"language_unknown".to_string()));
        assert!(sub.flags.contains(&"no_framework".to_string()));
    }
}

// ---------------------------------------------------------------- project-level rule config

