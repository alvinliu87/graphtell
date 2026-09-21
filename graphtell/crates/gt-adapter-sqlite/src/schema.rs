//! SQLite 表结构。

/// 全部建表语句（幂等）。
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
    from_id     INTEGER NOT NULL,
    to_id       INTEGER NOT NULL,
    phase       TEXT NOT NULL DEFAULT '',
    confidence  REAL NOT NULL DEFAULT 1.0,
    properties  TEXT,
    UNIQUE(project_id, kind, from_id, to_id)
);
CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_id);
CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_id);
CREATE INDEX IF NOT EXISTS idx_edges_project ON edges(project_id);
-- 复合索引：**批量取边的性能关键**，不要删。
--
-- `edges_incoming` / `edges_outgoing` 的查询形如
--   WHERE to_id IN (<400 个 id>) AND project_id IN (?)
-- 只有单列索引时，SQLite 的规划器会选 `idx_edges_project` ——
-- 也就是**扫描该工程的全部边**再逐行过滤 `to_id IN (...)`，
-- 代价 = chunk 数 × 工程边数 = O(N²)：实测 16k 节点取入边 156ms，
-- 且随规模按约 N^1.5 增长（而 `query_nodes` 是线性）。
--
-- 有了 (project_id, to_id) 后，规划器改为
--   SEARCH edges USING INDEX idx_edges_proj_to (project_id=? AND to_id=?)
-- 两个条件都走索引 —— 同上实测 9.8ms，且规模 ×4 耗时 ×3.4（线性）。
CREATE INDEX IF NOT EXISTS idx_edges_proj_to ON edges(project_id, to_id);
CREATE INDEX IF NOT EXISTS idx_edges_proj_from ON edges(project_id, from_id);
"#,
    r#"
CREATE TABLE IF NOT EXISTS node_annotations (
    id          INTEGER PRIMARY KEY,
    node_id     INTEGER NOT NULL,
    channel     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    subkind     TEXT,
    confidence  REAL NOT NULL DEFAULT 1.0,
    evidence    TEXT,
    phase       TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_annotations_node ON node_annotations(node_id);
CREATE INDEX IF NOT EXISTS idx_annotations_kind ON node_annotations(kind);
"#,
    r#"
CREATE TABLE IF NOT EXISTS aliases (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL,
    namespace   TEXT NOT NULL,
    key         TEXT NOT NULL,
    qualifier   TEXT NOT NULL DEFAULT '',
    node_id     INTEGER NOT NULL,
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
];
