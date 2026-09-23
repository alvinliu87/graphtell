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
    ProjectRuleConfig, ProjectStatus, SourceFile, SubProject, SubProjectId, SymbolEntry,
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
    ///
    /// 节点 id 由流水线按工程分段分配（`gt_pipeline::NODE_ID_STRIDE` 的设计约定），
    /// 因此这里不需要"当前最大 id"之类的全局协调：并发建图也不会互相覆盖。
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
    /// 返回 `(outgoing: from_id -> [to_id], incoming: to_id -> [from_id], semantic_incoming: to_id -> [from_id])`，
    /// 一次性取整个工程的链边，避免逐节点查库、也避开完整 `Edge`（含 `properties` JSON）的沉重传输。
    ///
    /// 第三份 `semantic_incoming` **只含语义边**（见 [`crate::model::kinds::is_semantic_edge`]）：
    /// 徽标里的"入边"必须按语义口径计数 —— 链边里的 `Calls` / `HasCallSite` 是语法调用边，
    /// 把它们数进"入边 N"会让数字既不与画布（只画语义边）对得上，也不承载业务含义。
    fn chain_adjacency(
        &self,
        project_id: ProjectId,
    ) -> Result<(
        HashMap<i64, Vec<i64>>,
        HashMap<i64, Vec<i64>>,
        HashMap<i64, Vec<i64>>,
    )>;
    /// 工程内出现过的全部**边种类**。
    ///
    /// 规则引擎用它校验判据是否成立（见 [`crate::model::RuleRequirements`]）：
    /// 图上从来没有 `Triggers` 边时，`no_incoming: Triggers` 对每个节点都成立，
    /// 会把全部事件节点报成"没人触发"。跑之前先确认这个边种类真的存在过。
    fn edge_kinds(&self, project_id: ProjectId) -> Result<Vec<String>>;
    /// 工程内出现过的全部标注 `(channel, kind)`。
    ///
    /// 与 [`Self::edge_kinds`] 同理，用于挡住"标注压根没产出"导致的恒真误报；
    /// 带上 `channel` 是因为 `NoCapability` 只认 `Capability` 通道的标注。
    fn annotation_kinds(&self, project_id: ProjectId) -> Result<Vec<(String, String)>>;
    fn annotations_of(&self, node: crate::model::NodeId) -> Result<Vec<Annotation>>;
    /// 批量取工程全部标注（`node_id -> 标注列表`）。
    ///
    /// 规则引擎要对成千上万个节点判断 `HasAnnotation` / `NoAnnotation`，
    /// 逐节点 `annotations_of` 是 N+1 往返（实测万级节点即秒级抖动）。
    /// 一次预装载后，规则求值全程在内存里完成。
    fn annotations_of_project(
        &self,
        project_id: ProjectId,
    ) -> Result<HashMap<i64, Vec<Annotation>>>;
    /// 批量取工程内全部源文件路径（`file_id -> path`）。
    ///
    /// 违规与召回都要把节点还原成 `path:line`；同样是避免逐文件往返。
    fn file_paths(&self, project_id: ProjectId) -> Result<HashMap<i64, String>>;
    fn stats(&self, project_id: ProjectId) -> Result<GraphStats>;
    /// 按「种类 + 端（side）」统计节点数（比 `query_nodes` 轻量，只 `COUNT`）。
    /// 视角层用它给带 `side` 过滤的视角（如前端本地存储 / 后端缓存拆分）算候选数量。
    ///
    /// `side` 为 `None` 时不限制端；`kind` 为 `None` 时不限制种类。
    fn count_nodes(
        &self,
        project_id: ProjectId,
        kind: Option<&NodeKind>,
        side: Option<&str>,
    ) -> Result<u64>;
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
    /// 列诊断，**排除**给定 code 前缀。
    ///
    /// 与 [`DiagnosticSink::count_diagnostics_excluding`] 必须成对使用：
    /// 页面上的角标按"排除 rule:"统计，列表却按"全部"取的话，
    /// 用户会看到角标写着 0 error、表格里却全是别的东西 —— 两个口径不一致
    /// 比任何一条具体错误都更能摧毁信任。
    fn list_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
        limit: u32,
    ) -> Result<Vec<Diagnostic>>;
    /// 按 code 前缀列出诊断（如 `rule:` 取全部规则违规）。
    ///
    /// `sub_project_id` 为 `Some(ids)` 时只返回归属这些子工程的诊断，
    /// 且**共享诊断（`sub_project_id IS NULL`，如跨子工程的表/队列）始终保留**——
    /// 与图视图的"共享节点在任一过滤下都显示"语义一致。为 `None` 时不限制。
    fn list_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
        limit: u32,
    ) -> Result<Vec<Diagnostic>>;
    /// 按 code 前缀清理诊断，返回删除条数。
    ///
    /// 规则可反复执行，若不清理上一轮的 `rule:*` 违规，诊断表会无限堆积、
    /// 且用户看到的会是"历史结论"而非当前代码的结论。
    fn clear_diagnostics(&self, project_id: ProjectId, code_prefix: &str) -> Result<u64>;
    /// 按 code 前缀 + 严重度聚合计数，返回 `(severity_snake_case, count)` 列表。
    ///
    /// 用于菜单角标这类轻量汇总：不拉全量违规，只取分组计数。
    fn count_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
    ) -> Result<Vec<(String, u64)>>;
    /// 按 code **排除**某前缀 + 严重度聚合计数，返回 `(severity_snake_case, count)` 列表。
    ///
    /// 诊断页展示的是"非规则"的诊断（根缺失、断链、identity 冲突等），
    /// 规则违规已单独归到合规检查，这里排除 `rule:` 前缀避免重复计数。
    fn count_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
    ) -> Result<Vec<(String, u64)>>;
}

/// 工程级规则配置端口（按工程覆盖规则的启用态与参数）。
pub trait RuleConfigStore: Send + Sync {
    /// 取某工程全部规则配置覆盖（key = rule_id）。
    fn get_rule_configs(
        &self,
        project_id: ProjectId,
    ) -> Result<std::collections::HashMap<String, ProjectRuleConfig>>;
    /// 写入（或清除）单条规则配置。覆盖为空（enabled=None 且 options 为空）= 删除该行。
    fn set_rule_config(&self, cfg: &ProjectRuleConfig) -> Result<()>;
    /// 删除某工程单条规则的配置（恢复继承默认）。
    fn delete_rule_config(&self, project_id: ProjectId, rule_id: &str) -> Result<()>;
}

/// 组合端口：一次拿到全部持久化能力。
pub trait Persistence:
    ProjectReader
    + ProjectWriter
    + GraphSink
    + GraphQuery
    + SymbolTableReader
    + DiagnosticSink
    + RuleConfigStore
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
        + RuleConfigStore
        + Send
        + Sync,
{
}
