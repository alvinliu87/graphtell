use super::*;
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
