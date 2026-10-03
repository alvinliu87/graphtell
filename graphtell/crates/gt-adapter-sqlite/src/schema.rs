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
