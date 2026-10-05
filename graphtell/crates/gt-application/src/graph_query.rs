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

    /// A subgraph (BFS, size-limited, for graphical display). `depth` is the number of hops from `root`:
    /// `depth = 1` yields the root plus its immediate neighbours; `max_nodes` caps the total node count and stops
    /// traversal once hit (so a long branch does not displace its siblings).
    pub fn subgraph(&self, root: NodeId, depth: u32, max_nodes: usize) -> Result<Subgraph> {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut frontier = vec![root];
        visited.insert(root.get());

        for _ in 0..=depth.max(1) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use serde_json::Value;
    use gt_domain::model::{
        Annotation, Diagnostic, Edge, EdgeId, FileId, GraphDelta, NewProject, NewSourceFile,
        NewSubProject, Node, NodeId, NodeKind, Project, ProjectId, ProjectPatch,
        ProjectRuleConfig, ProjectStatus, SourceFile, SubProject, SubProjectId, SymbolEntry,
        EdgeKind, Language, Phase, Span,
    };
    use gt_domain::model::graph::NodeSummary;
    use gt_domain::port::{
        DiagnosticSink, EdgeDirection, GraphQuery, GraphSink, GraphStats, NodeFilter,
        ProjectReader, ProjectWriter, RuleConfigStore, SymbolTableReader,
    };

    // ----- a minimal in-memory `Persistence` double: only the methods `GraphQueryService` actually calls are
    // real; the rest panic if ever invoked (they are not, by construction of these tests). -----
    #[derive(Default)]
    struct MemStore {
        nodes: HashMap<i64, Node>,
        edges: Vec<Edge>,
        sev_counts: Vec<(String, u64)>,
        code_counts: Vec<(String, String, u64)>,
        symbols: Vec<SymbolEntry>,
        stats: GraphStats,
        last_filter: Mutex<Option<NodeFilter>>,
    }

    fn node(id: i64, kind: &str, name: &str) -> Node {
        Node {
            id: NodeId::new(id),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            kind: NodeKind(kind.to_string()),
            name: name.to_string(),
            fqn: None,
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language(Language::UNKNOWN.to_string()),
            phase: Phase(Phase::CF_AST.to_string()),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    fn edge(id: i64, from: i64, to: i64, kind: &str) -> Edge {
        Edge {
            id: EdgeId::new(id),
            project_id: ProjectId::new(1),
            kind: EdgeKind(kind.to_string()),
            from_id: NodeId::new(from),
            to_id: NodeId::new(to),
            phase: Phase(Phase::CF_AST.to_string()),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    impl ProjectReader for MemStore {
        fn get_project(&self, _: ProjectId) -> Result<Option<Project>> { unimplemented!() }
        fn list_projects(&self) -> Result<Vec<Project>> { unimplemented!() }
        fn list_sub_projects(&self, _: ProjectId) -> Result<Vec<SubProject>> { unimplemented!() }
        fn list_files(&self, _: ProjectId, _: Option<SubProjectId>) -> Result<Vec<SourceFile>> {
            unimplemented!()
        }
    }
    impl ProjectWriter for MemStore {
        fn create_project(&self, _: NewProject) -> Result<Project> { unimplemented!() }
        fn update_project(&self, _: ProjectId, _: ProjectPatch) -> Result<Project> { unimplemented!() }
        fn delete_project(&self, _: ProjectId) -> Result<()> { unimplemented!() }
        fn set_project_status(&self, _: ProjectId, _: ProjectStatus) -> Result<()> { unimplemented!() }
        fn replace_sub_projects(&self, _: ProjectId, _: Vec<NewSubProject>) -> Result<Vec<SubProject>> {
            unimplemented!()
        }
        fn update_sub_project_facts(&self, _: SubProjectId, _: Value) -> Result<()> { unimplemented!() }
        fn set_sub_project_frameworks(&self, _: SubProjectId, _: Vec<String>) -> Result<()> {
            unimplemented!()
        }
        fn replace_files(&self, _: ProjectId, _: Vec<NewSourceFile>) -> Result<Vec<SourceFile>> {
            unimplemented!()
        }
    }
    impl GraphSink for MemStore {
        fn apply(&self, _: &GraphDelta) -> Result<()> { unimplemented!() }
    }
    impl GraphQuery for MemStore {
        fn query_nodes(&self, f: &NodeFilter) -> Result<Vec<Node>> {
            *self.last_filter.lock().unwrap() = Some(f.clone());
            Ok(vec![])
        }
        fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
            Ok(self.nodes.get(&id.get()).cloned())
        }
        fn edges_of(&self, node: NodeId, direction: EdgeDirection) -> Result<Vec<Edge>> {
            let id = node.get();
            Ok(self
                .edges
                .iter()
                .filter(|e| match direction {
                    EdgeDirection::Outgoing => e.from_id.get() == id,
                    EdgeDirection::Incoming => e.to_id.get() == id,
                    EdgeDirection::Both => e.from_id.get() == id || e.to_id.get() == id,
                })
                .cloned()
                .collect())
        }
        fn get_nodes(&self, _: &[NodeId]) -> Result<HashMap<i64, Node>> { unimplemented!() }
        fn nodes_summary(&self, _: ProjectId) -> Result<HashMap<i64, NodeSummary>> { unimplemented!() }
        fn edges_outgoing(&self, _: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>> { unimplemented!() }
        fn edges_incoming(&self, _: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>> { unimplemented!() }
        fn chain_adjacency(&self, _: ProjectId) -> Result<(HashMap<i64, Vec<i64>>, HashMap<i64, Vec<i64>>, HashMap<i64, Vec<i64>>)> {
            unimplemented!()
        }
        fn edge_kinds(&self, _: ProjectId) -> Result<Vec<String>> { unimplemented!() }
        fn node_kinds(&self, _: ProjectId) -> Result<Vec<String>> { unimplemented!() }
        fn annotation_kinds(&self, _: ProjectId) -> Result<Vec<(String, String)>> { unimplemented!() }
        fn annotations_of(&self, _: NodeId) -> Result<Vec<Annotation>> { unimplemented!() }
        fn annotations_of_project(&self, _: ProjectId) -> Result<HashMap<i64, Vec<Annotation>>> {
            unimplemented!()
        }
        fn file_paths(&self, _: ProjectId) -> Result<HashMap<i64, String>> { unimplemented!() }
        fn stats(&self, _: ProjectId) -> Result<GraphStats> {
            Ok(self.stats.clone())
        }
        fn count_nodes(&self, _: ProjectId, _: Option<&NodeKind>, _: &[String]) -> Result<u64> {
            unimplemented!()
        }
        fn find_edge(&self, _: EdgeId) -> Result<Option<Edge>> { unimplemented!() }
        fn file_path(&self, _: FileId) -> Result<Option<String>> { unimplemented!() }
    }
    impl SymbolTableReader for MemStore {
        fn get_symbol(&self, _: ProjectId, _: &str, _: &str) -> Result<Option<Value>> {
            unimplemented!()
        }
        fn list_symbols(&self, _: ProjectId, table: &str) -> Result<Vec<SymbolEntry>> {
            Ok(self.symbols.iter().filter(|s| s.table == table).cloned().collect())
        }
    }
    impl DiagnosticSink for MemStore {
        fn push_diagnostics(&self, _: &[Diagnostic]) -> Result<()> { unimplemented!() }
        fn list_diagnostics(&self, _: ProjectId, _: u32) -> Result<Vec<Diagnostic>> { unimplemented!() }
        fn list_diagnostics_excluding(&self, _: ProjectId, _: &str, _: u32) -> Result<Vec<Diagnostic>> {
            unimplemented!()
        }
        fn list_diagnostics_by_code(&self, _: ProjectId, _: &str, _: Option<&[SubProjectId]>, _: u32) -> Result<Vec<Diagnostic>> {
            unimplemented!()
        }
        fn clear_diagnostics(&self, _: ProjectId, _: &str) -> Result<u64> { unimplemented!() }
        fn count_diagnostics_by_code(&self, _: ProjectId, _: &str, _: Option<&[SubProjectId]>) -> Result<Vec<(String, u64)>> {
            unimplemented!()
        }
        fn count_diagnostics_excluding(&self, _: ProjectId, _: &str) -> Result<Vec<(String, u64)>> {
            Ok(self.sev_counts.clone())
        }
        fn count_diagnostics_by_code_excluding(&self, _: ProjectId, _: &str) -> Result<Vec<(String, String, u64)>> {
            Ok(self.code_counts.clone())
        }
    }
    impl RuleConfigStore for MemStore {
        fn get_rule_configs(&self, _: ProjectId) -> Result<HashMap<String, ProjectRuleConfig>> {
            unimplemented!()
        }
        fn set_rule_config(&self, _: &ProjectRuleConfig) -> Result<()> { unimplemented!() }
        fn delete_rule_config(&self, _: ProjectId, _: &str) -> Result<()> { unimplemented!() }
    }

    fn svc(store: MemStore) -> (GraphQueryService, Arc<MemStore>) {
        let arc = Arc::new(store);
        (GraphQueryService::new(arc.clone()), arc)
    }

    /// `diagnostics_summary` maps severities by name (unknown severities ignored), keeps the per-code breakdown,
    /// and sorts+dedups the `unsupported_languages` symbol list.
    #[test]
    fn diagnostics_summary_rollup_and_unsupported_languages_dedup() {
        let mut store = MemStore::default();
        store.sev_counts = vec![
            ("critical".into(), 2),
            ("error".into(), 5),
            ("warning".into(), 3),
            ("info".into(), 1),
            ("weird_sev".into(), 9), // must be ignored
        ];
        store.code_counts = vec![
            ("missing_root".into(), "critical".into(), 2),
            ("broken_link".into(), "error".into(), 3),
        ];
        store.symbols = vec![
            SymbolEntry { project_id: ProjectId::new(1), table: "unsupported_languages".into(), key: "go".into(), value: Value::Null },
            SymbolEntry { project_id: ProjectId::new(1), table: "unsupported_languages".into(), key: "rust".into(), value: Value::Null },
            SymbolEntry { project_id: ProjectId::new(1), table: "unsupported_languages".into(), key: "go".into(), value: Value::Null },
        ];

        let (s, _arc) = svc(store);
        let s = s.diagnostics_summary(ProjectId::new(1)).unwrap();
        assert_eq!(s.critical, 2);
        assert_eq!(s.error, 5);
        assert_eq!(s.warning, 3);
        assert_eq!(s.info, 1);
        let codes: Vec<&str> = s.by_code.iter().map(|c| c.code.as_str()).collect();
        assert!(codes.contains(&"missing_root"), "by_code 必须保留每条诊断码");
        assert!(codes.contains(&"broken_link"));
        assert_eq!(s.by_code.len(), 2);
        assert_eq!(s.unsupported_languages, vec!["go".to_string(), "rust".to_string()], "sort+dedup 应有去重");
    }

    /// BFS respects both the depth bound and the `max_nodes` cap (a node is only queued while under the cap, so
    /// over-long branches do not displace siblings), and `visited` prevents revisiting a node via a cycle.
    ///
    /// Depth contract: `depth` is the number of hops from the root, so `depth=1` yields the root **plus its
    /// immediate neighbours** (the intuitive reading — a UI "show depth-1 around this node" gets root + neighbours).
    #[test]
    fn subgraph_respects_depth_and_node_cap_and_visits_once() {
        let mut store = MemStore::default();
        store.nodes.insert(1, node(1, "A", "a"));
        store.nodes.insert(2, node(2, "A", "b"));
        store.nodes.insert(3, node(3, "A", "c"));
        store.nodes.insert(4, node(4, "A", "d"));
        store.edges = vec![edge(1, 1, 2, "E"), edge(2, 1, 3, "E"), edge(3, 2, 4, "E"), edge(4, 2, 1, "E")];

        let (s, _arc) = svc(store);

        let d1 = s.subgraph(NodeId::new(1), 1, 100).unwrap();
        let ids1: Vec<i64> = d1.nodes.iter().map(|n| n.id.get()).collect();
        assert_eq!(ids1, vec![1, 2, 3], "depth=1 含根 + 直接邻居");

        let d2 = s.subgraph(NodeId::new(1), 2, 100).unwrap();
        let ids2: Vec<i64> = d2.nodes.iter().map(|n| n.id.get()).collect();
        assert_eq!(ids2, vec![1, 2, 3, 4], "depth=2 扩展到第 2 环（孙子）");

        // max_nodes=2: node 3 is never queued because the cap is hit while expanding node 1's neighbors.
        let capped = s.subgraph(NodeId::new(1), 10, 2).unwrap();
        let capped_ids: Vec<i64> = capped.nodes.iter().map(|n| n.id.get()).collect();
        assert_eq!(capped_ids, vec![1, 2], "max_nodes 截断：3 不应入队");
        assert_eq!(capped.nodes.len(), 2);

        // The 2<->1 cycle must not produce a duplicate node.
        let cycled = s.subgraph(NodeId::new(1), 5, 100).unwrap();
        let mut seen = std::collections::HashSet::new();
        for n in &cycled.nodes {
            assert!(seen.insert(n.id.get()), "同一节点因环被重复访问");
        }
    }

    /// `subgraph` always uses `EdgeDirection::Both`, so a node reached only by an in-edge is still discovered at
    /// `depth=1` (a single-direction traversal would silently miss it).
    #[test]
    fn subgraph_traverses_in_edges_via_both_direction() {
        let mut store = MemStore::default();
        store.nodes.insert(5, node(5, "A", "five"));
        store.nodes.insert(6, node(6, "A", "six"));
        store.edges = vec![edge(1, 6, 5, "E")]; // only an in-edge into 5

        let (s, _arc) = svc(store);
        let g = s.subgraph(NodeId::new(5), 1, 100).unwrap();
        let ids: Vec<i64> = g.nodes.iter().map(|n| n.id.get()).collect();
        assert!(ids.contains(&5) && ids.contains(&6), "Both 方向必须沿入边发现 6");
    }

    /// A missing root yields an empty subgraph (no panic, no partial nodes).
    #[test]
    fn subgraph_missing_root_is_empty() {
        let (s, _arc) = svc(MemStore::default());
        let g = s.subgraph(NodeId::new(99), 3, 100).unwrap();
        assert!(g.nodes.is_empty());
        assert!(g.edges.is_empty());
    }

    /// `nodes` translates the optional `kind` / `name_contains` / `limit` / `offset` args into the `NodeFilter`
    /// the store receives — the one piece of logic in that pass-through.
    #[test]
    fn nodes_builds_node_filter() {
        let (s, arc) = svc(MemStore::default());
        let _ = s.nodes(ProjectId::new(1), Some("Class"), Some("foo"), 10, 5).unwrap();
        let f = arc.last_filter.lock().unwrap().clone().expect("query_nodes 应被调用");
        assert_eq!(f.project_id, ProjectId::new(1));
        assert_eq!(f.kind, Some(NodeKind("Class".to_string())));
        assert_eq!(f.name_contains, Some("foo".to_string()));
        assert_eq!(f.limit, Some(10));
        assert_eq!(f.offset, Some(5));
    }
}
