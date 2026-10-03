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
    /// This filter used to be a sub-query written into each chunk's SQL:
    /// `project_id IN (SELECT project_id FROM nodes WHERE id IN (<the chunk's 400 ids>))`.
    /// The chunk count grows linearly with size, so that sub-query ran **N/400 times over**; worse, SQLite
    /// rebuilds a temporary table for `IN (SELECT …)` on every execution, a cost proportional to the size of
    /// the sub-query result — measured, this degraded batch edge fetching to about **N^1.5~1.7**
    /// (16k nodes: in-edges 156ms / out-edges 131ms, 4~5 times `query_nodes`).
    ///
    /// Callers always pass ids from a single project, so the project set is resolved once here and then sent
    /// down to each chunk as a very short constant list (usually a single value).
    /// The semantics are unchanged; the cost drops from "one sub-query per chunk" to "one query in total".
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

// ---------------------------------------------------------------- projects

impl ProjectReader for SqliteStore {
    fn get_project(&self, id: ProjectId) -> Result<Option<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, root_path, description, config, status, created_at, updated_at
                 FROM projects WHERE id = ?1",
            )
            .map_err(DomainError::infra)?;
        stmt.query_row(params![id.get()], |row| {
            Ok(Project {
                id: ProjectId(row.get(0)?),
                name: row.get(1)?,
                root_path: PathBuf::from(row.get::<_, String>(2)?),
                description: row.get(3)?,
                config: parse_json::<ProjectConfig>(&row.get::<_, String>(4)?).unwrap_or_default(),
                status: parse_status(&row.get::<_, String>(5)?),
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })
        .optional()
        .map_err(DomainError::infra)
    }

    fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, root_path, description, config, status, created_at, updated_at
                 FROM projects ORDER BY id",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Project {
                    id: ProjectId(row.get(0)?),
                    name: row.get(1)?,
                    root_path: PathBuf::from(row.get::<_, String>(2)?),
                    description: row.get(3)?,
                    config: parse_json::<ProjectConfig>(&row.get::<_, String>(4)?)
                        .unwrap_or_default(),
                    status: parse_status(&row.get::<_, String>(5)?),
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn list_sub_projects(&self, project_id: ProjectId) -> Result<Vec<SubProject>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, name, root_path, language, role, detected_by, frameworks, facts
                 FROM sub_projects WHERE project_id = ?1 ORDER BY id",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |row| {
                Ok(SubProject {
                    id: SubProjectId(row.get(0)?),
                    project_id: ProjectId(row.get(1)?),
                    name: row.get(2)?,
                    root_path: PathBuf::from(row.get::<_, String>(3)?),
                    language: Language::new(row.get::<_, String>(4)?),
                    role: row.get(5)?,
                    detected_by: row.get(6)?,
                    frameworks: parse_json(&row.get::<_, String>(7)?).unwrap_or_default(),
                    facts: parse_json(&row.get::<_, String>(8)?).unwrap_or(Value::Null),
                })
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn list_files(
        &self,
        project_id: ProjectId,
        sub: Option<SubProjectId>,
    ) -> Result<Vec<SourceFile>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, sub_project_id, path, language, size_bytes, content_hash
                 FROM source_files WHERE project_id = ?1
                 AND (?2 IS NULL OR sub_project_id = ?2) ORDER BY id",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(
                params![project_id.get(), sub.map(|s| s.get())],
                |row| {
                    Ok(SourceFile {
                        id: FileId(row.get(0)?),
                        project_id: ProjectId(row.get(1)?),
                        sub_project_id: row.get::<_, Option<i64>>(2)?.map(SubProjectId),
                        path: row.get(3)?,
                        language: Language::new(row.get::<_, String>(4)?),
                        size_bytes: row.get::<_, i64>(5)? as u64,
                        content_hash: row.get(6)?,
                    })
                },
            )
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }
}

impl ProjectWriter for SqliteStore {
    fn create_project(&self, new: NewProject) -> Result<Project> {
        let now = now_millis();
        let config = new.config.unwrap_or_default();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO projects (name, root_path, description, config, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                new.name,
                new.root_path.to_string_lossy(),
                new.description,
                serde_json::to_string(&config).unwrap_or_else(|_| "{}".into()),
                ProjectStatus::Created.to_string(),
                now,
            ],
        )
        .map_err(DomainError::infra)?;
        let id = ProjectId(conn.last_insert_rowid());
        drop(conn);
        self.get_project(id)?.ok_or_else(|| DomainError::NotFound("project".into()))
    }

    fn update_project(&self, id: ProjectId, patch: ProjectPatch) -> Result<Project> {
        let current = self
            .get_project(id)?
            .ok_or_else(|| DomainError::NotFound(format!("project {id}")))?;
        let name = patch.name.unwrap_or(current.name);
        let root = patch.root_path.unwrap_or(current.root_path);
        let description = patch.description.or(current.description);
        let config = patch.config.unwrap_or(current.config);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE projects SET name=?1, root_path=?2, description=?3, config=?4, updated_at=?5
             WHERE id=?6",
            params![
                name,
                root.to_string_lossy(),
                description,
                serde_json::to_string(&config).unwrap_or_else(|_| "{}".into()),
                now_millis(),
                id.get()
            ],
        )
        .map_err(DomainError::infra)?;
        drop(conn);
        self.get_project(id)?.ok_or_else(|| DomainError::NotFound(format!("project {id}")))
    }

    fn delete_project(&self, id: ProjectId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM projects WHERE id = ?1", params![id.get()])
            .map_err(DomainError::infra)?;
        // Graph data not covered by cascading foreign keys is cleaned up by hand
        for sql in [
            "DELETE FROM nodes WHERE project_id = ?1",
            "DELETE FROM edges WHERE project_id = ?1",
            "DELETE FROM aliases WHERE project_id = ?1",
            "DELETE FROM symbol_tables WHERE project_id = ?1",
            "DELETE FROM diagnostics WHERE project_id = ?1",
            "DELETE FROM pipeline_runs WHERE project_id = ?1",
            "DELETE FROM source_files WHERE project_id = ?1",
            "DELETE FROM sub_projects WHERE project_id = ?1",
        ] {
            conn.execute(sql, params![id.get()]).map_err(DomainError::infra)?;
        }
        Ok(())
    }

    fn set_project_status(&self, id: ProjectId, status: ProjectStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE projects SET status=?1, updated_at=?2 WHERE id=?3",
            params![status.to_string(), now_millis(), id.get()],
        )
        .map_err(DomainError::infra)?;
        Ok(())
    }

    fn replace_sub_projects(
        &self,
        project_id: ProjectId,
        subs: Vec<NewSubProject>,
    ) -> Result<Vec<SubProject>> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sub_projects WHERE project_id = ?1", params![project_id.get()])
            .map_err(DomainError::infra)?;
        for s in subs {
            conn.execute(
                "INSERT INTO sub_projects (project_id, name, root_path, language, role, detected_by, frameworks, facts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    project_id.get(),
                    s.name,
                    s.root_path.to_string_lossy(),
                    s.language.as_str(),
                    s.role,
                    s.detected_by,
                    serde_json::to_string(&s.frameworks).unwrap_or_else(|_| "[]".into()),
                    serde_json::to_string(&s.facts).unwrap_or_else(|_| "null".into()),
                ],
            )
            .map_err(DomainError::infra)?;
        }
        drop(conn);
        self.list_sub_projects(project_id)
    }

    fn update_sub_project_facts(&self, id: SubProjectId, facts: Value) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sub_projects SET facts = ?1 WHERE id = ?2",
            params![serde_json::to_string(&facts).unwrap_or_else(|_| "null".into()), id.get()],
        )
        .map_err(DomainError::infra)?;
        Ok(())
    }

    fn set_sub_project_frameworks(&self, id: SubProjectId, frameworks: Vec<String>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sub_projects SET frameworks = ?1 WHERE id = ?2",
            params![serde_json::to_string(&frameworks).unwrap_or_else(|_| "[]".into()), id.get()],
        )
        .map_err(DomainError::infra)?;
        Ok(())
    }

    fn replace_files(
        &self,
        project_id: ProjectId,
        files: Vec<NewSourceFile>,
    ) -> Result<Vec<SourceFile>> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM source_files WHERE project_id = ?1", params![project_id.get()])
            .map_err(DomainError::infra)?;
        let mut stmt = conn
            .prepare(
                "INSERT INTO source_files (project_id, sub_project_id, path, language, size_bytes, content_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(DomainError::infra)?;
        for f in &files {
            stmt.execute(params![
                project_id.get(),
                f.sub_project_id.map(|s| s.get()),
                f.path,
                f.language.as_str(),
                f.size_bytes as i64,
                f.content_hash,
            ])
            .map_err(DomainError::infra)?;
        }
        // After back-filling sub_project_id, the caller writes it again
        drop(stmt);
        drop(conn);
        self.list_files(project_id, None)
    }
}

// ---------------------------------------------------------------- graph

impl GraphSink for SqliteStore {
    fn apply(&self, delta: &GraphDelta) -> Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(DomainError::infra)?;

        if delta.reset_project {
            if let Some(pid) = delta.project_id {
                // Order matters: tables referencing `nodes(id)` must be deleted **before** nodes.
                //
                // These tables now all have foreign keys with `ON DELETE CASCADE` as a backstop, but deleting the
                // child tables explicitly means "whether cascading really works" is not a precondition for
                // correctness. Annotations already carry `project_id`, so there is no need to route through
                // `node_id IN (SELECT id FROM nodes …)` — that spelling degrades to "deletes nothing" once the
                // nodes have been cleared first.
                for sql in [
                    "DELETE FROM node_annotations WHERE project_id = ?1",
                    "DELETE FROM edges WHERE project_id = ?1",
                    "DELETE FROM aliases WHERE project_id = ?1",
                    "DELETE FROM symbol_tables WHERE project_id = ?1",
                    "DELETE FROM diagnostics WHERE project_id = ?1",
                    "DELETE FROM nodes WHERE project_id = ?1",
                ] {
                    tx.execute(sql, params![pid.get()]).map_err(DomainError::infra)?;
                }
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO nodes
                     (id, project_id, sub_project_id, kind, name, fqn, identity, file_id,
                      start_line, end_line, start_byte, end_byte, language, phase, confidence, properties)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                )
                .map_err(DomainError::infra)?;
            for n in &delta.nodes {
                stmt.execute(params![
                    n.id.map(|i| i.get()),
                    n.project_id.get(),
                    n.sub_project_id.map(|s| s.get()),
                    n.kind.as_str(),
                    n.name,
                    n.fqn,
                    n.identity.as_ref().map(|i| {
                        // Store the full JSON (including scope) so the scope of same-named cache keys is not lost between
                        // front end and back end; fall back to the flat key string when serialization fails (old-data compatibility).
                        serde_json::to_string(i).unwrap_or_else(|_| i.key())
                    }),
                    n.file_id.map(|f| f.get()),
                    n.span.start_line as i64,
                    n.span.end_line as i64,
                    n.span.start_byte as i64,
                    n.span.end_byte as i64,
                    n.language.as_str(),
                    n.phase.as_str(),
                    n.confidence as f64,
                    if n.properties.is_null() {
                        None
                    } else {
                        Some(n.properties.to_string())
                    },
                ])
                .map_err(DomainError::infra)?;
            }
        }

        // Property patch
        for (id, patch) in &delta.property_patches {
            let current: Option<String> = tx
                .query_row("SELECT properties FROM nodes WHERE id = ?1", params![id.get()], |r| {
                    Ok(r.get::<_, Option<String>>(0)?)
                })
                .optional()
                .map_err(DomainError::infra)?
                .flatten();
            let mut merged: Value = current
                .as_deref()
                .and_then(|c| serde_json::from_str(c).ok())
                .unwrap_or(Value::Null);
            merge_into(&mut merged, patch);
            tx.execute(
                "UPDATE nodes SET properties = ?1 WHERE id = ?2",
                params![merged.to_string(), id.get()],
            )
            .map_err(DomainError::infra)?;
        }

        // Kind promotion (syntax node -> semantic node, **no new node created**)
        for (id, kind) in &delta.kind_patches {
            tx.execute(
                "UPDATE nodes SET kind = ?1 WHERE id = ?2",
                params![kind.as_str(), id.get()],
            )
            .map_err(DomainError::infra)?;
        }

        // Append co-occurrence locations (several definitions of a synthetic node)
        for (id, loc) in &delta.location_patches {
            let current: Option<String> = tx
                .query_row("SELECT properties FROM nodes WHERE id = ?1", params![id.get()], |r| {
                    Ok(r.get::<_, Option<String>>(0)?)
                })
                .optional()
                .map_err(DomainError::infra)?
                .flatten();
            let mut merged: Value = current
                .as_deref()
                .and_then(|c| serde_json::from_str(c).ok())
                .unwrap_or(Value::Null);
            let existing = merged
                .as_object()
                .and_then(|o| o.get("locations"))
                .cloned();
            let mut arr = match existing {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            };
            if arr.len() < 50 {
                arr.push(serde_json::to_value(loc).unwrap_or(Value::Null));
            }
            if let Some(obj) = merged.as_object_mut() {
                obj.insert("locations".into(), Value::Array(arr));
            }
            tx.execute(
                "UPDATE nodes SET properties = ?1 WHERE id = ?2",
                params![merged.to_string(), id.get()],
            )
            .map_err(DomainError::infra)?;
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR IGNORE INTO edges (project_id, kind, from_id, to_id, phase, confidence, properties)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )
                .map_err(DomainError::infra)?;
            for e in &delta.edges {
                stmt.execute(params![
                    e.project_id.get(),
                    e.kind.as_str(),
                    e.from_id.get(),
                    e.to_id.get(),
                    e.phase.as_str(),
                    e.confidence as f64,
                    if e.properties.is_null() {
                        None
                    } else {
                        Some(e.properties.to_string())
                    },
                ])
                .map_err(DomainError::infra)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    // `project_id` is persisted together with the annotation; when the delta carries no project id it falls
                    // back to the owning project of the node itself (the foreign key guarantees that node is in the
                    // database, so this never yields empty).
                    "INSERT INTO node_annotations (project_id, node_id, channel, kind, subkind, confidence, evidence, phase)
                     VALUES (COALESCE(?1, (SELECT project_id FROM nodes WHERE id = ?2)), ?2,?3,?4,?5,?6,?7,?8)",
                )
                .map_err(DomainError::infra)?;
            for a in &delta.annotations {
                stmt.execute(params![
                    delta.project_id.map(|p| p.get()),
                    a.node_id.get(),
                    a.channel.as_str(),
                    a.kind,
                    a.subkind,
                    a.confidence as f64,
                    a.evidence.to_string(),
                    a.phase.as_str(),
                ])
                .map_err(DomainError::infra)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO aliases (project_id, namespace, key, qualifier, node_id, confidence, evidence)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )
                .map_err(DomainError::infra)?;
            for a in &delta.aliases {
                stmt.execute(params![
                    a.project_id.get(),
                    a.namespace,
                    a.key,
                    a.qualifier.clone().unwrap_or_default(),
                    a.node_id.get(),
                    a.confidence as f64,
                    a.evidence.to_string(),
                ])
                .map_err(DomainError::infra)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO symbol_tables (project_id, table_name, key, value)
                     VALUES (?1,?2,?3,?4)",
                )
                .map_err(DomainError::infra)?;
            for s in &delta.symbols {
                stmt.execute(params![
                    s.project_id.get(),
                    s.table,
                    s.key,
                    s.value.to_string(),
                ])
                .map_err(DomainError::infra)?;
            }
        }

        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO diagnostics (project_id, sub_project_id, phase, code, severity, message, location, payload, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                )
                .map_err(DomainError::infra)?;
            for d in &delta.diagnostics {
                stmt.execute(params![
                    d.project_id.get(),
                    d.sub_project_id.map(|s| s.get()),
                    d.phase.as_str(),
                    d.code,
                    serde_json::to_string(&d.severity).unwrap_or_else(|_| "\"info\"".into()),
                    d.message,
                    d.location,
                    d.payload.to_string(),
                    now_millis(),
                ])
                .map_err(DomainError::infra)?;
            }
        }

        tx.commit().map_err(DomainError::infra)?;
        debug!(
            "persisted: {} nodes / {} edges / {} annotations",
            delta.nodes.len(),
            delta.edges.len(),
            delta.annotations.len()
        );
        Ok(())
    }
}

impl GraphQuery for SqliteStore {
    fn query_nodes(&self, filter: &NodeFilter) -> Result<Vec<Node>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT id, project_id, sub_project_id, kind, name, fqn, identity, file_id,
                    start_line, end_line, start_byte, end_byte, language, phase, confidence, properties
             FROM nodes WHERE project_id = ?1",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(filter.project_id.get())];
        if let Some(kind) = &filter.kind {
            // Match both `kind` and `properties.category`:
            // `category` is currently identical to `kind`; this fallback is kept for compatibility with any
            // query that groups by category.
            let p = binds.len() + 1;
            sql.push_str(&format!(
                " AND (kind = ?{p} OR json_extract(properties, '$.category') = ?{p})"
            ));
            binds.push(Box::new(kind.to_string()));
        }
        if let Some(name) = &filter.name_contains {
            sql.push_str(&format!(" AND name LIKE ?{}", binds.len() + 1));
            binds.push(Box::new(format!("%{name}%")));
        }
        sql.push_str(&format!(
            " ORDER BY id LIMIT ?{} OFFSET ?{}",
            binds.len() + 1,
            binds.len() + 2
        ));
        binds.push(Box::new(filter.limit.unwrap_or(100) as i64));
        binds.push(Box::new(filter.offset.unwrap_or(0) as i64));

        let mut stmt = conn.prepare(&sql).map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())), row_to_node)
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn count_nodes(
        &self,
        project_id: ProjectId,
        kind: Option<&NodeKind>,
        sides: &[String],
    ) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from("SELECT COUNT(*) FROM nodes WHERE project_id = ?1");
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_id.get())];
        if let Some(kind) = kind {
            let p = binds.len() + 1;
            sql.push_str(&format!(
                " AND (kind = ?{p} OR json_extract(properties, '$.category') = ?{p})"
            ));
            binds.push(Box::new(kind.to_string()));
        }
        if !sides.is_empty() {
            // Set semantics instead of one scalar: a node counts when any listed party appears in its derived
            // `sides` set (current pipeline), **or** equals its scalar `side` (rows written before `sides`
            // existed — so switching a perspective to a side filter never forces a rebuild).
            let base = binds.len();
            let mut scalar_ph = Vec::with_capacity(sides.len());
            let mut set_ph = Vec::with_capacity(sides.len());
            for i in 0..sides.len() {
                scalar_ph.push(format!("?{}", base + 1 + i));
                set_ph.push(format!("?{}", base + 1 + sides.len() + i));
            }
            // `json_each` over a missing key yields no rows, so `EXISTS` degrades to false and the scalar branch
            // carries legacy rows; each placeholder gets its own bind, hence pushing the list twice.
            sql.push_str(&format!(
                " AND ( json_extract(properties, '$.side') IN ({scalar})
                        OR EXISTS (SELECT 1 FROM json_each(COALESCE(properties, '{{}}'), '$.sides')
                                   WHERE json_each.value IN ({set})) )",
                scalar = scalar_ph.join(","),
                set = set_ph.join(","),
            ));
            for s in sides {
                binds.push(Box::new(s.clone()));
            }
            for s in sides {
                binds.push(Box::new(s.clone()));
            }
        }
        conn.query_row(&sql, rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())), |r| r.get(0))
            .map(|c: i64| c as u64)
            .map_err(DomainError::infra)
    }

    fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT id, project_id, sub_project_id, kind, name, fqn, identity, file_id,
                    start_line, end_line, start_byte, end_byte, language, phase, confidence, properties
             FROM nodes WHERE id = ?1", params![id.get()], row_to_node)
            .optional()
            .map_err(DomainError::infra)
    }

    fn get_nodes(&self, ids: &[NodeId]) -> Result<HashMap<i64, Node>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut out: HashMap<i64, Node> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, project_id, sub_project_id, kind, name, fqn, identity, file_id,
                    start_line, end_line, start_byte, end_byte, language, phase, confidence, properties
                 FROM nodes WHERE id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql).map_err(DomainError::infra)?;
            let params: Vec<i64> = chunk.iter().map(|n| n.get()).collect();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), row_to_node)
                .map_err(DomainError::infra)?;
            for n in rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(DomainError::infra)?
            {
                out.insert(n.id.get(), n);
            }
        }
        Ok(out)
    }

    fn nodes_summary(&self, project_id: ProjectId) -> Result<HashMap<i64, NodeSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, name, fqn, sub_project_id FROM nodes WHERE project_id = ?1 ORDER BY id",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |row| {
                Ok(NodeSummary {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    fqn: row.get(3)?,
                    sub_project_id: row.get::<_, Option<i64>>(4)?,
                })
            })
            .map_err(DomainError::infra)?;
        let list = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)?;
        let mut out: HashMap<i64, NodeSummary> = HashMap::with_capacity(list.len());
        for s in list {
            out.insert(s.id, s);
        }
        Ok(out)
    }

    fn edges_of(&self, node: NodeId, direction: EdgeDirection) -> Result<Vec<Edge>> {
        let conn = self.conn.lock().unwrap();
        // An edge must belong to the same project as its endpoint nodes: the database may hold leftover edges from
        // historical projects (nodes cleared but edges not when a project was deleted, or an endpoint id reused by a
        // new project). Without this filter, old-project edges mix into the current project and pollute views and
        // evidence chains.
        let sql = match direction {
            EdgeDirection::Outgoing => {
                "SELECT id, project_id, kind, from_id, to_id, phase, confidence, properties FROM edges \
                 WHERE from_id = ?1 AND project_id = (SELECT project_id FROM nodes WHERE id = ?1)"
            }
            EdgeDirection::Incoming => {
                "SELECT id, project_id, kind, from_id, to_id, phase, confidence, properties FROM edges \
                 WHERE to_id = ?1 AND project_id = (SELECT project_id FROM nodes WHERE id = ?1)"
            }
            EdgeDirection::Both => {
                "SELECT id, project_id, kind, from_id, to_id, phase, confidence, properties FROM edges \
                 WHERE (from_id = ?1 OR to_id = ?1) \
                 AND project_id = (SELECT project_id FROM nodes WHERE id = ?1)"
            }
        };
        let mut stmt = conn.prepare(sql).map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![node.get()], row_to_edge)
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn edges_outgoing(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let projects = self.project_ids_of(ids)?;
        if projects.is_empty() {
            return Ok(HashMap::new());
        }
        let proj_ph = vec!["?"; projects.len()].join(",");
        let conn = self.conn.lock().unwrap();
        let mut out: HashMap<i64, Vec<Edge>> = HashMap::new();
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // Same as `edges_of`: filter by the project owning the endpoint nodes, blocking leftover edges from old projects.
            let sql = format!(
                "SELECT id, project_id, kind, from_id, to_id, phase, confidence, properties \
                 FROM edges WHERE from_id IN ({placeholders}) AND project_id IN ({proj_ph})"
            );
            let mut stmt = conn.prepare(&sql).map_err(DomainError::infra)?;
            let mut params: Vec<i64> = chunk.iter().map(|n| n.get()).collect();
            params.extend(projects.iter().copied());
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), row_to_edge)
                .map_err(DomainError::infra)?;
            for e in rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(DomainError::infra)?
            {
                out.entry(e.from_id.get()).or_default().push(e);
            }
        }
        Ok(out)
    }

    fn edges_incoming(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // The project set is computed **only once** (see the notes on `project_ids_of`).
        let projects = self.project_ids_of(ids)?;
        if projects.is_empty() {
            return Ok(HashMap::new());
        }
        let proj_ph = vec!["?"; projects.len()].join(",");
        let conn = self.conn.lock().unwrap();
        let mut inc: HashMap<i64, Vec<Edge>> = HashMap::new();
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // Same as `edges_of`: filter by the project owning the endpoint nodes, blocking leftover edges from old projects.
            let sql = format!(
                "SELECT id, project_id, kind, from_id, to_id, phase, confidence, properties \
                 FROM edges WHERE to_id IN ({placeholders}) AND project_id IN ({proj_ph})"
            );
            let mut stmt = conn.prepare(&sql).map_err(DomainError::infra)?;
            let mut params: Vec<i64> = chunk.iter().map(|n| n.get()).collect();
            params.extend(projects.iter().copied());
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), row_to_edge)
                .map_err(DomainError::infra)?;
            for e in rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(DomainError::infra)?
            {
                inc.entry(e.to_id.get()).or_default().push(e);
            }
        }
        Ok(inc)
    }

    fn chain_adjacency(
        &self,
        project_id: ProjectId,
    ) -> Result<(
        HashMap<i64, Vec<i64>>,
        HashMap<i64, Vec<i64>>,
        HashMap<i64, Vec<i64>>,
    )> {
        let conn = self.conn.lock().unwrap();
        // Take only the integer adjacency of chain edges, without heavy columns such as `properties`; fetch the whole project once and bucket in memory.
        let mut stmt = conn
            .prepare("SELECT from_id, to_id, kind FROM edges WHERE project_id = ?1")
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
            })
            .map_err(DomainError::infra)?;
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut inc: HashMap<i64, Vec<i64>> = HashMap::new();
        // In-edge adjacency of **semantic edges only**: used to count "semantic in-edges" for candidate badges
        // (syntax call edges do not count). The same query buckets as it goes, with no extra round trip (kind is
        // already in the result rows).
        let mut sem_inc: HashMap<i64, Vec<i64>> = HashMap::new();
        for r in rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)?
        {
            if gt_domain::model::kinds::is_chain_edge(&r.2) {
                out.entry(r.0).or_default().push(r.1);
                inc.entry(r.1).or_default().push(r.0);
            }
            if gt_domain::model::kinds::is_semantic_edge(&r.2) {
                sem_inc.entry(r.1).or_default().push(r.0);
            }
        }
        Ok((out, inc, sem_inc))
    }

    fn edge_kinds(&self, project_id: ProjectId) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT DISTINCT kind FROM edges WHERE project_id = ?1 ORDER BY kind")
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |r| r.get::<_, String>(0))
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn node_kinds(&self, project_id: ProjectId) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT DISTINCT kind FROM nodes WHERE project_id = ?1 ORDER BY kind")
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |r| r.get::<_, String>(0))
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn annotation_kinds(&self, project_id: ProjectId) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT channel, kind FROM node_annotations
                 WHERE project_id = ?1
                 ORDER BY channel, kind",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn annotations_of(&self, node: NodeId) -> Result<Vec<Annotation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, node_id, channel, kind, subkind, confidence, evidence, phase
                 FROM node_annotations WHERE node_id = ?1 ORDER BY id",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![node.get()], |row| {
                Ok(Annotation {
                    id: row.get(0)?,
                    node_id: NodeId(row.get(1)?),
                    channel: AnnotationChannel(row.get(2)?),
                    kind: row.get(3)?,
                    subkind: row.get(4)?,
                    confidence: row.get::<_, f64>(5)? as f32,
                    evidence: parse_json(&row.get::<_, String>(6).unwrap_or_else(|_| "null".into()))
                        .unwrap_or(Value::Null),
                    phase: Phase(row.get(7)?),
                })
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn annotations_of_project(
        &self,
        project_id: ProjectId,
    ) -> Result<std::collections::HashMap<i64, Vec<Annotation>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, node_id, channel, kind, subkind, confidence, evidence, phase
                 FROM node_annotations
                 WHERE project_id = ?1
                 ORDER BY id",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |row| {
                Ok(Annotation {
                    id: row.get(0)?,
                    node_id: NodeId(row.get(1)?),
                    channel: AnnotationChannel(row.get(2)?),
                    kind: row.get(3)?,
                    subkind: row.get(4)?,
                    confidence: row.get::<_, f64>(5)? as f32,
                    evidence: parse_json(&row.get::<_, String>(6).unwrap_or_else(|_| "null".into()))
                        .unwrap_or(Value::Null),
                    phase: Phase(row.get(7)?),
                })
            })
            .map_err(DomainError::infra)?;
        let mut map: std::collections::HashMap<i64, Vec<Annotation>> =
            std::collections::HashMap::new();
        for ann in rows {
            let ann = ann.map_err(DomainError::infra)?;
            map.entry(ann.node_id.get()).or_default().push(ann);
        }
        Ok(map)
    }

    fn file_paths(&self, project_id: ProjectId) -> Result<std::collections::HashMap<i64, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, path FROM source_files WHERE project_id = ?1")
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(DomainError::infra)?;
        let mut map = std::collections::HashMap::new();
        for r in rows {
            let (id, path) = r.map_err(DomainError::infra)?;
            map.insert(id, path);
        }
        Ok(map)
    }

    fn stats(&self, project_id: ProjectId) -> Result<GraphStats> {
        let conn = self.conn.lock().unwrap();
        let nodes: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE project_id = ?1", params![project_id.get()], |r| r.get(0))
            .map_err(DomainError::infra)?;
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges WHERE project_id = ?1", params![project_id.get()], |r| r.get(0))
            .map_err(DomainError::infra)?;
        let annotations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_annotations WHERE project_id = ?1",
                params![project_id.get()],
                |r| r.get(0),
            )
            .map_err(DomainError::infra)?;
        let mut stmt = conn
            .prepare("SELECT kind, COUNT(*) FROM nodes WHERE project_id = ?1 GROUP BY kind")
            .map_err(DomainError::infra)?;
        let by_kind = stmt
            .query_map(params![project_id.get()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })
            .map_err(DomainError::infra)?
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()
            .map_err(DomainError::infra)?;
        // Count by `properties.category` (`category` is currently identical to `kind`).
        let mut stmt_cat = conn
            .prepare(
                "SELECT json_extract(properties, '$.category'), COUNT(*) FROM nodes \
                 WHERE project_id = ?1 AND json_extract(properties, '$.category') IS NOT NULL \
                 GROUP BY json_extract(properties, '$.category')",
            )
            .map_err(DomainError::infra)?;
        let by_category = stmt_cat
            .query_map(params![project_id.get()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })
            .map_err(DomainError::infra)?
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()
            .map_err(DomainError::infra)?;
        Ok(GraphStats {
            nodes: nodes as u64,
            edges: edges as u64,
            annotations: annotations as u64,
            by_kind,
            by_category,
        })
    }

    fn find_edge(&self, id: EdgeId) -> Result<Option<Edge>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, project_id, kind, from_id, to_id, phase, confidence, properties FROM edges WHERE id = ?1",
            params![id.get()],
            row_to_edge,
        )
        .optional()
        .map_err(DomainError::infra)
    }

    fn file_path(&self, id: FileId) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT path FROM source_files WHERE id = ?1", params![id.get()], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .map_err(DomainError::infra)
    }
}

// ---------------------------------------------------------------- symbol table & diagnostics

impl SymbolTableReader for SqliteStore {
    fn get_symbol(&self, project_id: ProjectId, table: &str, key: &str) -> Result<Option<Value>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM symbol_tables WHERE project_id = ?1 AND table_name = ?2 AND key = ?3",
            params![project_id.get(), table, key],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(DomainError::infra)
        .map(|v| v.and_then(|v| serde_json::from_str(&v).ok()))
    }

    fn list_symbols(&self, project_id: ProjectId, table: &str) -> Result<Vec<SymbolEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT key, value FROM symbol_tables WHERE project_id = ?1 AND table_name = ?2 ORDER BY key",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get(), table], |r| {
                Ok(SymbolEntry {
                    project_id,
                    table: table.to_string(),
                    key: r.get(0)?,
                    value: parse_json(&r.get::<_, String>(1)?).unwrap_or(Value::Null),
                })
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }
}

impl DiagnosticSink for SqliteStore {
    fn push_diagnostics(&self, items: &[Diagnostic]) -> Result<()> {
        let delta = GraphDelta {
            project_id: items.first().map(|d| d.project_id),
            diagnostics: items.to_vec(),
            ..Default::default()
        };
        self.apply(&delta)
    }

    fn list_diagnostics(&self, project_id: ProjectId, limit: u32) -> Result<Vec<Diagnostic>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT project_id, sub_project_id, phase, code, severity, message, location, payload
                 FROM diagnostics WHERE project_id = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get(), limit as i64], |r| {
                Ok(Diagnostic {
                    project_id: ProjectId(r.get(0)?),
                    sub_project_id: r.get::<_, Option<i64>>(1)?.map(SubProjectId),
                    phase: Phase(r.get(2)?),
                    code: r.get(3)?,
                    severity: parse_json::<Severity>(
                        &r.get::<_, String>(4).unwrap_or_else(|_| "\"info\"".into()),
                    )
                    .unwrap_or(Severity::Info),
                    message: r.get(5)?,
                    location: r.get(6)?,
                    payload: parse_json(&r.get::<_, String>(7).unwrap_or_else(|_| "null".into()))
                        .unwrap_or(Value::Null),
                })
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn list_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
        limit: u32,
    ) -> Result<Vec<Diagnostic>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT project_id, sub_project_id, phase, code, severity, message, location, payload
                 FROM diagnostics
                 WHERE project_id = ?1 AND code NOT LIKE ?2 ESCAPE '\\'
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(DomainError::infra)?;
        let pattern = format!("{}%", like_escape(exclude_prefix));
        let rows = stmt
            .query_map(params![project_id.get(), pattern, limit as i64], |r| {
                Ok(Diagnostic {
                    project_id: ProjectId(r.get(0)?),
                    sub_project_id: r.get::<_, Option<i64>>(1)?.map(SubProjectId),
                    phase: Phase(r.get(2)?),
                    code: r.get(3)?,
                    severity: parse_json::<Severity>(
                        &r.get::<_, String>(4).unwrap_or_else(|_| "\"info\"".into()),
                    )
                    .unwrap_or(Severity::Info),
                    message: r.get(5)?,
                    location: r.get(6)?,
                    payload: parse_json(&r.get::<_, String>(7).unwrap_or_else(|_| "null".into()))
                        .unwrap_or(Value::Null),
                })
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn list_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
        limit: u32,
    ) -> Result<Vec<Diagnostic>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT project_id, sub_project_id, phase, code, severity, message, location, payload
             FROM diagnostics WHERE project_id = ?1 AND code LIKE ?2 ESCAPE '\\'",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(project_id.get()),
            Box::new(format!("{}%", like_escape(code_prefix))),
        ];
        if let Some(ids) = sub_project_id {
            if !ids.is_empty() {
                let placeholders: Vec<String> =
                    ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 3)).collect();
                // Shared diagnostics (sub_project_id IS NULL, e.g. cross-sub-project tables / queues) survive every
                // filter, consistent with the graph view's "shared nodes are always shown" semantics.
                sql.push_str(&format!(
                    " AND (sub_project_id IS NULL OR sub_project_id IN ({}))",
                    placeholders.join(", ")
                ));
                for s in ids {
                    binds.push(Box::new(s.get()));
                }
            }
        }
        // The ordering is deliberately **severity first, then write order**: this query carries a `LIMIT`, so once
        // the violation count exceeds the cap it is truncated — truncating by `id DESC` (write order) would cut off
        // "the rules that finished last" and might leave only info, making the whole critical / error tier vanish,
        // so the severity distribution the user sees bears no relation to the real result (measured: of 996
        // violations 59 were critical, but taking 500 left critical at 0). Truncation must evict the least severe first.
        //
        // `severity` is stored as a JSON string (e.g. `"critical"`, with quotes), so `LIKE` is used rather than
        // equality to avoid depending on the exact serialization form.
        sql.push_str(
            " ORDER BY CASE
                WHEN severity LIKE '%critical%' THEN 0
                WHEN severity LIKE '%error%' THEN 1
                WHEN severity LIKE '%warning%' THEN 2
                ELSE 3
              END, id DESC
              LIMIT ?",
        );
        sql.push_str(&format!("{}", binds.len() + 1));
        binds.push(Box::new(limit as i64));
        let mut stmt = conn.prepare(&sql).map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())), |r| {
                Ok(Diagnostic {
                    project_id: ProjectId(r.get(0)?),
                    sub_project_id: r.get::<_, Option<i64>>(1)?.map(SubProjectId),
                    phase: Phase(r.get(2)?),
                    code: r.get(3)?,
                    severity: parse_json::<Severity>(
                        &r.get::<_, String>(4).unwrap_or_else(|_| "\"info\"".into()),
                    )
                    .unwrap_or(Severity::Info),
                    message: r.get(5)?,
                    location: r.get(6)?,
                    payload: parse_json(&r.get::<_, String>(7).unwrap_or_else(|_| "null".into()))
                        .unwrap_or(Value::Null),
                })
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn clear_diagnostics(&self, project_id: ProjectId, code_prefix: &str) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "DELETE FROM diagnostics WHERE project_id = ?1 AND code LIKE ?2 ESCAPE '\\'",
                params![project_id.get(), format!("{}%", like_escape(code_prefix))],
            )
            .map_err(DomainError::infra)?;
        Ok(n as u64)
    }

    fn count_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
    ) -> Result<Vec<(String, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT severity, COUNT(*) FROM diagnostics
             WHERE project_id = ?1 AND code LIKE ?2 ESCAPE '\\'",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(project_id.get()),
            Box::new(format!("{}%", like_escape(code_prefix))),
        ];
        if let Some(ids) = sub_project_id {
            if !ids.is_empty() {
                let placeholders: Vec<String> =
                    ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 3)).collect();
                sql.push_str(&format!(
                    " AND (sub_project_id IS NULL OR sub_project_id IN ({}))",
                    placeholders.join(", ")
                ));
                for s in ids {
                    binds.push(Box::new(s.get()));
                }
            }
        }
        sql.push_str(" GROUP BY severity");
        let mut stmt = conn.prepare(&sql).map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())), |r| {
                let raw_sev: String = r.get(0)?;
                let sev = parse_json::<Severity>(&raw_sev).unwrap_or(Severity::Info);
                let n: i64 = r.get(1)?;
                Ok((severity_label(sev).to_string(), n as u64))
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
    }

    fn count_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
    ) -> Result<Vec<(String, u64)>> {
        // The four-tier rollup is folded out of **the same** per-code aggregation rather than a separate
        // `GROUP BY severity`: the "which codes to exclude" definition is allowed to exist in exactly one place,
        // otherwise the badge and the diagnostics page will sooner or later disagree.
        let mut acc: Vec<(String, u64)> = Vec::new();
        for (_code, sev, n) in self.count_diagnostics_by_code_excluding(project_id, exclude_prefix)? {
            match acc.iter_mut().find(|(s, _)| *s == sev) {
                Some((_, total)) => *total += n,
                None => acc.push((sev, n)),
            }
        }
        Ok(acc)
    }

    fn count_diagnostics_by_code_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
    ) -> Result<Vec<(String, String, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT code, severity, COUNT(*) FROM diagnostics
                 WHERE project_id = ?1 AND code NOT LIKE ?2 ESCAPE '\\'
                 GROUP BY code, severity
                 ORDER BY COUNT(*) DESC, code ASC",
            )
            .map_err(DomainError::infra)?;
        let pattern = format!("{}%", like_escape(exclude_prefix));
        let rows = stmt
            .query_map(params![project_id.get(), pattern], |r| {
                let code: String = r.get(0)?;
                let raw_sev: String = r.get(1)?;
                let sev = parse_json::<Severity>(&raw_sev).unwrap_or(Severity::Info);
                let n: i64 = r.get(2)?;
                Ok((code, severity_label(sev).to_string(), n as u64))
            })
            .map_err(DomainError::infra)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)
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

// ---------------------------------------------------------------- project-level rule config

impl RuleConfigStore for SqliteStore {
    fn get_rule_configs(
        &self,
        project_id: ProjectId,
    ) -> Result<std::collections::HashMap<String, ProjectRuleConfig>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT rule_id, enabled, options FROM project_rule_config WHERE project_id = ?1",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get()], |row| {
                let rule_id: String = row.get(0)?;
                let enabled: Option<i64> = row.get(1)?;
                let options: String = row.get(2)?;
                let options: Value =
                    serde_json::from_str(&options).unwrap_or(Value::Object(Default::default()));
                Ok((rule_id, enabled, options))
            })
            .map_err(DomainError::infra)?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (rule_id, enabled, options) = r.map_err(DomainError::infra)?;
            out.insert(
                rule_id.clone(),
                ProjectRuleConfig {
                    project_id,
                    rule_id,
                    enabled: enabled.map(|e| e != 0),
                    options,
                },
            );
        }
        Ok(out)
    }

    fn set_rule_config(&self, cfg: &ProjectRuleConfig) -> Result<()> {
        // An empty override (override not enabled and options empty) = back to the inherited state, so delete the row.
        let is_empty = cfg.enabled.is_none()
            && cfg
                .options
                .as_object()
                .map(|o| o.is_empty())
                .unwrap_or(true);
        let conn = self.conn.lock().unwrap();
        if is_empty {
            conn.execute(
                "DELETE FROM project_rule_config WHERE project_id = ?1 AND rule_id = ?2",
                params![cfg.project_id.get(), cfg.rule_id],
            )
            .map_err(DomainError::infra)?;
            return Ok(());
        }
        let enabled: Option<i64> = cfg.enabled.map(|b| if b { 1 } else { 0 });
        let options = serde_json::to_string(&cfg.options).unwrap_or_else(|_| "{}".to_string());
        conn.execute(
            "INSERT INTO project_rule_config(project_id, rule_id, enabled, options)
             VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(project_id, rule_id) DO UPDATE SET enabled = excluded.enabled, options = excluded.options",
            params![cfg.project_id.get(), cfg.rule_id, enabled, options],
        )
        .map_err(DomainError::infra)?;
        Ok(())
    }

    fn delete_rule_config(&self, project_id: ProjectId, rule_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM project_rule_config WHERE project_id = ?1 AND rule_id = ?2",
            params![project_id.get(), rule_id],
        )
        .map_err(DomainError::infra)?;
        Ok(())
    }
}
