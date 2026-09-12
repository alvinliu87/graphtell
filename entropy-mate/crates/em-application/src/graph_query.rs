//! 图查询用例（供 UI 浏览与检索）。

use std::sync::Arc;

use em_domain::error::Result;
use em_domain::model::{
    Annotation, Diagnostic, Edge, Node, NodeId, NodeKind, ProjectId, SymbolEntry,
};
use em_domain::port::{EdgeDirection, GraphStats, NodeFilter, Persistence};
use serde::Serialize;

/// 图查询服务。
pub struct GraphQueryService {
    store: Arc<dyn Persistence>,
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

    pub fn diagnostics(&self, project_id: ProjectId, limit: u32) -> Result<Vec<Diagnostic>> {
        self.store.list_diagnostics(project_id, limit)
    }

    /// 子图（BFS，限制规模，供图形化展示）。
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

/// 子图快照。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
