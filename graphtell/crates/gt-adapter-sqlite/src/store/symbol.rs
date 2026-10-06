use super::*;
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
