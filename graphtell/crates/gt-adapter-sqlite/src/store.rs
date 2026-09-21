//! SQLite 仓储实现。

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Mutex,
};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    Annotation, AnnotationChannel, Diagnostic, Edge, EdgeId, EdgeKind, FileId, GraphDelta,
    Language, NewProject, NewSourceFile, NewSubProject,
    Node, NodeId, NodeKind, Phase, Project, ProjectConfig, ProjectId, ProjectPatch, ProjectStatus,
    Severity, SourceFile, Span, SubProject, SubProjectId, SymbolEntry,
};
use gt_domain::port::{
    DiagnosticSink, EdgeDirection, GraphQuery, GraphSink, GraphStats, NodeFilter, ProjectReader,
    ProjectWriter, SymbolTableReader,
};
use gt_domain::model::graph::NodeSummary;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tracing::{debug, info};

use crate::schema::MIGRATIONS;

/// SQLite 仓储。
///
/// `Connection` 不是 `Sync`，用 `Mutex` 包装以满足端口的 `Send + Sync` 约束。
pub struct SqliteStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl SqliteStore {
    /// 取一批节点所属的工程集合（去重）。
    ///
    /// # 为什么必须**只在批量查询开头算一次**
    ///
    /// 原来这个过滤是写在每个 chunk 的 SQL 里的子查询
    /// `project_id IN (SELECT project_id FROM nodes WHERE id IN (<本 chunk 的 400 个 id>))`。
    /// chunk 数随规模线性增长，于是这个子查询被**重复执行 N/400 次**；
    /// 更要命的是 SQLite 对 `IN (SELECT …)` 会为每次执行重建一张临时表，
    /// 代价与子查询结果集大小相关 —— 实测让批量取边退化到约 **N^1.5~1.7**
    /// （16k 节点：取入边 156ms / 出边 131ms，是 `query_nodes` 的 4~5 倍）。
    ///
    /// 调用方总是传同一工程的 id 集合，所以这里一次性求出工程集合、
    /// 再以极短的常量列表（通常只有 1 个值）下发给每个 chunk 即可。
    /// 语义完全不变，代价从"每 chunk 一次子查询"降为"总共一次查询"。
    fn project_ids_of(&self, ids: &[NodeId]) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut out: Vec<i64> = Vec::new();
        // 900 是 SQLite 默认变量上限的保守取值（默认 999，留点余量）
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

    /// 打开（或创建）数据库并执行迁移。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(DomainError::infra)?;
        }
        let conn = Connection::open(&path).map_err(DomainError::infra)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")
            .map_err(DomainError::infra)?;
        for sql in MIGRATIONS {
            conn.execute_batch(sql).map_err(DomainError::infra)?;
        }
        info!("SQLite 已打开: {}", path.display());
        Ok(Self { conn: Mutex::new(conn), path })
    }

    /// 内存库（测试用）。
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(DomainError::infra)?;
        for sql in MIGRATIONS {
            conn.execute_batch(sql).map_err(DomainError::infra)?;
        }
        Ok(Self { conn: Mutex::new(conn), path: PathBuf::from(":memory:") })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------- 工程

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
        // 外键未级联到的图数据手动清理
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
        // 回填 sub_project_id 后由调用方重新写入
        drop(stmt);
        drop(conn);
        self.list_files(project_id, None)
    }
}

// ---------------------------------------------------------------- 图

impl GraphSink for SqliteStore {
    fn max_node_id(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let m: i64 = conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM nodes", [], |r| r.get(0))
            .map_err(DomainError::infra)?;
        Ok(m)
    }

    fn apply(&self, delta: &GraphDelta) -> Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(DomainError::infra)?;

        if delta.reset_project {
            if let Some(pid) = delta.project_id {
                // 顺序重要：注解要**先于** nodes 删除 —— 它靠 `node_id IN (SELECT id FROM nodes …)`
                // 定位，若 nodes 已被清空，子查询为空，注解就永远删不掉，每次重跑都会叠加一层。
                for sql in [
                    "DELETE FROM node_annotations WHERE node_id IN (SELECT id FROM nodes WHERE project_id = ?1)",
                    "DELETE FROM nodes WHERE project_id = ?1",
                    "DELETE FROM edges WHERE project_id = ?1",
                    "DELETE FROM aliases WHERE project_id = ?1",
                    "DELETE FROM symbol_tables WHERE project_id = ?1",
                    "DELETE FROM diagnostics WHERE project_id = ?1",
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
                    n.identity.as_ref().map(|i| i.key()),
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

        // 属性补丁
        for (id, patch) in &delta.property_patches {
            let current: Option<String> = tx
                .query_row("SELECT properties FROM nodes WHERE id = ?1", params![id.get()], |r| r.get(0))
                .optional()
                .map_err(DomainError::infra)?;
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

        // 共现位置追加（合成节点的多处定义）
        for (id, loc) in &delta.location_patches {
            let current: Option<String> = tx
                .query_row("SELECT properties FROM nodes WHERE id = ?1", params![id.get()], |r| r.get(0))
                .optional()
                .map_err(DomainError::infra)?;
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
                    "INSERT INTO node_annotations (node_id, channel, kind, subkind, confidence, evidence, phase)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )
                .map_err(DomainError::infra)?;
            for a in &delta.annotations {
                stmt.execute(params![
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
                    "INSERT INTO diagnostics (project_id, phase, code, severity, message, location, payload, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                )
                .map_err(DomainError::infra)?;
            for d in &delta.diagnostics {
                stmt.execute(params![
                    d.project_id.get(),
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
            "落库: {} 节点 / {} 边 / {} 标注",
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
            // 同时匹配 `kind` 与 `properties.category`：
            // 同时匹配 `kind` 与 `properties.category`（`category` 目前与 `kind` 一致，
            // 此兜底保留以兼容任何以 category 分组的查询）。
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
        // 边必须与其端点节点同属一个工程：库中可能残留历史工程的边
        // （删除工程后节点被清、边未清，或端点 id 被新工程复用），
        // 不过滤会把旧工程的边混进当前工程，污染视图与证据链。
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
            // 同 `edges_of`：按端点节点所属工程过滤，挡掉历史工程的残留边。
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
        // 工程集合**只算一次**（见 `project_ids_of` 的说明）。
        let projects = self.project_ids_of(ids)?;
        if projects.is_empty() {
            return Ok(HashMap::new());
        }
        let proj_ph = vec!["?"; projects.len()].join(",");
        let conn = self.conn.lock().unwrap();
        let mut inc: HashMap<i64, Vec<Edge>> = HashMap::new();
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // 同 `edges_of`：按端点节点所属工程过滤，挡掉历史工程的残留边。
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
        // 只取链边的整数邻接，不含 `properties` 等重列；一次取全工程，内存里分桶。
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
        // 只含**语义边**的入边邻接：给候选徽标按"语义入边"计数（语法调用边不计）。
        // 同一次查询顺手分桶，不额外往返（kind 本来就在结果行里）。
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

    fn annotation_kinds(&self, project_id: ProjectId) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT a.channel, a.kind FROM node_annotations a
                 JOIN nodes n ON n.id = a.node_id
                 WHERE n.project_id = ?1
                 ORDER BY a.channel, a.kind",
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
                "SELECT a.id, a.node_id, a.channel, a.kind, a.subkind, a.confidence, a.evidence, a.phase
                 FROM node_annotations a
                 JOIN nodes n ON n.id = a.node_id
                 WHERE n.project_id = ?1
                 ORDER BY a.id",
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
                "SELECT COUNT(*) FROM node_annotations WHERE node_id IN
                 (SELECT id FROM nodes WHERE project_id = ?1)",
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
        // 按 `properties.category` 统计（`category` 目前与 `kind` 一致）。
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

// ---------------------------------------------------------------- 符号表 & 诊断

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
                "SELECT project_id, phase, code, severity, message, location, payload
                 FROM diagnostics WHERE project_id = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(DomainError::infra)?;
        let rows = stmt
            .query_map(params![project_id.get(), limit as i64], |r| {
                Ok(Diagnostic {
                    project_id: ProjectId(r.get(0)?),
                    phase: Phase(r.get(1)?),
                    code: r.get(2)?,
                    severity: parse_json::<Severity>(
                        &r.get::<_, String>(3).unwrap_or_else(|_| "\"info\"".into()),
                    )
                    .unwrap_or(Severity::Info),
                    message: r.get(4)?,
                    location: r.get(5)?,
                    payload: parse_json(&r.get::<_, String>(6).unwrap_or_else(|_| "null".into()))
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
        limit: u32,
    ) -> Result<Vec<Diagnostic>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT project_id, phase, code, severity, message, location, payload
                 FROM diagnostics
                 WHERE project_id = ?1 AND code LIKE ?2 ESCAPE '\\'
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(DomainError::infra)?;
        let pattern = format!("{}%", like_escape(code_prefix));
        let rows = stmt
            .query_map(params![project_id.get(), pattern, limit as i64], |r| {
                Ok(Diagnostic {
                    project_id: ProjectId(r.get(0)?),
                    phase: Phase(r.get(1)?),
                    code: r.get(2)?,
                    severity: parse_json::<Severity>(
                        &r.get::<_, String>(3).unwrap_or_else(|_| "\"info\"".into()),
                    )
                    .unwrap_or(Severity::Info),
                    message: r.get(4)?,
                    location: r.get(5)?,
                    payload: parse_json(&r.get::<_, String>(6).unwrap_or_else(|_| "null".into()))
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
}

/// 转义 `LIKE` 通配符，避免规则 id 里的 `%` / `_` 被当成通配符。
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

// ---------------------------------------------------------------- 辅助

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
            .map(|s| identity_from_key(&s)),
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

fn identity_from_key(key: &str) -> gt_domain::model::IdentityKey {
    match key.split_once(':') {
        Some((kind, value)) => gt_domain::model::IdentityKey {
            kind: gt_domain::model::SynthesizedKind(kind.to_string()),
            value: value.to_string(),
        },
        None => gt_domain::model::IdentityKey::named(key),
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
