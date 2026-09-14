//! 框架知识库（FKB）的领域模型。
//!
//! FKB 是「预置框架知识」的可序列化形式：ThinkPHP 6、Uni-app、Laravel、CRMEB…
//! 每个框架一份 YAML，描述
//! * 如何**识别**该框架（[`Detector`]）
//! * 如何**解析框架根**（[`RootRule`]，如 `composer.json` 的 `autoload.psr-4`）
//! * P3 要装载哪些**权威符号表**（[`LoaderSpec`]）
//! * 各阶段执行的**规则**（[`Rule`] = 选择器 + 绑定）
//!
//! 全部数据驱动，内核不认识任何具体框架 —— 这是 **开闭原则** 与
//! **依赖倒置** 的落点：新增框架只需加一份 YAML。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::graph::{MergeStrategy, Span};
use super::kinds::{AnnotationChannel, EdgeKind, NodeKind, Phase, SynthesizedKind};
use crate::model::kinds::Language;

/// 一份框架知识。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameworkKnowledge {
    pub id: String,
    pub display_name: String,
    pub language: Language,
    /// 适用版本提示，仅用于展示。
    pub version_hint: Option<String>,
    /// 识别信号。
    pub detectors: Vec<Detector>,
    /// 框架根 / 关键路径解析规则。
    pub root_rules: Vec<RootRule>,
    /// P3 权威符号表装载器。
    pub loaders: Vec<LoaderSpec>,
    /// 分阶段规则。
    pub rules: Vec<Rule>,
    /// P7 动态解析声明：哪些调用是容器解析 / 事件触发 / 门面调用。
    pub resolvers: Vec<ResolverSpec>,
    /// 缺省排除目录（叠加在工程/语言默认规则之上）。
    pub exclude_globs: Vec<String>,
}

/// 框架识别信号。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Detector {
    /// manifest 文件里存在某个依赖。
    ManifestDependency {
        manifest: String,
        dependency: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
    /// 存在某个特征文件/目录。
    FileExists {
        path: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
}

fn default_conf() -> f32 {
    0.9
}

/// 框架根解析规则。
///
/// 例：从 `composer.json` 的 `autoload.psr-4` 解析出 `AppRoot = "app"`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootRule {
    pub id: String,
    /// 产出的事实键，如 `app_root`。
    pub key: String,
    pub source: RootSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
    /// 兜底候选目录；解析失败时按顺序探测。
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootSource {
    /// 从 JSON manifest 的某个指针取值。
    ManifestJson {
        manifest: String,
        /// 点分路径，如 `autoload.psr-4`。
        pointer: String,
        /// 取值策略。
        pick: PickStrategy,
    },
    /// 直接探测目录是否存在。
    DirectoryExists { path: String },
    /// 从 PHP 配置文件（如 ThinkPHP 的 `config/database.php`）按点分指针取值。
    ///
    /// 用于自动探测工程级配置（如表前缀），避免把项目特定约定写死在 FKB。
    ManifestPhp {
        /// 相对工程根的路径，如 `config/database.php`。
        manifest: String,
        /// 点分路径，如 `connections.mysql.prefix`。
        pointer: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickStrategy {
    /// 取所有映射目录中最浅的一个。
    ShallowestDir,
    /// 取第一个映射目录。
    FirstDir,
    /// 取键名等于指定值的映射目录。
    ByNamespaceKey,
}

/// P3 符号表装载器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderSpec {
    pub id: String,
    /// 输出符号表名：`schema` / `config_keys` / `i18n` / `facade_map` / `route_list` 等。
    pub table: String,
    pub from: LoaderSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoaderSource {
    /// 读取单个文件并按 key_path 取值。
    File {
        path: String,
        #[serde(default)]
        key_path: Option<String>,
        #[serde(default)]
        format: FileFormat,
    },
    /// 按 glob 批量读取（如 `lang/*/*.php`），可用捕获组提取 locale。
    Glob {
        pattern: String,
        /// 从路径捕获组中提取 locale 的正则（第一个捕获组）。
        #[serde(default)]
        locale_regex: Option<String>,
        #[serde(default)]
        format: FileFormat,
    },
    /// FKB 内联声明的常量表（如 FacadeMap —— 由框架知识给出，不靠猜）。
    Inline { rows: Vec<Value> },
    /// 内置装载器（由流水线实现，如从 PHP 源码收集 `$table` 与 `Db::name`）。
    Builtin {
        name: String,
        #[serde(default)]
        params: Value,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    Php,
    Json,
    Yaml,
    Sql,
    Text,
    #[default]
    Auto,
}

/// 一条规则：在某阶段，对匹配到的目标执行一组动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub phase: Phase,
    pub selector: Selector,
    pub binding: Vec<Action>,
    #[serde(default = "default_conf")]
    pub confidence: f32,
}

/// 选择器：决定规则作用于什么。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selector {
    /// 调用点，如 `Db::name('store_order')`。
    Call {
        /// callee 匹配模式，支持 `|` 分隔多模式与 `*` 通配：
        /// `think\facade\Db::name|*:where|Db::raw`
        #[serde(default)]
        callee: Option<String>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// 继承 / 实现 / trait。
    Inheritance {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        with_property: Option<String>,
    },
    /// 配置文件条目。
    ConfigEntry {
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        key_path: Option<String>,
    },
    /// 语法声明。
    Declaration {
        #[serde(default)]
        node_kind: Option<NodeKind>,
        #[serde(default)]
        fqn_matches: Option<String>,
    },
    /// **图上的节点**（P6 专用：选择器是节点而非源码）。
    Node {
        #[serde(default)]
        node_kind: Option<NodeKind>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// P7 动态解析：容器 make / 事件触发 / 门面调用 / 获取器等。
    Dynamic {
        #[serde(default)]
        call: Option<String>,
        #[serde(default)]
        channel: Option<String>,
    },
}

/// 谓词（`where` 条件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    /// 目标（类）拥有某属性。
    HasProperty(String),
    /// 权威符号表中该键存在。
    InSymbolTable { table: String, key_of: ValueSource },
    /// 权威符号表的列命中给定名字之一。
    ColumnsMatch { table: String, names: Vec<String> },
    /// 节点上已有某标注。
    HasAnnotation { kind: String },
    /// 作用域链上不存在给定能力。
    NoneOfCapability(Vec<String>),
    /// i18n 是否存在缺失 locale。
    HasMissing(bool),
    /// fan_in 不小于阈值。
    FanInGte(u64),
    /// 参数个数等于给定值。
    ArgCount(usize),
    /// 节点名（或 identity 值）包含给定子串（大小写不敏感）。
    NameMatches(String),
    /// 节点属性等于给定值。
    PropertyIs { name: String, value: String },
}

/// 绑定动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Action {
    /// 打标注。
    Annotate(AnnotateAction),
    /// 合成节点。
    Synthesize(SynthesizeAction),
    /// 仅建边。
    Link(LinkAction),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotateAction {
    /// 期望执行的阶段（Pre / Post）。当规则 phase 与此处不一致时跳过。
    pub phase: Option<Phase>,
    pub channel: AnnotationChannel,
    pub target: AnnotateTarget,
    pub annotations: Vec<AnnotationSpec>,
    pub merge: MergeStrategy,
    /// 作用域：`[RouteSelf, EnclosingGroup, Global]`。
    pub scope: Option<Vec<String>>,
    pub r#where: Vec<Predicate>,
    /// 从作用域链继承能力时的置信度衰减。
    pub confidence_scale: Option<f32>,
}

impl Default for AnnotateAction {
    fn default() -> Self {
        Self {
            phase: None,
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            target: AnnotateTarget::Matched,
            annotations: Vec::new(),
            merge: MergeStrategy::MaxByKind,
            scope: None,
            r#where: Vec::new(),
            confidence_scale: None,
        }
    }
}

/// 标注目标。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotateTarget {
    /// 选择器直接匹配到的节点。
    Matched,
    /// 从匹配结果的某个字段派生（如 `array_values` → 解析成类）。
    FromField {
        source: ValueSource,
        resolve: Option<ResolveAs>,
    },
    /// 引用本规则此前合成出的节点。
    SynthesizedRef(String),
}

impl Default for AnnotateTarget {
    fn default() -> Self {
        AnnotateTarget::Matched
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotationSpec {
    pub kind: String,
    pub subkind: Option<SubkindSource>,
    pub severity: Option<String>,
    pub confidence: f32,
    pub evidence: Option<Value>,
    pub channel: Option<AnnotationChannel>,
}

impl Default for AnnotationSpec {
    fn default() -> Self {
        Self {
            kind: String::new(),
            subkind: None,
            severity: None,
            confidence: 1.0,
            evidence: None,
            channel: None,
        }
    }
}

/// subkind 的来源：字面量 / 权威符号表 / 计算值 / fan_in 分级。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubkindSource {
    Literal(String),
    FromSymbolTable {
        table: String,
        field: String,
        #[serde(default)]
        of: Option<ValueSource>,
    },
    FromFanIn {
        thresholds: FanInThresholds,
    },
    Computed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanInThresholds {
    pub high: u64,
    pub medium: u64,
    #[serde(default)]
    pub low_label: Option<String>,
    #[serde(default)]
    pub medium_label: Option<String>,
    #[serde(default)]
    pub high_label: Option<String>,
}

/// 合成动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SynthesizeAction {
    /// 节点种类（开放字符串，如 `Table` / `HttpContract` / `ExternalSystem`）。
    ///
    /// 若同时给了 `subtype`，则**子类型提升为 kind**（`kind = subtype`），
    /// 而此处的 `node` 记为节点的 `category`（伞形名，如 `ExternalSystem`）。
    pub node: NodeKind,
    /// 子类型（`Event` / `Queue` / `Cache`…），可选；给了就作为最终 kind。
    pub subtype: Option<String>,
    pub identity: IdentitySpec,
    pub fields: Vec<FieldSpec>,
    pub link: Option<LinkSpec>,
    pub confidence: f32,
    /// `MergeBy(key)` —— 多份数据源合并成一个节点的不同字段，而不是建多个节点。
    pub modifiers: Vec<String>,
    /// 别名注册（合成后自动写入 by_alias）。
    pub alias: Option<AliasSpec>,
}

impl Default for SynthesizeAction {
    fn default() -> Self {
        Self {
            node: NodeKind(NodeKind::UNKNOWN.to_string()),
            subtype: None,
            identity: IdentitySpec::default(),
            fields: Vec::new(),
            link: None,
            confidence: 0.9,
            modifiers: Vec::new(),
            alias: None,
        }
    }
}

/// 合成节点的身份规格。
///
/// **`identity` 是整个 Synthesize 阶段的核心**：三条不同的规则只要算出
/// 相同的 identity，产出就会幂等合并成一个节点。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentitySpec {
    /// `Fqn` / `Named` / `ContractId`。
    pub kind: SynthesizedKind,
    /// 单值身份（`Fqn` / `Named`）。
    pub value: Option<ValueSource>,
    /// `ContractId` 的 HTTP 方法来源。
    pub method: Option<ValueSource>,
    /// `ContractId` 的路径来源。
    pub path: Option<ValueSource>,
    #[serde(default)]
    pub normalize: Vec<NormalizeStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct FieldSpec {
    pub name: String,
    pub value: Option<ValueSource>,
    /// 累积合并：`{ key: locale, value: text }`。
    pub accumulate: Option<AccumulateSpec>,
    /// 从权威符号表补充字段。
    pub from_symbol_table: Option<SymbolFieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccumulateSpec {
    pub key: ValueSource,
    pub value: ValueSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolFieldSpec {
    pub table: String,
    pub field: String,
    #[serde(default)]
    pub of: Option<ValueSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct LinkSpec {
    pub kind: EdgeKind,
    /// 边的另一端来源（如 handler 字符串）。
    pub to: Option<ValueSource>,
    /// 方向：incoming（来源指向新节点）/ outgoing（新节点指向来源）/ to_target。
    pub direction: Direction,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// 匹配到的调用方 ——> 新合成节点。
    #[default]
    Incoming,
    /// 新合成节点 ——> 匹配到的调用方。
    Outgoing,
    /// 新合成节点 ——> `to` 解析出的目标。
    ToTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AliasSpec {
    pub namespace: String,
    pub key: ValueSource,
    pub qualifier: Option<ValueSource>,
}

/// 值来源（结构化，便于 YAML 书写）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ValueSource {
    /// 第 n 个实参。
    pub arg: Option<usize>,
    /// 实参是对象字面量时取其字段（如 `uni.request({url:..})`）。
    pub field: Option<String>,
    /// 取类属性（如 Model 的 `$table`）。
    pub property: Option<String>,
    /// 取当前节点自身（类短名 / FQN）。
    #[serde(rename = "self")]
    pub self_value: Option<bool>,
    /// 跳过 key_path 的前 n 段后剩余的部分（点分连接）。
    pub path_segment: Option<usize>,
    /// 调用点的方法名（如 `Route::post` 的 `post`）。
    pub method_name: Option<bool>,
    /// 配置条目的值本身。
    pub entry_value: Option<bool>,
    /// 取数组的全部值（一对多）。
    pub array_values: Option<bool>,
    /// 取数组长度。
    pub array_length: Option<bool>,
    /// 取配置文件条目的 key_path。
    pub key_path: Option<bool>,
    /// 取文件名主干。
    pub file_stem: Option<bool>,
    /// 取当前 locale（i18n 装载时）。
    pub locale: Option<bool>,
    /// 字面量。
    pub literal: Option<String>,
    /// 嵌套来源：`{ source: { arg: 1 }, field: 'url' }`。
    pub source: Option<Box<ValueSource>>,
    /// 变换（如 `class_to_topic`、`snake_plural`）。
    pub transform: Option<TransformSpec>,
    /// 归一化链。
    pub normalize: Option<Vec<NormalizeStep>>,
    /// 解析方式。
    pub resolve: Option<ResolveAs>,
    /// 取不到时的默认值。
    pub default: Option<String>,
    /// 多段拼接：`{ path: [{file_stem:true},{key_path:true}], join: '.' }`。
    pub path: Option<Vec<ValueSource>>,
    pub join: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TransformSpec {
    pub snake_plural: Option<bool>,
    pub snake: Option<bool>,
    pub strip_namespace: Option<bool>,
    pub class_to_topic: Option<bool>,
    pub lower: Option<bool>,
    pub upper: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAs {
    /// `Foo::class` → 完全限定类名 → 查 by_name。
    ClassConst,
    /// `'Login/appleLogin'` → 按 控制器/方法 模式拼出 FQN。
    HandlerPattern,
    /// 查 by_alias 索引。
    ByAlias,
    /// 直接当作名字使用。
    AsIs,
}

/// 归一化步骤（identity 幂等合并的关键）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizeStep {
    StripPrefix(Vec<String>),
    Lower,
    Upper,
    /// 保证以 `/` 开头。
    LeadingSlash,
    /// 复数转单数。
    Singularize,
    /// 类名转 `snake_case` 复数（Model 约定表名）。
    SnakePlural,
    /// 去掉命名空间，只留最后一段。
    StripNamespace,
    Trim,
    Replace { from: String, to: String },
}

/// 仅建边的动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct LinkAction {
    pub kind: EdgeKind,
    pub from: Option<ValueSource>,
    pub to: Option<ValueSource>,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

/// P7 解析漏斗的层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveTier {
    /// L1 字面 FQN，如 `app()->make(StoreOrderServices::class)`。
    Exact = 1,
    /// L2 容器注册表（`provider.php` 中的绑定）。
    Registry = 2,
    /// L3 别名索引（Facade / 事件名 / 获取器）。
    Alias = 3,
    /// L4 约定（命名空间拼接、类名推导表名）。
    Convention = 4,
    /// L5 常量传播。
    ConstProp = 5,
    /// L6 与有限全集求交（schema 的 203 张表）。
    Intersection = 6,
    /// L7 完全未知。
    Unknown = 7,
}

impl ResolveTier {
    /// 该层级对应的基础置信度。
    pub fn base_confidence(self) -> f32 {
        match self {
            Self::Exact => 1.0,
            Self::Registry => 0.95,
            Self::Alias => 0.85,
            Self::Convention => 0.8,
            Self::ConstProp => 0.6,
            Self::Intersection => 0.7,
            Self::Unknown => 0.3,
        }
    }
}

/// 一次动态解析的产物。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub tier: ResolveTier,
    /// 候选节点；空表示未解析。
    pub candidates: Vec<super::ids::NodeId>,
    pub confidence: f32,
    pub evidence: String,
}

impl Resolution {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            tier: ResolveTier::Unknown,
            candidates: Vec::new(),
            confidence: ResolveTier::Unknown.base_confidence(),
            evidence: reason.into(),
        }
    }
    pub fn resolved(tier: ResolveTier, candidate: super::ids::NodeId, evidence: impl Into<String>) -> Self {
        Self { tier, candidates: vec![candidate], confidence: tier.base_confidence(), evidence: evidence.into() }
    }
}

/// 调用点上下文（供选择器匹配使用）。
#[derive(Debug, Clone)]
pub struct CallContext {
    pub owner_fqn: String,
    pub owner_node: Option<super::ids::NodeId>,
    pub callee_text: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<super::syntax::FactValue>,
    pub span: Span,
    pub sub_project: Option<super::ids::SubProjectId>,
    pub file_path: String,
}

/// P7 动态解析声明。
///
/// 让「哪些调用需要动态解析」这件事也由 FKB 决定，而不是写死在内核里。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolverSpec {
    pub id: String,
    /// 匹配模式，如 `app()->make|app|make`。
    #[serde(default)]
    pub call: Option<String>,
    pub strategy: ResolveStrategy,
    /// 起始解析层级（容器默认 Regist发ry）。
    #[serde(default)]
    pub from_tier: Option<ResolveTier>,
}

/// 具体解析策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStrategy {
    /// `app()->make(X)` / `app('x')`：L1 字面 → L2 注册表 → L4 约定 → L6 求交。
    Container,
    /// `event('x')`：查 L3 别名索引。
    Event,
    /// `think\facade\Cache::get()`：查 L3 FacadeMap。
    Facade,
    /// `$order->status_text`：复合键 accessor 别名。
    Accessor,
    /// `Route::post('p','Login/appleLogin')`：handler 模式解析。
    Handler,
    /// `$services->appAuth()`：按变量类型解析实例方法调用。
    ///
    /// 类型来源：方法参数类型提示（ThinkPHP 控制器 DI 约定）与构造器属性注入
    /// （`__construct(T $x){ $this->p = $x; }`），由 P2 记录、本策略消费。
    VariableType,
}
