use super::*;
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
