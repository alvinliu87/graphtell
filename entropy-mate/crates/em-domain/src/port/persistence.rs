//! 持久化端口。
//!
//! 按 **接口隔离原则** 拆成若干细粒度 trait，再由一个空的组合 trait
//! [`Persistence`] 提供 blanket impl，方便上层一次拿到全部能力。

use std::collections::HashMap;

use serde_json::Value;

use crate::error::Result;
use crate::model::{
    AliasEntry, Annotation, Diagnostic, Edge, NewAnnotation, NewEdge, NewNode, NewProject,
    NewSourceFile, NewSubProject, Node, NodeId, NodeKind, Project, ProjectId, ProjectPatch,
    ProjectStatus, SourceFile, SubProject, SubProjectId, SymbolEntry,
};
use crate::model::graph::NodeSummary;

/// 图的一次批量写入。
///
/// 流水线各阶段在内存中累积变更，阶段结束一次性落库 ——
/// 既保证原子性，也让领域逻辑保持纯粹（不依赖事务 API）。
#[derive(Debug, Clone, Default)]
pub struct GraphDelta {
    pub nodes: Vec<NewNode>,
    pub edges: Vec<NewEdge>,
    pub annotations: Vec<NewAnnotation>,
    pub aliases: Vec<AliasEntry>,
    pub symbols: Vec<SymbolEntry>,
    pub diagnostics: Vec<Diagnostic>,
    /// 为 `true` 时先清空该工程的图数据（重跑流水线用）。
    pub reset_project: bool,
    pub project_id: Option<ProjectId>,
    /// 合成节点在后续阶段被补充的属性（id → patch）。
    pub property_patches: Vec<(crate::model::NodeId, Value)>,
    /// 合成节点的"多处共现位置"追加（id → 位置）。
    pub location_patches: Vec<(crate::model::NodeId, crate::model::SourceLocation)>,
}

impl GraphDelta {
    pub fn new(project_id: ProjectId) -> Self {
        Self { project_id: Some(project_id), ..Default::default() }
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.edges.is_empty()
            && self.annotations.is_empty()
            && self.aliases.is_empty()
            && self.symbols.is_empty()
            && self.diagnostics.is_empty()
            && self.property_patches.is_empty()
            && self.location_patches.is_empty()
            && !self.reset_project
    }
    pub fn merge(&mut self, mut other: GraphDelta) {
        self.property_patches.append(&mut other.property_patches);
        self.location_patches.append(&mut other.location_patches);
        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
        self.annotations.extend(other.annotations);
        self.aliases.extend(other.aliases);
        self.symbols.extend(other.symbols);
        self.diagnostics.extend(other.diagnostics);
        self.reset_project |= other.reset_project;
    }
}

/// 工程读端口。
pub trait ProjectReader: Send + Sync {
    fn get_project(&self, id: ProjectId) -> Result<Option<Project>>;
    fn list_projects(&self) -> Result<Vec<Project>>;
    fn list_sub_projects(&self, project_id: ProjectId) -> Result<Vec<SubProject>>;
    fn list_files(&self, project_id: ProjectId, sub: Option<SubProjectId>) -> Result<Vec<SourceFile>>;
}

/// 工程写端口。
pub trait ProjectWriter: Send + Sync {
    fn create_project(&self, new: NewProject) -> Result<Project>;
    fn update_project(&self, id: ProjectId, patch: ProjectPatch) -> Result<Project>;
    fn delete_project(&self, id: ProjectId) -> Result<()>;
    fn set_project_status(&self, id: ProjectId, status: ProjectStatus) -> Result<()>;
    fn replace_sub_projects(
        &self,
        project_id: ProjectId,
        subs: Vec<NewSubProject>,
    ) -> Result<Vec<SubProject>>;
    fn update_sub_project_facts(&self, id: SubProjectId, facts: Value) -> Result<()>;
    /// 回填 P3 识别出的框架标识。
    fn set_sub_project_frameworks(&self, id: SubProjectId, frameworks: Vec<String>) -> Result<()>;
    fn replace_files(&self, project_id: ProjectId, files: Vec<NewSourceFile>)
        -> Result<Vec<SourceFile>>;
}

/// 图写入端口。
pub trait GraphSink: Send + Sync {
    /// 应用一批变更。
    fn apply(&self, delta: &GraphDelta) -> Result<()>;
}

/// 节点过滤条件。
#[derive(Debug, Clone, Default)]
pub struct NodeFilter {
    pub project_id: ProjectId,
    pub kind: Option<NodeKind>,
    pub name_contains: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// 图统计。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphStats {
    pub nodes: u64,
    pub edges: u64,
    pub annotations: u64,
    pub by_kind: std::collections::BTreeMap<String, u64>,
    /// 按 `properties.category` 统计（目前 `category` 与 `kind` 一致）。
    #[serde(default)]
    pub by_category: std::collections::BTreeMap<String, u64>,
}

/// 图查询端口。
pub trait GraphQuery: Send + Sync {
    fn query_nodes(&self, filter: &NodeFilter) -> Result<Vec<Node>>;
    fn get_node(&self, id: crate::model::NodeId) -> Result<Option<Node>>;
    /// 批量取节点（`id IN (...)`，内部分块）。
    ///
    /// 折叠视图要把每条边 `via` 链上所有节点的位置一次内联出来，一次请求涉及几十上百个
    /// 节点 —— 逐个 `get_node` 是 N+1 往返，实测占对象视图耗时近三分之一。
    fn get_nodes(&self, ids: &[crate::model::NodeId]) -> Result<HashMap<i64, Node>>;
    fn edges_of(
        &self,
        node: crate::model::NodeId,
        direction: EdgeDirection,
    ) -> Result<Vec<Edge>>;
    /// 工程全部节点的概要（id/kind/name/fqn/sub_project_id），供视图层一次预加载，
    /// 取代 BFS 里每个节点一次的 `get_node` 往返。返回 `id -> 概要` 便于 O(1) 查询。
    fn nodes_summary(&self, project_id: ProjectId) -> Result<HashMap<i64, NodeSummary>>;
    /// 批量取「出边」：返回 `from_id -> 边列表`，内部按 `from_id IN (...)` 分块查询。
    fn edges_outgoing(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>>;
    /// 批量取「入边」：返回 `to_id -> 边列表`，内部按 `to_id IN (...)` 分块查询。
    fn edges_incoming(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>>;
    /// 链式边邻接（仅 `from_id, to_id`，已过滤为调用链边），供候选打分 BFS 在内存里跑。
    /// 返回 `(outgoing: from_id -> [to_id], incoming: to_id -> [from_id])`，
    /// 一次性取整个工程的链边，避免逐节点查库、也避开完整 `Edge`（含 `properties` JSON）的沉重传输。
    fn chain_adjacency(
        &self,
        project_id: ProjectId,
    ) -> Result<(HashMap<i64, Vec<i64>>, HashMap<i64, Vec<i64>>)>;
    fn annotations_of(&self, node: crate::model::NodeId) -> Result<Vec<Annotation>>;
    fn stats(&self, project_id: ProjectId) -> Result<GraphStats>;
    /// 按主键取边（供"边证据链"查询）。
    fn find_edge(&self, id: crate::model::EdgeId) -> Result<Option<Edge>>;
    /// 取文件路径（供跳转定位）。
    fn file_path(&self, id: crate::model::FileId) -> Result<Option<String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDirection {
    Outgoing,
    Incoming,
    Both,
}

/// 权威符号表读端口。
pub trait SymbolTableReader: Send + Sync {
    fn get_symbol(&self, project_id: ProjectId, table: &str, key: &str) -> Result<Option<Value>>;
    fn list_symbols(&self, project_id: ProjectId, table: &str) -> Result<Vec<SymbolEntry>>;
}

/// 诊断落库端口。
pub trait DiagnosticSink: Send + Sync {
    fn push_diagnostics(&self, items: &[Diagnostic]) -> Result<()>;
    fn list_diagnostics(
        &self,
        project_id: ProjectId,
        limit: u32,
    ) -> Result<Vec<Diagnostic>>;
}

/// 组合端口：一次拿到全部持久化能力。
pub trait Persistence:
    ProjectReader
    + ProjectWriter
    + GraphSink
    + GraphQuery
    + SymbolTableReader
    + DiagnosticSink
    + Send
    + Sync
{
}

impl<T> Persistence for T where
    T: ProjectReader
        + ProjectWriter
        + GraphSink
        + GraphQuery
        + SymbolTableReader
        + DiagnosticSink
        + Send
        + Sync,
{
}
