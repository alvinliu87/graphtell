//! 图模型：节点、边、标注、别名、符号表。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{EdgeId, FileId, NodeId, ProjectId, SubProjectId};
use super::kinds::{AnnotationChannel, EdgeKind, NodeKind, Phase, SynthesizedKind};
use crate::model::kinds::Language;

/// 源码位置。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Span {
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// 图节点。
///
/// *语法节点* 由 CfAst 阶段从语言语法直接创建；
/// *合成节点* 由 Synthesize 阶段按 FKB 规则从语义汇聚而成（如 `Table`、`HttpContract`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub kind: NodeKind,
    /// 短名，如 `StoreOrderServices`。
    pub name: String,
    /// 完全限定名，如 `app\services\order\StoreOrderServices`。
    /// 合成节点没有 FQN，使用 [`Node::identity`]。
    pub fqn: Option<String>,
    /// 合成节点的幂等身份键；语法节点为 `None`。
    pub identity: Option<IdentityKey>,
    pub file_id: Option<FileId>,
    pub span: Span,
    pub language: Language,
    /// 产生该节点的阶段。
    pub phase: Phase,
    pub confidence: f32,
    /// 语言/框架相关的扩展字段（如 `I18nKey.texts`、`Table.columns`）。
    pub properties: Value,
}

impl Node {
    /// 用于展示的限定名：优先 FQN，其次 identity，最后短名。
    pub fn display_name(&self) -> String {
        if let Some(fqn) = &self.fqn {
            return fqn.clone();
        }
        if let Some(id) = &self.identity {
            return id.value.clone();
        }
        self.name.clone()
    }
}

/// 新建节点入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewNode {
    /// 由流水线分配的主键；`None` 表示交由数据库生成。
    pub id: Option<NodeId>,
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub kind: NodeKind,
    pub name: String,
    pub fqn: Option<String>,
    pub identity: Option<IdentityKey>,
    pub file_id: Option<FileId>,
    pub span: Span,
    pub language: Language,
    pub phase: Phase,
    pub confidence: f32,
    pub properties: Value,
}

impl NewNode {
    pub fn new(project_id: ProjectId, kind: NodeKind, name: impl Into<String>) -> Self {
        Self {
            id: None,
            project_id,
            sub_project_id: None,
            kind,
            name: name.into(),
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
}

/// 合成节点的**幂等身份键**。
///
/// 这是整个 Synthesize 阶段的核心：
/// 三条不同规则（`Db::name('store_order')`、Model 的 `$table` 属性、类名约定）
/// 只要算出相同的 `IdentityKey`，就必须合并成同一个节点。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdentityKey {
    /// 身份类型：`Fqn` / `Named` / `ContractId`，可扩展。
    pub kind: SynthesizedKind,
    /// 归一化后的值，如 `store_order`、`order.pay_success`、`POST /apple_login`。
    pub value: String,
}

impl IdentityKey {
    pub fn fqn(value: impl Into<String>) -> Self {
        Self { kind: SynthesizedKind(SynthesizedKind::FQN.to_string()), value: value.into() }
    }
    pub fn named(value: impl Into<String>) -> Self {
        Self { kind: SynthesizedKind(SynthesizedKind::NAMED.to_string()), value: value.into() }
    }
    pub fn contract(method: &str, path: &str) -> Self {
        Self {
            kind: SynthesizedKind(SynthesizedKind::CONTRACT_ID.to_string()),
            value: format!("{} {}", method.to_uppercase(), path),
        }
    }
    /// 用于数据库唯一索引与跨规则比对的字符串形式。
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, self.value)
    }
}

impl std::fmt::Display for IdentityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.key())
    }
}

/// 图边。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub project_id: ProjectId,
    pub kind: EdgeKind,
    pub from_id: NodeId,
    pub to_id: NodeId,
    pub phase: Phase,
    pub confidence: f32,
    pub properties: Value,
}

/// 节点概要（供视图层一次性预加载，取代逐个 `get_node` 往返）。
#[derive(Debug, Clone)]
pub struct NodeSummary {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub fqn: Option<String>,
    pub sub_project_id: Option<i64>,
}

/// 新建边入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEdge {
    pub project_id: ProjectId,
    pub kind: EdgeKind,
    pub from_id: NodeId,
    pub to_id: NodeId,
    pub phase: Phase,
    pub confidence: f32,
    pub properties: Value,
}

impl NewEdge {
    pub fn new(project_id: ProjectId, kind: EdgeKind, from_id: NodeId, to_id: NodeId) -> Self {
        Self {
            project_id,
            kind,
            from_id,
            to_id,
            phase: Phase(Phase::CF_AST.to_string()),
            confidence: 1.0,
            properties: Value::Null,
        }
    }
}

/// 标注合并策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum MergeStrategy {
    /// 同 kind 只保留 confidence 最大的一条。
    MaxByKind,
    /// 同 kind 可共存（例如一个类既被 `where` 净化、又被 `whereRaw` 污染）。
    Coexist,
    /// 累积到数组（capability 投影 / texts 合并）。
    Accumulate,
    /// 直接覆盖。
    Replace,
}

impl Default for MergeStrategy {
    fn default() -> Self {
        MergeStrategy::MaxByKind
    }
}

/// 标注：挂在节点上的语义标签。
///
/// 分通道（[`AnnotationChannel`]）存放，互不干扰 —— 一个节点可以同时有
/// `FkbMark(pii.phone)`、`Taint(sink:RawSql)`、`Alias(event_name)`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: i64,
    pub node_id: NodeId,
    pub channel: AnnotationChannel,
    pub kind: String,
    pub subkind: Option<String>,
    pub confidence: f32,
    /// 证据：`{ hook, location, source }`。
    pub evidence: Value,
    pub phase: Phase,
}

/// 新建标注入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAnnotation {
    pub node_id: NodeId,
    pub channel: AnnotationChannel,
    pub kind: String,
    pub subkind: Option<String>,
    pub confidence: f32,
    pub evidence: Value,
    pub phase: Phase,
    pub merge: MergeStrategy,
}

/// 别名索引条目（P3/P6 注册，P7 解析时查询）。
///
/// 支持复合键：`(namespace, key, qualifier)`。
/// 例如获取器必须是 `("accessor", "status_text", Some("app\\model\\order\\StoreOrder"))`，
/// 否则 `StoreOrder::status_text` 与 `User::status_text` 会撞名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasEntry {
    pub project_id: ProjectId,
    pub namespace: String,
    pub key: String,
    pub qualifier: Option<String>,
    pub node_id: NodeId,
    pub confidence: f32,
    pub evidence: Value,
}

/// 权威符号表条目（P3 装载，供 P5/P6/P7 查询）。
///
/// 例如 `schema`（203 张表及其列）、`config_keys`、`i18n`、`facade_map`、`route_list`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub project_id: ProjectId,
    /// 表名，如 `schema` / `config_keys` / `i18n`。
    pub table: String,
    /// 表内主键。
    pub key: String,
    /// 任意 JSON 载荷。
    pub value: Value,
}

/// 流水线诊断信息（冲突、缺失、未知解析等）。
///
/// 设计为**一等产物**：`AnnotateTargetMissing`、`UnresolvedLink` 等
/// 本身就是有价值的发现（例如"路由指向不存在的 handler"= 点击即 500）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub project_id: ProjectId,
    pub phase: Phase,
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub location: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
    Critical,
}

/// 单次流水线运行的统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhaseReport {
    pub phase: String,
    pub nodes_created: u64,
    pub edges_created: u64,
    pub annotations_created: u64,
    pub aliases_created: u64,
    pub diagnostics: Vec<Diagnostic>,
    pub duration_ms: u64,
}
