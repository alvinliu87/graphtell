use super::*;
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
        side: Option<&str>,
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
        if let Some(side) = side {
            // Scalar equality: a node is owned by exactly one party, so `side` names it — no set to walk.
            let p = binds.len() + 1;
            sql.push_str(&format!(" AND json_extract(properties, '$.side') = ?{p}"));
            binds.push(Box::new(side.to_string()));
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

    fn coverage(&self, project_id: ProjectId) -> Result<CoverageReport> {
        let conn = self.conn.lock().unwrap();

        // A call site is "extracted by FKB" when its **enclosing method** contributes at least one
        // *semantic* or *bridge* edge (the only edges a rule produces). The graph does not point the
        // edge at the CallSite node itself — it points at the method that contains the call — so we scan
        // every edge once to (a) collect the set of nodes that took part in a semantic/bridge edge and
        // (b) record the `HasCallSite` mapping (method -> call site) used to credit a call site by its
        // owner. Classification uses the engine's own `is_semantic_edge` / `is_bridge_edge` so FKB-
        // declared edge kinds are covered automatically.
        let mut edge_stmt = conn
            .prepare("SELECT from_id, to_id, kind FROM edges WHERE project_id = ?1")
            .map_err(DomainError::infra)?;
        let edges = edge_stmt
            .query_map(params![project_id.get()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
            })
            .map_err(DomainError::infra)?
            .collect::<std::result::Result<Vec<(i64, i64, String)>, _>>()
            .map_err(DomainError::infra)?;

        let mut covered_nodes: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut callsite_owner: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for (from, to, kind) in &edges {
            if gt_domain::model::kinds::is_semantic_edge(kind)
                || gt_domain::model::kinds::is_bridge_edge(kind)
            {
                covered_nodes.insert(*from);
                covered_nodes.insert(*to);
            } else if kind == "HasCallSite" {
                callsite_owner.insert(*to, *from);
            }
        }

        // Load every CallSite node (dev/diagnostic command; acceptable to hold in memory).
        let mut cs_stmt = conn
            .prepare(
                "SELECT id, sub_project_id, name, file_id, start_line \
                 FROM nodes WHERE project_id = ?1 AND kind = 'CallSite'",
            )
            .map_err(DomainError::infra)?;
        let call_sites = cs_stmt
            .query_map(params![project_id.get()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, i32>(4)?,
                ))
            })
            .map_err(DomainError::infra)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DomainError::infra)?;

        let mut file_paths: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        {
            let mut fp_stmt = conn
                .prepare("SELECT id, path FROM source_files WHERE project_id = ?1")
                .map_err(DomainError::infra)?;
            let rows = fp_stmt
                .query_map(params![project_id.get()], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(DomainError::infra)?;
            for r in rows {
                let (id, path) = r.map_err(DomainError::infra)?;
                file_paths.insert(id, path);
            }
        }

        // Sub-projects — queried under the same lock (the mutex is not reentrant).
        let mut subs: Vec<(i64, String, String, String, String)> = Vec::new();
        {
            let mut sp_stmt = conn
                .prepare(
                    "SELECT id, name, language, role, frameworks \
                     FROM sub_projects WHERE project_id = ?1",
                )
                .map_err(DomainError::infra)?;
            let rows = sp_stmt
                .query_map(params![project_id.get()], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .map_err(DomainError::infra)?;
            for r in rows {
                subs.push(r.map_err(DomainError::infra)?);
            }
        }

        // Per-sub accumulators.
        let mut total: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
        let mut covered: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
        // Distinct-callee sampling of uncovered call sites, capped per sub-project.
        const SAMPLE_CAP: usize = 25;
        let mut samples: std::collections::HashMap<i64, (Vec<UncoveredCall>, std::collections::HashSet<String>)> =
            std::collections::HashMap::new();

        for (id, sub_opt, name, file_id, line) in call_sites {
            let sub = sub_opt.unwrap_or(-1);
            *total.entry(sub).or_insert(0) += 1;
            // A call site counts as covered when its owner method took part in a semantic/bridge edge
            // (or, rarely, the call site node itself did).
            let is_covered = callsite_owner
                .get(&id)
                .map_or(false, |owner| covered_nodes.contains(owner))
                || covered_nodes.contains(&id);
            if is_covered {
                *covered.entry(sub).or_insert(0) += 1;
            } else if let Some(slot) = samples.get_mut(&sub) {
                if slot.1.len() < SAMPLE_CAP && slot.1.insert(name.clone()) {
                    slot.0.push(UncoveredCall {
                        callee: name,
                        file: file_id.and_then(|fid| file_paths.get(&fid).cloned()),
                        line: line.max(0) as u32,
                    });
                }
            } else {
                let mut seen = std::collections::HashSet::new();
                seen.insert(name.clone());
                samples.insert(
                    sub,
                    (
                        vec![UncoveredCall {
                            callee: name,
                            file: file_id.and_then(|fid| file_paths.get(&fid).cloned()),
                            line: line.max(0) as u32,
                        }],
                        seen,
                    ),
                );
            }
        }

        let mut sub_report = Vec::new();
        let mut totals_calls = 0u64;
        let mut totals_covered = 0u64;
        let mut with_gaps = 0u64;
        for (sid, name, language, role, frameworks_json) in subs {
            let t = *total.get(&sid).unwrap_or(&0);
            let c = *covered.get(&sid).unwrap_or(&0);
            let ratio = if t == 0 { 1.0 } else { c as f64 / t as f64 };
            let frameworks: Vec<String> =
                serde_json::from_str(&frameworks_json).unwrap_or_default();
            let mut flags = Vec::new();
            let mut knowledge_gap = false;
            if language.eq_ignore_ascii_case("unknown") {
                flags.push("language_unknown".to_string());
                knowledge_gap = true;
            }
            if frameworks.is_empty() {
                flags.push("no_framework".to_string());
                knowledge_gap = true;
            }
            if t >= 20 && ratio < 0.3 {
                // Advisory only. Most call sites in real code are utility calls (`Math.min`,
                // `JSON.parse`, `console.log`, `this.$emit` …) that no rule should ever capture, and
                // non-literal URLs are rejected on purpose (they would invent junk contracts). So a low
                // ratio alone does NOT prove a knowledge gap, and it must not count toward `with_gaps`.
                flags.push("low_coverage".to_string());
            }
            if knowledge_gap {
                with_gaps += 1;
            }
            totals_calls += t;
            totals_covered += c;
            let (samp, _) = samples.remove(&sid).unwrap_or_default();
            sub_report.push(SubCoverage {
                sub_project_id: sid,
                name,
                language,
                role,
                frameworks,
                total_calls: t,
                covered_calls: c,
                coverage_ratio: ratio,
                flags,
                uncovered_samples: samp,
            });
        }
        // Any call sites with no sub-project (sub = -1): fold into a synthetic entry.
        if let Some(t) = total.get(&-1) {
            let c = covered.get(&-1).unwrap_or(&0);
            let ratio = if *t == 0 { 1.0 } else { *c as f64 / *t as f64 };
            let (samp, _) = samples.remove(&-1).unwrap_or_default();
            totals_calls += t;
            totals_covered += c;
            with_gaps += 1;
            sub_report.push(SubCoverage {
                sub_project_id: -1,
                name: "(unassigned)".to_string(),
                language: "unknown".to_string(),
                role: String::new(),
                frameworks: vec![],
                total_calls: *t,
                covered_calls: *c,
                coverage_ratio: ratio,
                flags: vec!["no_framework".to_string()],
                uncovered_samples: samp,
            });
        }

        let ratio = if totals_calls == 0 {
            1.0
        } else {
            totals_covered as f64 / totals_calls as f64
        };
        Ok(CoverageReport {
            project_id: project_id.get(),
            totals: CoverageTotals {
                total_calls: totals_calls,
                covered_calls: totals_covered,
                coverage_ratio: ratio,
                sub_projects: sub_report.len() as u64,
                sub_projects_with_gaps: with_gaps,
            },
            sub_projects: sub_report,
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
