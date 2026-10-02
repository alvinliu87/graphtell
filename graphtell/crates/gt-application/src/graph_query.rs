//! Graph query use cases (for the UI to browse and search).

use std::sync::Arc;

use gt_domain::error::Result;
use gt_domain::model::{
    Annotation, Diagnostic, Edge, Node, NodeId, NodeKind, ProjectId, RULE_CODE_PREFIX,
    SymbolEntry,
};
use gt_domain::port::{EdgeDirection, GraphStats, NodeFilter, Persistence};
use serde::{Deserialize, Serialize};

/// The graph query service.
pub struct GraphQueryService {
    store: Arc<dyn Persistence>,
}

/// Severity rollup of non-rule diagnostics (for menu badges).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DiagnosticSummary {
    pub critical: u64,
    pub error: u64,
    pub warning: u64,
    pub info: u64,
    /// **Languages with no parser yet** (`go` / `rust` …), from the `unsupported_languages` symbol table written by
    /// P2.
    ///
    /// There is also a `NoParserForLanguage` build diagnostic, but the language name there only appears inside the
    /// copy, so a banner in the UI would have to parse the string back out. This gives a structured version the UI
    /// can read directly.
    #[serde(default)]
    pub unsupported_languages: Vec<String>,
    /// Entry counts split by **issue type** (diagnostic code); different severities of one code each get their own
    /// row.
    ///
    /// So the UI can say "6 kinds of problems" instead of "445 diagnostics": one engine diagnostic repeats at
    /// hundreds of places, and giving only the total makes users read "the same thing happened 349 times" as "349
    /// problems". `#[serde(default)]` keeps old frontends / old snapshots from blowing up when the field is absent.
    #[serde(default)]
    pub by_code: Vec<DiagnosticCodeCount>,
}

/// The entry count of one diagnostic kind (the same `code` + the same severity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCodeCount {
    pub code: String,
    /// `critical` / `error` / `warning` / `info` (the same convention as what is persisted).
    pub severity: String,
    pub count: u64,
}

impl GraphQueryService {
    pub fn new(store: Arc<dyn Persistence>) -> Self {
        Self { store }
    }

    pub fn stats(&self, project_id: ProjectId) -> Result<GraphStats> {
        self.store.stats(project_id)
    }

    pub fn nodes(
        &self,
        project_id: ProjectId,
        kind: Option<&str>,
        name_contains: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Node>> {
        self.store.query_nodes(&NodeFilter {
            project_id,
            kind: kind.map(NodeKind::new),
            name_contains: name_contains.map(|s| s.to_string()),
            limit: Some(limit),
            offset: Some(offset),
        })
    }

    pub fn node(&self, id: NodeId) -> Result<Option<Node>> {
        self.store.get_node(id)
    }

    pub fn neighbors(&self, id: NodeId, direction: EdgeDirection) -> Result<Vec<Edge>> {
        self.store.edges_of(id, direction)
    }

    pub fn annotations(&self, id: NodeId) -> Result<Vec<Annotation>> {
        self.store.annotations_of(id)
    }

    pub fn symbols(&self, project_id: ProjectId, table: &str) -> Result<Vec<SymbolEntry>> {
        self.store.list_symbols(project_id, table)
    }

    /// **Build-time** diagnostics (missing root, broken links, identity conflicts, etc.), excluding compliance
    /// violations.
    ///
    /// The `rule:` prefix must be excluded, and the convention must match [`Self::diagnostics_summary`]:
    /// build diagnostics and compliance violations live in the same table, so with only a `LIMIT` to cut them off,
    /// which ones surface depends on the **write order** (violations are written later with larger ids, so they push
    /// the whole build-diagnostic block out), and the page ends up showing "badge 0 errors, and a pile of unrelated
    /// things in the table".
    pub fn diagnostics(&self, project_id: ProjectId, limit: u32) -> Result<Vec<Diagnostic>> {
        self.store
            .list_diagnostics_excluding(project_id, RULE_CODE_PREFIX, limit)
    }

    /// A severity rollup of non-rule diagnostics (for menu badges).
    ///
    /// The diagnostics page shows "non-rule" diagnostics (missing root, broken links, identity conflicts, etc.);
    /// rule violations already have their own compliance-check section, so the `rule:` prefix is excluded here to
    /// avoid double counting.
    pub fn diagnostics_summary(&self, project_id: ProjectId) -> Result<DiagnosticSummary> {
        let counts = self.store.count_diagnostics_excluding(project_id, RULE_CODE_PREFIX)?;
        let mut s = DiagnosticSummary::default();
        for (sev, n) in counts {
            match sev.as_str() {
                "critical" => s.critical = n,
                "error" => s.error = n,
                "warning" => s.warning = n,
                "info" => s.info = n,
                _ => {}
            }
        }
        s.by_code = self
            .store
            .count_diagnostics_by_code_excluding(project_id, RULE_CODE_PREFIX)?
            .into_iter()
            .map(|(code, severity, count)| DiagnosticCodeCount { code, severity, count })
            .collect();
        s.unsupported_languages = self
            .store
            .list_symbols(project_id, "unsupported_languages")?
            .into_iter()
            .map(|e| e.key)
            .collect();
        s.unsupported_languages.sort();
        s.unsupported_languages.dedup();
        Ok(s)
    }

    /// A subgraph (BFS, size-limited, for graphical display).
    pub fn subgraph(&self, root: NodeId, depth: u32, max_nodes: usize) -> Result<Subgraph> {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut frontier = vec![root];
        visited.insert(root.get());

        for _ in 0..depth.max(1) {
            let mut next = Vec::new();
            for id in frontier {
                if let Some(n) = self.store.get_node(id)? {
                    nodes.push(n);
                }
                for e in self.store.edges_of(id, EdgeDirection::Both)? {
                    edges.push(e.clone());
                    let other = if e.from_id == id { e.to_id } else { e.from_id };
                    if visited.insert(other.get()) && nodes.len() < max_nodes {
                        next.push(other);
                    }
                }
                if nodes.len() >= max_nodes {
                    break;
                }
            }
            frontier = next;
            if frontier.is_empty() || nodes.len() >= max_nodes {
                break;
            }
        }
        Ok(Subgraph { nodes, edges })
    }
}

/// A subgraph snapshot.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
