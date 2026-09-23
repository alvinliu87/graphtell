//! 可扩展的"种类"值对象：`NodeKind` / `EdgeKind` / `Phase` / `AnnotationChannel`。
//!
//! # 为什么用 `String` newtype 而不是 `enum`
//!
//! GraphTell 的目标是兼容所有主流技术栈。语言、框架与阶段都会随 FKB
//! （Framework Knowledge Base，YAML）不断扩展，若写死成 Rust `enum`，
//! 每增加一种节点类型都要重新编译内核 —— 这违反了 **开闭原则（OCP）**。
//!
//! 因此这里采用「开放字符串 + 常量速记」：常量只是**已知种类的文档**，
//! 不是限制；FKB 可以自由引入新种类。

macro_rules! declare_open_kind {
    ($name:ident => $doc:literal; $($const:ident = $lit:literal => $cdoc:literal),* $(,)?) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            $(
                #[doc = $cdoc]
                pub const $const: &'static str = $lit;
            )*

            pub fn new<S: Into<String>>(s: S) -> Self { Self(s.into()) }

            pub fn as_str(&self) -> &str { &self.0 }

            /// 是否为某个已知种类。
            pub fn is(&self, other: &str) -> bool { self.0 == other }

            /// 大小写不敏感比较（FKB 里常写小写）。
            pub fn eq_ignore_ascii_case(&self, other: &str) -> bool {
                self.0.eq_ignore_ascii_case(other)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_string()) }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl Default for $name {
            fn default() -> Self { Self("Unknown".to_string()) }
        }
    };
}

declare_open_kind! { NodeKind => "图节点种类（语法节点 + 合成节点，开放可扩展）";
    FILE       = "File"       => "源文件",
    DIRECTORY  = "Directory"  => "目录",
    NAMESPACE  = "Namespace"  => "命名空间 / 模块",
    CLASS      = "Class"      => "类",
    INTERFACE  = "Interface"  => "接口",
    TRAIT      = "Trait"      => "PHP trait / Rust trait / mixin",
    ENUM       = "Enum"       => "枚举",
    ENUM_CASE  = "EnumCase"   => "枚举成员",
    METHOD     = "Method"     => "类方法",
    FUNCTION   = "Function"   => "自由函数",
    PROPERTY   = "Property"   => "类属性 / 字段",
    CONST      = "Const"      => "常量",
    CALL_SITE  = "CallSite"   => "方法体内的一次调用点（P2 细化）",
    // ---- 合成节点（Synthesize 阶段按 FKB 规则物化）----
    TABLE      = "Table"         => "数据库表（Mediator，汇聚 200 处引用）",
    HTTP_CONTRACT = "HttpContract" => "HTTP 契约桥（前后端汇聚点）",
    CONFIG_KEY = "ConfigKey"   => "配置键",
    I18N_KEY   = "I18nKey"     => "国际化键",
    // 进程外中介：原本挂在 `ExternalSystem` 类别伞下，现各自是独立种类（kind），
    // 命名粒度与 Table / ConfigKey 一致，视角也直接按种类切换。
    EVENT      = "Event"       => "事件总线节点（进程外中介）",
    QUEUE      = "Queue"       => "消息队列节点（进程外中介）",
    // 事件 / 队列视角的**消费方**语义角色：监听器 / 消费者类在画布上重标为此 kind，
    // 与 Event / Queue 同族（仅视图层重标，DB 中仍存 `Class`，免重建）。
    EVENT_HANDLER = "EventHandler" => "事件 / 队列处理器（监听器 / 消费者类）",
    CACHE      = "Cache"       => "缓存节点（进程外中介）",
    TOPIC      = "Topic"       => "消息主题节点（进程外中介）",
    SCHEDULE   = "Schedule"    => "定时任务节点（计划任务 / 调度）",
    // 前端路由节点：uni-app `pages.json` 声明的页面路由，与后端 `Route` 同构
    // （路由视角里前后端「页面 ↔ 接口」直接对看）。
    PAGE       = "Page"        => "页面 / 路由节点（前端 pages.json 声明的路由）",
    // 前端事件总线节点：`uni.$emit('evt')` / `bus.$emit('evt')` 等组件解耦通信，
    // 与后端 `Event` 同构——同一事件名即同一节点，发射方与监听方都连到它。
    EVENT_BUS  = "EventBus"    => "事件总线节点（前端 uni.$emit / bus.$emit 组件通信）",
    UNKNOWN       = "Unknown"       => "未能归类 / 由 FKB 动态引入的新种类",
}

impl NodeKind {
    /// **第一类**合成（语义）节点：自身即语义、各带独立 identity 与视角的领域资产。
    ///
    /// 折叠视图默认**只**展示语义节点；其余（File / Class / Method / Function / CallSite…）
    /// 都只是实现细节，属于"点击展开才看的语法链路"。
    ///
    /// `Event` / `Queue` / `Cache` / `Topic` 这类进程外中介与 Table / ConfigKey 同级，
    /// 不再笼统地挂在 `ExternalSystem` 类别伞下——每个语义节点都以具体种类作为 kind。
    pub const SYNTHESIZED: &'static [&'static str] = &[
        Self::TABLE,
        Self::HTTP_CONTRACT,
        Self::CONFIG_KEY,
        Self::I18N_KEY,
        Self::EVENT,
        Self::QUEUE,
        Self::CACHE,
        Self::TOPIC,
        Self::SCHEDULE,
        Self::PAGE,
        Self::EVENT_BUS,
    ];

    /// 是否为"第一类"语义节点（kind 自身即语义）。
    ///
    /// 除内置清单外，还包括 **FKB 声明**的种类（见
    /// [`crate::model::FrameworkKnowledge::semantic_kinds`] + [`register_semantic_kinds`]）
    /// —— 新增一种语义节点不该以改内核为代价。
    pub fn is_semantic(&self) -> bool {
        if Self::SYNTHESIZED.iter().any(|k| self.0 == *k) {
            return true;
        }
        EXTRA_SEMANTIC
            .get_or_init(Default::default)
            .read()
            .map(|set| set.contains(&self.0))
            .unwrap_or(false)
    }
}

/// FKB 追加登记的语义节点种类（进程内单例，随 FKB 装载填充）。
static EXTRA_SEMANTIC: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// 登记 FKB 声明的语义节点种类（可重复调用，幂等合并）。
pub fn register_semantic_kinds(kinds: impl IntoIterator<Item = String>) {
    let mut set = EXTRA_SEMANTIC
        .get_or_init(Default::default)
        .write()
        .expect("语义种类注册表未被破坏");
    set.extend(kinds);
}

/// 当前已登记的 FKB 语义节点种类（供诊断 / 测试观察）。
pub fn extra_semantic_kinds() -> Vec<String> {
    let mut out: Vec<String> = EXTRA_SEMANTIC
        .get_or_init(Default::default)
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    out.sort();
    out
}

declare_open_kind! { EdgeKind => "图边种类（开放可扩展）";
    CONTAINS      = "Contains"      => "包包含系",
    DECLARES      = "Declares"      => "声明",
    EXTENDS       = "Extends"       => "继承",
    IMPLEMENTS    = "Implements"    => "实现接口",
    USES_TRAIT    = "UsesTrait"     => "使用 trait",
    CALLS         = "Calls"         => "调用",
    HAS_CALL_SITE = "HasCallSite"   => "方法体内含调用点",
    IMPORTS       = "Imports"       => "文件导入",
    // ---- 语义边（Synthesize / Resolve 建立）----
    HANDLED_BY    = "HandledBy"     => "由…处理（契约桥 → handler）",
    CALLS_HTTP    = "CallsHttp"     => "前端调用某个 HTTP 契约",
    TRIGGERS      = "Triggers"      => "触发事件",
    PUBLISHES_TO  = "PublishesTo"   => "投递到队列 topic",
    READS_DB      = "ReadsDb"       => "读库",
    WRITES_DB     = "WritesDb"      => "写库",
    WRITES_CACHE  = "WritesCache"   => "写缓存",
    MAPS_TO       = "MapsTo"        => "模型映射到表",
    READS_CONFIG  = "ReadsConfig"   => "读配置",
    MUTATES       = "Mutates"       => "改变状态容器（前端 Store / Vuex、Pinia…）",
    // 前端页面跳转：`uni.navigateTo` / `redirectTo` / `reLaunch` / `switchTab` 等，
    // 从发起方（函数 / 组件方法）指向目标 `Page` 节点。
    NAVIGATES_TO  = "NavigatesTo"   => "前端页面跳转（uni.navigateTo 等）",
    // 前端事件总线：发射方 `--Emits-->` 事件节点 `<--ListensTo--` 监听方，
    // 与后端 `Event` 的 `Triggers` 同构——事件节点在折叠视图里充当「via」桥。
    EMITS       = "Emits"       => "前端发射事件总线事件（uni.$emit / bus.$emit）",
    LISTENS_TO  = "ListensTo"   => "前端监听事件总线事件（uni.$on / bus.$on）",
    RESOLVES_TO   = "ResolvesTo"    => "动态解析结果",
    UNKNOWN       = "Unknown"       => "未能归类 / 由 FKB 动态引入的新边种类",
}

impl EdgeKind {
    /// 语义边（业务资源依赖）：Synthesize / Resolve 建立的业务依赖（读库 / 读配置 /
    /// 缓存 / 事件 / 跨服务…）。折叠视图默认只展示这些；它们**两端都应是语义节点**，
    /// 因此既可在画布上绘制，也计入候选徽标「语义入边 N」。
    ///
    /// 注意：`HandledBy` / `CallsHttp` **不在此列**——它们是「桥边」（见 [`Self::BRIDGE`]），
    /// 一端是语义节点、另一端是语法节点（handler / 前端函数），不是资源依赖，不计入
    /// 「入边 N」，也绝不画成画布边；其语法端点降级进 `orphans` 记账（前端抽屉 / Inspector
    /// 可展开查看）。其余（Contains / Declares / Calls / HasCallSite …）是实现结构，属语法链路。
    pub const SEMANTIC: &'static [&'static str] = &[
        Self::TRIGGERS,
        Self::PUBLISHES_TO,
        Self::READS_DB,
        Self::WRITES_DB,
        Self::MAPS_TO,
        Self::READS_CONFIG,
        Self::RESOLVES_TO,
        Self::WRITES_CACHE,
        Self::MUTATES,
        Self::NAVIGATES_TO,
        Self::EMITS,
        Self::LISTENS_TO,
        "ReadsCache",
    ];

    /// 桥边（语义 ↔ 语法之间的发现连接器）：`HandledBy`（契约 → handler）、
    /// `CallsHttp`（前端函数 → 契约）。它们不是业务资源依赖——不计入「入边 N」、
    /// 不当画布边——但必须留在 `is_chain_edge` 里供发现遍历把语法实现连通到语义资源；
    /// 其语法端点（handler / 调用方）降级进 `orphans` 记账，画布恒为语义节点。
    pub const BRIDGE: &'static [&'static str] = &[Self::HANDLED_BY, Self::CALLS_HTTP];

    /// 是否为"对人类有意义的语义边"（业务资源依赖，见 [`Self::SEMANTIC`]）。
    ///
    /// 除内置清单外，还包括 **FKB 声明**的边种类（见 [`register_edge_kinds`]）——
    /// 新增一种语义边不该以改内核为代价（与节点种类的
    /// [`crate::model::kinds::EXTRA_SEMANTIC`] 同构）。
    pub fn is_semantic(&self) -> bool {
        if Self::SEMANTIC.iter().any(|k| self.0 == *k) {
            return true;
        }
        EXTRA_SEMANTIC_EDGE
            .get_or_init(Default::default)
            .read()
            .map(|set| set.contains(&self.0))
            .unwrap_or(false)
    }

    /// 是否为"桥边"（语义 ↔ 语法之间的发现连接器，见 [`Self::BRIDGE`]）。
    ///
    /// 除内置清单外，还包括 **FKB 声明**的桥边种类（见 [`register_edge_kinds`]）。
    pub fn is_bridge(&self) -> bool {
        if Self::BRIDGE.iter().any(|k| self.0 == *k) {
            return true;
        }
        EXTRA_BRIDGE_EDGE
            .get_or_init(Default::default)
            .read()
            .map(|set| set.contains(&self.0))
            .unwrap_or(false)
    }
}

/// 语义边判定（权威来源：[`EdgeKind::SEMANTIC`]）。
///
/// 与 `is_chain_edge` 的区别：链边是"沿调用链能否走通"（含 `Calls` / `HasCallSite`
/// 这类语法调用边），语义边才是"这条边本身对人类有意义"（读写表 / 触发事件…）。
/// 计数口径要按语义边 —— 否则 `Calls` 会混进"入边 N"，数字既不与画布对得上，
/// 也读不出任何业务含义。
pub fn is_semantic_edge(kind: &str) -> bool {
    EdgeKind(kind.to_string()).is_semantic()
}

/// 桥边判定（权威来源：[`EdgeKind::BRIDGE`]）。
///
/// 桥边连接"语义节点 ↔ 语法节点"，不是业务资源依赖：不计入「入边 N」、不当画布边，
/// 但保留在发现遍历里。折叠逻辑用它与 `is_semantic_edge` 共同决定"是否可被绘制闸门放行、
/// 但仍降级进 `orphans` 记账"——详见 `gt_application::view_service`。
pub fn is_bridge_edge(kind: &str) -> bool {
    EdgeKind(kind.to_string()).is_bridge()
}

/// FKB 追加登记的语义 / 桥边种类（进程内单例，随 FKB 装载填充）。
///
/// 与节点的 [`EXTRA_SEMANTIC`] 同构：让"新增一种边种类"也只需写 FKB、不改内核——
/// 这是「只写 FKB、零代码」承诺在**边**这一维的落地（节点的同款机制早已就位）。
static EXTRA_SEMANTIC_EDGE: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();
static EXTRA_BRIDGE_EDGE: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// 登记 FKB 声明的语义 / 桥边种类（可重复调用，幂等合并）。
///
/// 调用方：`gt_adapter_fkb::loader` 在装载 FKB 时，把 `semantic_edge_kinds` /
/// `bridge_edge_kinds` 喂进来。之后 `is_semantic` / `is_bridge` / `is_chain_edge`
/// 会自动把它们当一等公民，无需再碰 `kinds.rs` 的 `SEMANTIC` / `BRIDGE` 清单。
pub fn register_edge_kinds(
    semantic: impl IntoIterator<Item = String>,
    bridge: impl IntoIterator<Item = String>,
) {
    {
        let mut set = EXTRA_SEMANTIC_EDGE
            .get_or_init(Default::default)
            .write()
            .expect("语义边注册表未被破坏");
        set.extend(semantic);
    }
    {
        let mut set = EXTRA_BRIDGE_EDGE
            .get_or_init(Default::default)
            .write()
            .expect("桥边注册表未被破坏");
        set.extend(bridge);
    }
}

/// 当前已登记的 FKB 边种类（供诊断 / 测试 / `validate` 观察）。
pub fn extra_edge_kinds() -> (Vec<String>, Vec<String>) {
    let sem: Vec<String> = EXTRA_SEMANTIC_EDGE
        .get_or_init(Default::default)
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    let bri: Vec<String> = EXTRA_BRIDGE_EDGE
        .get_or_init(Default::default)
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    (sem, bri)
}

/// 调用链边：折叠视图沿这些边做"正向发现"，把语法节点当透传。
///
/// 权威来源是 [`EdgeKind::is_semantic`] + [`EdgeKind::is_bridge`]（含 FKB 声明的种类）
/// 加上少量纯语法链边（`Calls` / `HasCallSite`）。集中在此作为唯一权威来源——
/// 新增边种类只要登记进 `SEMANTIC` / `BRIDGE` 即自动可遍历，无需在此再列举。
pub fn is_chain_edge(kind: &str) -> bool {
    matches!(kind, "Calls" | "HasCallSite")
        || EdgeKind(kind.to_string()).is_semantic()
        || EdgeKind(kind.to_string()).is_bridge()
}

declare_open_kind! { Phase => "流水线阶段";
    INGEST        = "Ingest"        => "P0 摄取：识别子工程与待分析文件",
    CF_AST        = "CfAst"         => "P2 语法级建图",
    PREPARE       = "Prepare"       => "P3 装载 FKB 与权威符号表",
    ANNOTATE_PRE  = "AnnotatePre"   => "P4 按源码选择器打标",
    SYNTHESIZE    = "Synthesize"    => "P5 合成非代码语义节点",
    ANNOTATE_POST = "AnnotatePost"  => "P6 在汇聚结果上打标 / 注册别名",
    RESOLVE       = "Resolve"       => "P7 动态解析（漏斗 + 不动点）",
    PROPAGATE     = "Propagate"     => "P8 语义沿调用链向上传播（通用，不绑定框架）",
    CHECK         = "Check"         => "合规检查（图建完后只读跑规则，产出违规，诊断 code 前缀 rule:）",
}

declare_open_kind! { AnnotationChannel => "标注通道（不同通道互不干扰，可共存）";
    FKB_MARK    = "FkbMark"    => "框架语义标签：listener / pii / auth.public / data.criticality",
    TAINT       = "Taint"      => "污点语义：source / sanitizer / sink",
    CAPABILITY  = "Capability" => "作用域声明的能力：RateLimiting / Authentication",
    ALIAS       = "Alias"      => "别名注册：event_name / facade / accessor",
}

declare_open_kind! { SynthesizedKind => "合成节点的 identity 类型";
    FQN     = "Fqn"     => "完全限定名（如 `Table:store_order`）",
    NAMED   = "Named"   => "具名（如事件名 `order.pay_success`）",
    CONTRACT_ID = "ContractId" => "HTTP 契约 `METHOD /path`",
}

/// 技术栈语言（开放可扩展）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default)]
#[serde(transparent)]
pub struct Language(pub String);

impl Language {
    pub const PHP: &'static str = "php";
    pub const JAVASCRIPT: &'static str = "javascript";
    pub const TYPESCRIPT: &'static str = "typescript";
    pub const JAVA: &'static str = "java";
    pub const PYTHON: &'static str = "python";
    pub const RUST: &'static str = "rust";
    pub const UNKNOWN: &'static str = "unknown";

    pub fn new<S: Into<String>>(s: S) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_php(&self) -> bool {
        self.0 == Self::PHP
    }
}

impl From<&str> for Language {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_kind_registry_extends_classification() {
        // 内置语义边：注册前即识别。
        assert!(EdgeKind("ReadsDb".to_string()).is_semantic());
        assert!(is_chain_edge("ReadsDb"));

        // 全新语义边：注册前不识别，注册后识别（且自动可沿调用链遍历）。
        assert!(!EdgeKind("SendsWebhook".to_string()).is_semantic());
        assert!(!is_chain_edge("SendsWebhook"));
        register_edge_kinds(vec!["SendsWebhook".to_string()], vec![]);
        assert!(EdgeKind("SendsWebhook".to_string()).is_semantic());
        assert!(is_chain_edge("SendsWebhook"));

        // 全新桥边：注册前不识别，注册后识别为桥边且可遍历。
        assert!(!EdgeKind("MyBridge".to_string()).is_bridge());
        register_edge_kinds(vec![], vec!["MyBridge".to_string()]);
        assert!(EdgeKind("MyBridge".to_string()).is_bridge());
        assert!(is_chain_edge("MyBridge"));

        // 旧硬编码路径不受影响：内置桥边仍识别。
        assert!(EdgeKind("HandledBy".to_string()).is_bridge());
        assert!(is_chain_edge("HandledBy"));
        // 纯语法链边仍走内置分支。
        assert!(is_chain_edge("Calls"));
        assert!(is_chain_edge("HasCallSite"));
    }
}
