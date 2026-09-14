//! 视图模型：视角（Perspective）注册表与各类视图的响应结构。
//!
//! # 为什么视角要由配置声明
//!
//! 「路由视角 / 表视角 / 领域聚合」这些是**分析范式**而不是后端固有概念。
//! 写死成 Rust enum 会让新增视角必须改代码；这里用一份 YAML 声明，
//! 后端只负责按声明去切片，新增视角 = 加一段配置（开闭原则）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{NodeId, ProjectId, SubProjectId};

/// 视角注册表。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewRegistry {
    /// 顶部一级筛选器的可选项。
    pub perspectives: Vec<PerspectiveSpec>,
    /// 节点种类 → 视角 id。点击某节点时据此切换一级筛选器。
    ///
    /// **未列入的种类不会切换顶部筛选器**，只打开右侧 Inspector ——
    /// 例如 `ConfigKey` / `KeyPattern` / `Component` / `SecretLocation`
    /// 这类资产/结构节点没有"单链路"语义。
    pub node_views: BTreeMap<String, String>,
}

impl ViewRegistry {
    pub fn by_id(&self, id: &str) -> Option<&PerspectiveSpec> {
        self.perspectives.iter().find(|p| p.id == id)
    }

    /// 某节点种类对应的视角（用于"点击即切"）。
    pub fn view_for_kind(&self, kind: &str) -> Option<&PerspectiveSpec> {
        self.node_views.get(kind).and_then(|id| self.by_id(id))
    }
}

/// 视角声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerspectiveSpec {
    pub id: String,
    pub label: String,
    /// 对象类（单链路）还是聚合类（概览）。
    pub mode: ViewMode,
    /// 对象类视角对应的节点种类。
    pub node_kind: Option<String>,
    /// 默认布局算法。
    pub layout: LayoutMode,
    /// 聚合视角的分组维度。
    pub group_by: Option<GroupBy>,
    /// 矩阵视角的行维度。
    pub row_from: Option<GroupBy>,
    /// 矩阵视角的列维度。
    pub col_from: Option<GroupBy>,
    /// 链路展开的跳数。
    pub depth: u32,
    pub description: Option<String>,
    /// 对象视角默认折叠（透传）的语法节点种类；折叠后只展示语义节点与它们之间的依赖边。
    #[serde(default)]
    pub collapsed_kinds: Vec<String>,
}

impl Default for PerspectiveSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            mode: ViewMode::Object,
            node_kind: None,
            layout: LayoutMode::Radial,
            group_by: None,
            row_from: None,
            col_from: None,
            depth: 2,
            description: None,
            collapsed_kinds: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// 对象类：**只渲染当前这一个对象**的链路子图。
    Object,
    /// 聚合类：不是单链路，而是聚合概览（聚类框 / 矩阵）。
    Aggregate,
}

/// 布局算法。**任何节点都不允许力导向自由漂移**：位置由算法确定，与交互无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutMode {
    /// 径向 / 同心环：环 = 跳数。对象入口子图默认。
    Radial,
    /// 分层（Sugiyama）：自上而下，正交折线。下钻调用链。
    Layered,
    /// 线性 Spine：一条链横排/竖排。污点取证。
    Spine,
    /// 聚类 Compound：大框套小节点。
    Compound,
    /// 矩阵：行列两维度，单元格为关系强度。
    Matrix,
    /// ER 正交：表关系。
    Er,
}

/// 分组维度。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    NodeKind,
    SubProject,
    /// 按节点属性分组（`property:domain`）。
    Property(String),
}

// ---------------------------------------------------------------- 响应

/// 源码位置三元组（path + symbol + line），用于跳转与防止行号漂移。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    /// 符号（类名 / 方法名 / 表名），行号漂移时用于重新定位。
    pub symbol: Option<String>,
    /// 说明，如 "Model 的 $table 定义"、"Db::name('store_order') 调用处"。
    pub note: Option<String>,
}

/// 视图中的节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub id: NodeId,
    pub kind: String,
    /// 语义节点的**类别**（目前与 `kind` 一致）；第一类语义节点等同于 `kind`，语法节点为 `None`。
    #[serde(default)]
    pub category: Option<String>,
    pub name: String,
    pub fqn: Option<String>,
    /// 距中心的跳数（0 = 中心）。
    pub ring: u32,
    pub sub_project_id: Option<SubProjectId>,
    /// 该节点是否有对应视角（决定"点击即切"是否可用）。
    pub has_own_view: bool,
    /// 该节点对应的**视角 id**（`node_views` 映射结果）。
    /// 点击时一级视角切到它、二级对象设为该节点；无对应视角时为 `null`。
    #[serde(default)]
    pub own_view: Option<String>,
    /// 跳转用的定义位置；合成节点会有**多个**。
    pub locations: Vec<SourceLocation>,
    /// 标注摘要（pii / auth.public / data.criticality …）。
    pub annotations: Vec<String>,
    /// 视角相关的度量（如入边数）。
    pub metrics: Value,
}

/// 视图中的边。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeView {
    pub id: i64,
    pub kind: String,
    pub from: NodeId,
    pub to: NodeId,
    /// 是否为已解析的实边；虚线 = 待验证假设。
    pub resolved: bool,
    pub confidence: f32,
    /// 虚线边经过的跳数（`via: 3 hops`）。
    pub hops: Option<u32>,
    /// 这条边**折叠掉的中间节点**（按"从起点到终点"排序）。
    ///
    /// 折叠视图里语义节点之间是"提拉"出来的直接边，中间其实经过了若干语法节点；
    /// 这里如实记录它们，前端才能在边上标 `via N 跳`、点击展开完整调用链 ——
    /// 不能让"看起来直连"骗人。为空表示图上确实是直接边。
    #[serde(default)]
    pub via: Vec<ViaNode>,
    /// 终点被"调用处"的位置（即 `via 最后一跳 → to` 这一跳的 CallSite）。
    /// 与每个 `ViaNode.call_site` 一起，让折叠链既显示"定义处"也显示"调用处"。
    #[serde(default)]
    pub to_call_site: Option<SourceLocation>,
}

/// 边上被折叠掉的中间节点（调用链的一环）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViaNode {
    pub id: NodeId,
    pub kind: String,
    pub name: String,
    /// 本跳被"调用处"的位置（即上一跳调用本节点的 CallSite）。
    /// 起点（`from`）不携带此项；其余每一跳都有"谁调了我"的位置。
    #[serde(default)]
    pub call_site: Option<SourceLocation>,
}

/// 被刻意隐藏的部分 —— 诚实性守门。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiddenInfo {
    /// 中心节点实际的邻居总数。
    pub total: usize,
    /// 本次视图画出来的邻居数。
    pub shown: usize,
    /// 未画出的邻居按节点种类统计。
    pub by_kind: BTreeMap<String, usize>,
    /// 人类可读说明。
    pub note: String,
}

/// 未解析记账。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedInfo {
    pub code: String,
    pub message: String,
    pub location: Option<String>,
}

/// 对象类视角：以**一个**对象为中心的链路子图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectView {
    pub project_id: ProjectId,
    pub perspective: String,
    pub layout: LayoutMode,
    pub center: NodeView,
    /// `rings[i]` = 距离中心 i+1 跳的节点。
    pub rings: Vec<Vec<NodeView>>,
    pub edges: Vec<EdgeView>,
    pub hidden: HiddenInfo,
    pub unresolved: Vec<UnresolvedInfo>,
    /// 视角专属结论（表视角=引用数/PII/关键度；路由视角=鉴权/死端点…）。
    pub conclusions: Value,
    /// 二级筛选器的候选对象列表。
    pub candidates: Vec<Candidate>,
}

/// 二级筛选器候选。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub id: NodeId,
    pub name: String,
    /// 供列表展示的次要信息（如引用数）。
    pub badge: Option<String>,
}

/// 聚合视角的聚类框。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub key: String,
    pub label: String,
    /// 框内节点数（可能很大，只给计数而不全画）。
    pub count: usize,
    /// 实际画出的样例节点（上限由请求决定）。
    pub members: Vec<NodeView>,
}

/// 矩阵视角。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixView {
    pub rows: Vec<String>,
    pub cols: Vec<String>,
    /// `cells[row][col]` = 关联数量。
    pub cells: Vec<Vec<u32>>,
    pub row_totals: Vec<u32>,
    pub col_totals: Vec<u32>,
}

/// 聚合类视角。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateView {
    pub project_id: ProjectId,
    pub perspective: String,
    pub layout: LayoutMode,
    pub clusters: Vec<Cluster>,
    pub matrix: Option<MatrixView>,
    pub hidden: HiddenInfo,
    pub unresolved: Vec<UnresolvedInfo>,
    pub conclusions: Value,
    /// 数据不足时的诚实说明（例如图里还没有 Domain 节点）。
    pub notice: Option<String>,
}

/// 边的证据链（"虚线是待验证假设"的落地方式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeEvidence {
    pub edge: EdgeView,
    /// `resolved=false` 时说明为什么没解析出来。
    pub reason: Option<String>,
    /// 证据位置：实边为单点，虚线边为途经的每个 CallSite。
    pub locations: Vec<SourceLocation>,
    /// 中间跳（若有）。
    pub via: Vec<String>,
}

/// 节点的定义位置列表（合成节点会有多个，绝不编造单一位置）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLocations {
    pub id: NodeId,
    pub kind: String,
    pub name: String,
    /// 是否合成节点（决定 UI 是否提示"多处共现"）。
    pub synthetic: bool,
    pub locations: Vec<SourceLocation>,
    /// 引用该节点的其它位置数量（用于"另有 N 处引用"）。
    pub reference_count: usize,
}
