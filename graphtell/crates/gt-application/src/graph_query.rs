//! 图查询用例（供 UI 浏览与检索）。

use std::sync::Arc;

use gt_domain::error::Result;
use gt_domain::model::{
    Annotation, Diagnostic, Edge, Node, NodeId, NodeKind, ProjectId, RULE_CODE_PREFIX,
    SymbolEntry,
};
use gt_domain::port::{EdgeDirection, GraphStats, NodeFilter, Persistence};
use serde::{Deserialize, Serialize};

/// 图查询服务。
pub struct GraphQueryService {
    store: Arc<dyn Persistence>,
}

/// 非规则诊断的严重度汇总（菜单角标用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DiagnosticSummary {
    pub critical: u64,
    pub error: u64,
    pub warning: u64,
    pub info: u64,
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

    /// **建图期**诊断（根缺失、断链、identity 冲突等），排除合规违规。
    ///
    /// 必须排除 `rule:` 前缀，且与 [`Self::diagnostics_summary`] 同口径：
    /// 建图诊断与合规违规混在同一张表、只靠 `LIMIT` 截断的话，
    /// 谁露出来取决于**写入顺序**（合规违规后写、id 更大，会把建图诊断整段挤走），
    /// 于是页面出现"角标 0 error、表格里也是一堆不相关的东西"。
    pub fn diagnostics(&self, project_id: ProjectId, limit: u32) -> Result<Vec<Diagnostic>> {
        self.store
            .list_diagnostics_excluding(project_id, RULE_CODE_PREFIX, limit)
    }

    /// 非规则诊断的严重度汇总（菜单角标用）。
    ///
    /// 诊断页展示的是"非规则"诊断（根缺失、断链、identity 冲突等），
    /// 规则违规已单独归到合规检查，这里排除 `rule:` 前缀避免重复计数。
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
        Ok(s)
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
