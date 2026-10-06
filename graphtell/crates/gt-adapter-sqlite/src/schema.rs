//! SQLite table structure.

/// All create-table statements (idempotent).
pub const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE IF NOT EXISTS projects (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL,
    root_path    TEXT NOT NULL,
    description  TEXT,
    config       TEXT NOT NULL DEFAULT '{}',
    status       TEXT NOT NULL DEFAULT 'created',
    created_at   INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL DEFAULT 0
);
"#,
    r#"
CREATE TABLE IF NOT EXISTS sub_projects (
    id           INTEGER PRIMARY KEY,
    project_id   INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    root_path    TEXT NOT NULL,
    language     TEXT NOT NULL DEFAULT 'unknown',
    role         TEXT NOT NULL DEFAULT 'unknown',
    detected_by  TEXT NOT NULL DEFAULT '',
    frameworks   TEXT NOT NULL DEFAULT '[]',
    facts        TEXT NOT NULL DEFAULT 'null'
);
CREATE INDEX IF NOT EXISTS idx_sub_projects_project ON sub_projects(project_id);
"#,
    r#"
CREATE TABLE IF NOT EXISTS source_files (
    id             INTEGER PRIMARY KEY,
    project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    sub_project_id INTEGER,
    path           TEXT NOT NULL,
    language       TEXT NOT NULL DEFAULT 'unknown',
    size_bytes     INTEGER NOT NULL DEFAULT 0,
    content_hash   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_files_project ON source_files(project_id);
CREATE INDEX IF NOT EXISTS idx_files_sub ON source_files(sub_project_id);
"#,
    r#"
CREATE TABLE IF NOT EXISTS nodes (
    id             INTEGER PRIMARY KEY,
    project_id     INTEGER NOT NULL,
    sub_project_id INTEGER,
    kind           TEXT NOT NULL,
    name           TEXT NOT NULL,
    fqn            TEXT,
    identity       TEXT,
    file_id        INTEGER,
    start_line     INTEGER NOT NULL DEFAULT 0,
    end_line       INTEGER NOT NULL DEFAULT 0,
    start_byte     INTEGER NOT NULL DEFAULT 0,
    end_byte       INTEGER NOT NULL DEFAULT 0,
    language       TEXT NOT NULL DEFAULT 'unknown',
    phase          TEXT NOT NULL DEFAULT '',
    confidence     REAL NOT NULL DEFAULT 1.0,
    properties     TEXT
);
CREATE INDEX IF NOT EXISTS idx_nodes_project ON nodes(project_id);
CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(project_id, kind);
CREATE INDEX IF NOT EXISTS idx_nodes_fqn ON nodes(project_id, fqn);
CREATE INDEX IF NOT EXISTS idx_nodes_identity ON nodes(project_id, identity);
CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_id);
"#,
    r#"
CREATE TABLE IF NOT EXISTS edges (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    -- Foreign key: points at `nodes(id)`.
    --
    -- Without it, dirty data of the form "the node is gone but the edge remains" is completely undetectable —
    -- a concurrent build once made 10 projects overwrite each other's nodes while the UI showed "36k edges /
    -- 0 nodes", which looks like a successful build (the edge unique key includes `project_id` and there is no
    -- foreign key, so inserts sail through).
    -- Old databases get the constraint back via `SqliteStore::ensure_node_fks`, which rebuilds this table.
    from_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    to_id       INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    phase       TEXT NOT NULL DEFAULT '',
    confidence  REAL NOT NULL DEFAULT 1.0,
    properties  TEXT,
    UNIQUE(project_id, kind, from_id, to_id)
);
CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_id);
CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_id);
CREATE INDEX IF NOT EXISTS idx_edges_project ON edges(project_id);
-- Composite index: **the key to batch edge-fetch performance** — do not delete.
--
-- `edges_incoming` / `edges_outgoing` queries look like
--   WHERE to_id IN (<400 ids>) AND project_id IN (?)
-- With single-column indexes only, SQLite's planner picks `idx_edges_project` —
-- i.e. **scan every edge of the project** and then filter row by row on `to_id IN (...)`,
-- cost = chunk count x project edge count = O(N^2): measured at 156ms to fetch in-edges for 16k nodes,
-- growing about N^1.5 with size (while `query_nodes` is linear).
--
-- With (project_id, to_id) the planner switches to
--   SEARCH edges USING INDEX idx_edges_proj_to (project_id=? AND to_id=?)
-- both conditions use an index — measured at 9.8ms, and 4x the size costs 3.4x the time (linear).
CREATE INDEX IF NOT EXISTS idx_edges_proj_to ON edges(project_id, to_id);
CREATE INDEX IF NOT EXISTS idx_edges_proj_from ON edges(project_id, from_id);
"#,
    r#"
CREATE TABLE IF NOT EXISTS node_annotations (
    -- A redundant copy of `project_id`: an annotation naturally belongs to a project, and resolving it through
    -- `node_id IN (SELECT id FROM nodes WHERE project_id=?)` breaks once the nodes are deleted first
    -- (re-running a build clears nodes first): that subquery is empty and the annotations can never be
    -- deleted again, leaving permanently undeletable dangling annotations.
    project_id   INTEGER NOT NULL DEFAULT 0,
    id          INTEGER PRIMARY KEY,
    node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    channel     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    subkind     TEXT,
    confidence  REAL NOT NULL DEFAULT 1.0,
    evidence    TEXT,
    phase       TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_annotations_node ON node_annotations(node_id);
CREATE INDEX IF NOT EXISTS idx_annotations_kind ON node_annotations(kind);
-- `idx_annotations_project` is not created here: old databases' `node_annotations` has no `project_id` column,
-- and while `CREATE TABLE IF NOT EXISTS` would quietly skip, the index statement errors out outright
-- (no such column), making the whole database unopenable. That index is created by
-- `SqliteStore::ensure_annotation_project` after the column is added.
"#,
    r#"
CREATE TABLE IF NOT EXISTS aliases (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL,
    namespace   TEXT NOT NULL,
    key         TEXT NOT NULL,
    qualifier   TEXT NOT NULL DEFAULT '',
    node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    confidence  REAL NOT NULL DEFAULT 1.0,
    evidence    TEXT,
    UNIQUE(project_id, namespace, key, qualifier)
);
CREATE INDEX IF NOT EXISTS idx_alias_lookup ON aliases(project_id, namespace, key);
"#,
    r#"
CREATE TABLE IF NOT EXISTS symbol_tables (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL,
    table_name  TEXT NOT NULL,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    UNIQUE(project_id, table_name, key)
);
CREATE INDEX IF NOT EXISTS idx_symbols ON symbol_tables(project_id, table_name);
"#,
    r#"
CREATE TABLE IF NOT EXISTS diagnostics (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL,
    sub_project_id INTEGER,
    phase       TEXT NOT NULL DEFAULT '',
    code        TEXT NOT NULL,
    severity    TEXT NOT NULL DEFAULT 'info',
    message     TEXT NOT NULL,
    location    TEXT,
    payload     TEXT,
    created_at  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_diag_project ON diagnostics(project_id);
"#,
    r#"
CREATE TABLE IF NOT EXISTS pipeline_runs (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL,
    phase       TEXT NOT NULL,
    nodes       INTEGER NOT NULL DEFAULT 0,
    edges       INTEGER NOT NULL DEFAULT 0,
    annotations INTEGER NOT NULL DEFAULT 0,
    aliases     INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    started_at  INTEGER NOT NULL DEFAULT 0
);
"#,
    r#"
CREATE TABLE IF NOT EXISTS project_rule_config (
    project_id  INTEGER NOT NULL,
    rule_id     TEXT NOT NULL,
    enabled     INTEGER,
    options     TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (project_id, rule_id)
);
"#,
];

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    fn apply_all() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        for sql in super::MIGRATIONS {
            conn.execute_batch(sql)
                .unwrap_or_else(|e| panic!("migration must be valid SQL: {e}\n---\n{sql}"));
        }
        conn
    }

    fn exists(conn: &Connection, kind: &str, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
            rusqlite::params![kind, name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    /// Every statement must apply **and be idempotent** — the whole set runs on every `SqliteStore::open`, so a
    /// single non-idempotent statement makes an existing database unopenable.
    #[test]
    fn migrations_apply_and_are_idempotent() {
        let conn = apply_all();
        for sql in super::MIGRATIONS {
            conn.execute_batch(sql)
                .unwrap_or_else(|e| panic!("migration must be re-runnable (IF NOT EXISTS): {e}\n---\n{sql}"));
        }
    }

    #[test]
    fn every_expected_table_is_created() {
        let conn = apply_all();
        for t in [
            "projects",
            "sub_projects",
            "source_files",
            "nodes",
            "edges",
            "node_annotations",
            "aliases",
            "symbol_tables",
            "diagnostics",
            "pipeline_runs",
            "project_rule_config",
        ] {
            assert!(exists(&conn, "table", t), "missing table `{t}`");
        }
    }

    /// Without a foreign key to `nodes`, "the node is gone but the edge remains" is undetectable — a concurrent
    /// build once produced 10 projects overwriting each other's nodes while the UI showed "36k edges / 0 nodes".
    #[test]
    fn child_tables_reference_nodes_with_cascade() {
        let conn = apply_all();
        for t in ["edges", "node_annotations", "aliases"] {
            let mut stmt = conn
                .prepare(&format!("PRAGMA foreign_key_list({t})"))
                .unwrap();
            let fks: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(2)?, r.get::<_, String>(6)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert!(!fks.is_empty(), "`{t}` must have a foreign key to nodes");
            assert!(
                fks.iter().all(|(table, on_delete)| table == "nodes" && on_delete == "CASCADE"),
                "the foreign key of `{t}` must be REFERENCES nodes ON DELETE CASCADE: {fks:?}"
            );
        }
        // `edges` references nodes on both endpoints.
        let mut stmt = conn.prepare("PRAGMA foreign_key_list(edges)").unwrap();
        let from: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(from.contains(&"from_id".to_string()) && from.contains(&"to_id".to_string()), "{from:?}");
    }

    /// These two composite indexes are the whole difference between 9.8ms and 156ms for batch edge fetch
    /// (the planner otherwise scans every edge of the project). Deleting one is a silent O(N^2) regression.
    #[test]
    fn edge_batch_lookup_indexes_exist() {
        let conn = apply_all();
        for idx in ["idx_edges_proj_to", "idx_edges_proj_from"] {
            assert!(exists(&conn, "index", idx), "missing composite index `{idx}` (batch edge reads would degrade)");
        }
    }

    /// Deliberately **not** created here (the name only appears in an explanatory SQL comment): old databases'
    /// `node_annotations` has no `project_id`, and while `CREATE TABLE IF NOT EXISTS` would skip quietly, the
    /// index statement errors out outright — making the whole database unopenable. `ensure_annotation_project`
    /// adds it after the column exists.
    #[test]
    fn annotation_project_index_is_not_created_by_the_migrations() {
        let conn = apply_all();
        assert!(
            !exists(&conn, "index", "idx_annotations_project"),
            "`idx_annotations_project` must not be created by MIGRATIONS (an old database missing the column would not open at all)"
        );
    }

    /// Every `CREATE INDEX` declared in `MIGRATIONS` must actually materialise. A silent index removal is an
    /// O(N^2) regression (see `idx_edges_proj_to` below) that no existing assertion would catch.
    #[test]
    fn every_expected_index_is_created() {
        let conn = apply_all();
        for idx in [
            "idx_sub_projects_project",
            "idx_files_project",
            "idx_files_sub",
            "idx_nodes_project",
            "idx_nodes_kind",
            "idx_nodes_fqn",
            "idx_nodes_identity",
            "idx_nodes_file",
            "idx_edges_from",
            "idx_edges_to",
            "idx_edges_project",
            "idx_edges_proj_to",
            "idx_edges_proj_from",
            "idx_annotations_node",
            "idx_annotations_kind",
            "idx_alias_lookup",
            "idx_symbols",
            "idx_diag_project",
        ] {
            assert!(exists(&conn, "index", idx), "missing index `{idx}` (performance / correctness regression)");
        }
    }

    /// The `UNIQUE` dedup keys are what keep a re-run build from duplicating rows. They must reject an exact
    /// repeat, while a different key in the same table is still allowed.
    #[test]
    fn unique_constraints_reject_duplicates() {
        let conn = apply_all();
        // This test is about the UNIQUE dedup keys, not the foreign-key cascade — turn FK off so inserts
        // referencing throw-away ids (no real node rows) don't trip the FK check.
        conn.execute("PRAGMA foreign_keys = OFF", []).unwrap();

        conn.execute(
            "INSERT INTO edges (project_id, kind, from_id, to_id) VALUES (1, 'Calls', 10, 20)",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO edges (project_id, kind, from_id, to_id) VALUES (1, 'Calls', 10, 20)",
            [],
        );
        assert!(
            dup.is_err(),
            "the edges unique key (project_id,kind,from_id,to_id) must reject duplicates: {dup:?}"
        );
        // a different key tuple is allowed
        conn.execute(
            "INSERT INTO edges (project_id, kind, from_id, to_id) VALUES (1, 'Calls', 10, 21)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO aliases (project_id, namespace, key, node_id) VALUES (1, 'ns', 'k', 5)",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO aliases (project_id, namespace, key, node_id) VALUES (1, 'ns', 'k', 6)",
            [],
        );
        assert!(dup.is_err(), "the aliases unique key must reject duplicates: {dup:?}");

        conn.execute(
            "INSERT INTO symbol_tables (project_id, table_name, key, value) VALUES (1, 'schema', 'users', 'x')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO symbol_tables (project_id, table_name, key, value) VALUES (1, 'schema', 'users', 'y')",
            [],
        );
        assert!(dup.is_err(), "the symbol_tables unique key must reject duplicates: {dup:?}");
    }

    /// The `REFERENCES nodes ON DELETE CASCADE` declaration only matters if it actually fires: an edge to a
    /// non-existent node must be rejected, and deleting a node must take its edges with it — otherwise the
    /// "36k edges / 0 nodes" dirty-data state (a concurrent build once produced it) is invisible.
    #[test]
    fn node_deletion_cascades_to_edges() {
        let conn = apply_all();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, project_id, kind, name) VALUES (100, 1, 'Class', 'C')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO edges (project_id, kind, from_id, to_id) VALUES (1, 'Calls', 100, 100)",
            [],
        )
        .unwrap();

        // FK enforcement: an edge to a node that does not exist must be rejected.
        let orphan = conn.execute(
            "INSERT INTO edges (project_id, kind, from_id, to_id) VALUES (1, 'Calls', 999, 999)",
            [],
        );
        assert!(
            orphan.is_err(),
            "with foreign keys on, an edge pointing at a missing node must be rejected: {orphan:?}"
        );

        // Cascade: deleting the node removes its edges.
        conn.execute("DELETE FROM nodes WHERE id = 100", []).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges WHERE to_id = 100", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "deleting a node must cascade to its edges");
    }
}
