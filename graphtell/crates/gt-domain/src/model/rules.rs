//! 合规规则（Check）—— 在建好的图上回答「这个代码库是否违规」。
//!
//! # 与 FKB `Rule` 的区别
//!
//! FKB 的 [`crate::model::Rule`] 是**合成规则**：在 P4/P6 阶段给图"添东西"
//! （打标注、合成语义节点）。本模块的 [`CheckRule`] 是**检查规则**：图建完之后
//! 只读地遍历它，产出"违规"（[`Violation`]）。
//!
//! 二者都声明在 YAML 里、都由内核之外的知识驱动 —— 内核不认识任何具体规则。
//!
//! # 为什么违规落成 `Diagnostic`
//!
//! [`Violation::to_diagnostic`] 把违规转成诊断（code = `rule:<id>`）。诊断已经是
//! 一等产物（有 `severity` / `location` / `payload`，DiagnosticsPage 直接渲染），
//! 复用它意味着规则引擎**不需要新的存储与新的页面**。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::graph::{Diagnostic, Severity};
use crate::model::ids::{NodeId, ProjectId};
use crate::model::kinds::Phase;

/// 违规诊断的 code 前缀（用于与建图期诊断区分、以及重跑时清理）。
pub const RULE_CODE_PREFIX: &str = "rule:";

/// 违规诊断所处的阶段。
pub fn check_phase() -> Phase {
    Phase(Phase::CHECK.to_string())
}

/// 一条检查规则。
///
/// ```yaml
/// - id: http-contract-without-handler
///   title: HTTP 契约没有处理者
///   severity: error
///   category: correctness
///   applies_to: { kinds: [HttpContract] }
///   when:
///     - no_incoming: HandledBy
///   message: "契约 {name} 没有 handler，请求它会在运行时报错"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRule {
    /// 稳定标识，同时是诊断 code 的后缀（`rule:<id>`）。
    pub id: String,
    /// 人类可读的规则名。
    pub title: String,
    /// 规则意图说明（供 UI 展示）。
    #[serde(default)]
    pub description: Option<String>,
    /// 命中时的严重级别。
    #[serde(default = "default_severity")]
    pub severity: Severity,
    /// 分组标签（architecture / security / deadcode / contract …）。
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 规则作用的节点范围（决定候选集，也决定求值成本）。
    #[serde(default)]
    pub applies_to: RuleScope,
    /// 命中条件：**全部满足**才判违规（`when` 为空表示范围内全部命中）。
    #[serde(default)]
    pub when: Vec<CheckPredicate>,
    /// 违规文案，支持 `{name}` / `{kind}` / `{fqn}` / `{identity}` / `{file}` / `{line}` 占位符。
    pub message: String,
    /// 修复建议。
    #[serde(default)]
    pub remediation: Option<String>,
}

fn default_severity() -> Severity {
    Severity::Warning
}
fn default_category() -> String {
    "general".to_string()
}
fn default_true() -> bool {
    true
}

impl CheckRule {
    /// 诊断 code：`rule:<id>`。
    pub fn code(&self) -> String {
        format!("{RULE_CODE_PREFIX}{}", self.id)
    }

    /// 判据依赖的图事实（自动推导，见 [`RuleRequirements`]）。
    pub fn requirements(&self) -> RuleRequirements {
        let mut out = RuleRequirements::default();
        for p in &self.when {
            collect(p, &mut out);
        }
        out
    }

    /// 环境是否匹配：语言的**任一**子工程命中即可，框架同理。
    ///
    /// 一个工程往往是多语言的（CRMEB = php + javascript，litemall = java + javascript），
    /// 所以是"存在匹配"而不是"全部匹配" —— 只要有 PHP 子工程，PHP 规则就该跑。
    pub fn applies_to_env(&self, languages: &[String], frameworks: &[String]) -> bool {
        let lang_ok = self.applies_to.languages.is_empty()
            || self
                .applies_to
                .languages
                .iter()
                .any(|l| languages.iter().any(|x| x.eq_ignore_ascii_case(l)));
        if !lang_ok {
            return false;
        }
        self.applies_to.frameworks.is_empty()
            || self
                .applies_to
                .frameworks
                .iter()
                .any(|f| frameworks.iter().any(|x| x.eq_ignore_ascii_case(f)))
    }

    /// 渲染违规文案。
    pub fn render(
        &self,
        node_name: &str,
        node_kind: &str,
        fqn: Option<&str>,
        identity: Option<&str>,
        file: Option<&str>,
        line: Option<u32>,
    ) -> String {
        self.message
            .replace("{name}", node_name)
            .replace("{kind}", node_kind)
            .replace("{fqn}", fqn.unwrap_or(""))
            .replace("{identity}", identity.unwrap_or(""))
            .replace("{file}", file.unwrap_or(""))
            .replace("{line}", &line.map(|l| l.to_string()).unwrap_or_default())
    }
}

/// 规则作用的节点范围。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleScope {
    /// 节点种类白名单；为空表示不限制种类（慎用，成本高）。
    #[serde(default)]
    pub kinds: Vec<String>,
    /// 名字预筛选（大小写不敏感子串），可显著降低候选集规模。
    #[serde(default)]
    pub name_contains: Option<String>,
    /// 候选集上限，防止规则扫全图拖垮一次检查。
    #[serde(default = "default_scope_limit")]
    pub limit: u32,
    /// **适用语言白名单**（`php` / `java` / `javascript` / `typescript` …）。
    ///
    /// 为空表示跨语言通用 —— 这类规则的判据必须只依赖**图拓扑**
    /// （边种类 / 扇入扇出），因为拓扑是归一化的：PHP 与 Java 都产出
    /// `HttpContract --HandledBy--> Method`。
    ///
    /// 一旦规则的判据依赖某个 FKB 产出的标注或边（例如 PHP 的事件语义
    /// `Triggers` / `Emits`），就必须声明语言：Java 工程里根本没有这些边，
    /// 规则会把每个事件节点都报成"没人触发"—— 环境闸门就是要挡住这种误报。
    #[serde(default)]
    pub languages: Vec<String>,
    /// **适用框架白名单**（`thinkphp6` / `laravel` / `spring-boot` / `uni-app` …）。
    ///
    /// 粒度比 `languages` 更细：同一语言下不同框架的边语义也可能不同。
    /// 为空表示不限框架。
    #[serde(default)]
    pub frameworks: Vec<String>,
}

fn default_scope_limit() -> u32 {
    20_000
}

/// 检查谓词。
///
/// 命名刻意贴近 [`crate::model::Predicate`]（FKB 合成用），但语义是**面向已建成的图**：
/// 边种类、扇入扇出、标注、属性 —— 而不是源码里的调用点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckPredicate {
    /// 节点种类属于给定集合。
    KindIn(Vec<String>),
    /// 名字包含子串（大小写不敏感）。
    NameContains(String),
    /// 名字以给定前缀开头（大小写不敏感）。
    NameStartsWith(String),
    /// FQN 包含子串。
    FqnContains(String),
    /// identity 值包含子串（如 `POST /api/xxx`）。
    IdentityContains(String),
    /// `name` / `fqn` / `identity` 任一包含子串。
    TextContains(String),
    /// 节点上有给定种类的标注。
    HasAnnotation(String),
    /// 节点上**没有**给定种类的标注。
    NoAnnotation(String),
    /// 节点属性等于给定值（如 `side = frontend`）。
    PropertyIs { name: String, value: String },
    /// 节点缺少给定属性。
    PropertyMissing(String),
    /// 作用域链上没有给定能力（如 `Authentication` / `RateLimiting`）。
    NoCapability(Vec<String>),
    /// 扇入（语义入边数）不小于阈值。
    FanInGte(u64),
    /// 扇入不大于阈值（`0` = 没有任何语义入边）。
    FanInLte(u64),
    /// 扇出不小于阈值。
    FanOutGte(u64),
    /// **没有**给定种类的入边（如契约没有 `HandledBy`）。
    NoIncoming(String),
    /// 有给定种类的入边。
    HasIncoming(String),
    /// **没有**给定种类的出边。
    NoOutgoing(String),
    /// 有给定种类的出边。
    HasOutgoing(String),
    /// 全部满足。
    AllOf(Vec<CheckPredicate>),
    /// 任一满足。
    AnyOf(Vec<CheckPredicate>),
    /// 取反。
    Not(Box<CheckPredicate>),
}

/// 规则判据所依赖的图事实。
///
/// 由 [`CheckRule::requirements`] 从谓词**自动推导**，不需要人工在 YAML 里声明 ——
/// 人工声明的 `requires` 会和 `when` 脱节（`when` 改了忘记改 `requires`），
/// 而推导出来的永远和判据一致。
///
/// # 为什么必须有这个
///
/// **反向谓词在"证据不存在"时是恒真的**。图上从来没有 `Triggers` 边时，
/// `no_incoming: Triggers` 对每个节点都成立 —— 于是"没人触发"会命中全部事件节点。
/// 这正是本项目已经踩过的两个坑（2683 条幽灵调用、28 个 EventBus 全误报）。
/// 因此跑规则前必须确认：**判据里提到的边/标注，图里真的存在过**。
#[derive(Debug, Clone, Default)]
pub struct RuleRequirements {
    /// 判据用到的边种类（`HasIncoming` / `NoIncoming` / `HasOutgoing` / `NoOutgoing`）。
    pub edges: Vec<String>,
    /// 判据用到的标注种类（`HasAnnotation` / `NoAnnotation`）。
    pub annotations: Vec<String>,
    /// 判据用到的能力（`NoCapability`）；要求 `Capability` 通道上有对应标注。
    pub capabilities: Vec<String>,
}

impl RuleRequirements {
    fn push_edge(&mut self, k: &str) {
        if !self.edges.iter().any(|e| e.eq_ignore_ascii_case(k)) {
            self.edges.push(k.to_string());
        }
    }
    fn push_annotation(&mut self, k: &str) {
        if !self.annotations.iter().any(|a| a.eq_ignore_ascii_case(k)) {
            self.annotations.push(k.to_string());
        }
    }
    fn push_capabilities(&mut self, caps: &[String]) {
        for c in caps {
            if !self.capabilities.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                self.capabilities.push(c.clone());
            }
        }
    }
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty() && self.annotations.is_empty() && self.capabilities.is_empty()
    }
}

/// 递归收集谓词里的依赖。
///
/// `Not` 内部照收不翻转极性：这里做的是"判据是否成立"的**保守检查**，
/// 漏报一次（该拦没拦）远好过误拦一条本来能跑的规则。
fn collect(p: &CheckPredicate, out: &mut RuleRequirements) {
    match p {
        CheckPredicate::HasIncoming(k)
        | CheckPredicate::NoIncoming(k)
        | CheckPredicate::HasOutgoing(k)
        | CheckPredicate::NoOutgoing(k) => out.push_edge(k),
        CheckPredicate::HasAnnotation(k) | CheckPredicate::NoAnnotation(k) => {
            out.push_annotation(k)
        }
        CheckPredicate::NoCapability(caps) => out.push_capabilities(caps),
        CheckPredicate::AllOf(list) | CheckPredicate::AnyOf(list) => {
            for inner in list {
                collect(inner, out);
            }
        }
        CheckPredicate::Not(inner) => collect(inner, out),
        _ => {}
    }
}

/// 一次命中的违规。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub project_id: ProjectId,
    pub rule_id: String,
    pub title: String,
    pub category: String,
    pub severity: Severity,
    pub node_id: NodeId,
    pub node_name: String,
    pub node_kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl Violation {
    /// 诊断 code（`rule:<id>`）。
    pub fn code(&self) -> String {
        format!("{RULE_CODE_PREFIX}{}", self.rule_id)
    }

    /// `path:line` 定位串；没有文件信息时为 `None`。
    pub fn location(&self) -> Option<String> {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => Some(format!("{f}:{l}")),
            (Some(f), None) => Some(f.clone()),
            _ => None,
        }
    }

    /// 落成诊断（复用既有的一等产物通道）。
    pub fn to_diagnostic(&self) -> Diagnostic {
        Diagnostic {
            project_id: self.project_id,
            phase: check_phase(),
            code: self.code(),
            severity: self.severity,
            message: self.message.clone(),
            location: self.location(),
            payload: json!({
                "rule_id": self.rule_id,
                "title": self.title,
                "category": self.category,
                "node_id": self.node_id.get(),
                "node_name": self.node_name,
                "node_kind": self.node_kind,
                "remediation": self.remediation,
                "file": self.file,
                "line": self.line,
            }),
        }
    }

    /// 从诊断还原（诊断表里只有 payload 时用于重建违规）。
    pub fn from_diagnostic(d: &Diagnostic) -> Option<Self> {
        let p = d.payload.as_object()?;
        Some(Self {
            project_id: d.project_id,
            rule_id: p.get("rule_id")?.as_str()?.to_string(),
            title: p.get("title")?.as_str().unwrap_or_default().to_string(),
            category: p.get("category")?.as_str().unwrap_or_default().to_string(),
            severity: d.severity,
            node_id: NodeId(p.get("node_id")?.as_i64()?),
            node_name: p.get("node_name")?.as_str().unwrap_or_default().to_string(),
            node_kind: p.get("node_kind")?.as_str().unwrap_or_default().to_string(),
            message: d.message.clone(),
            remediation: p
                .get("remediation")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            file: p.get("file").and_then(|v| v.as_str()).map(|s| s.to_string()),
            line: p.get("line").and_then(|v| v.as_u64()).map(|v| v as u32),
        })
    }
}

/// 一次规则检查的报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckReport {
    pub project_id: ProjectId,
    /// 已装载的规则总数（含被禁用的）。
    pub rules_total: usize,
    /// 实际执行的规则数。
    pub rules_run: usize,
    /// 命中的违规。
    pub violations: Vec<Violation>,
    /// 按严重级别计数。
    pub by_severity: BTreeMap<String, u64>,
    /// 按规则计数。
    pub by_rule: BTreeMap<String, u64>,
    /// **跑了但一条都没命中的规则 id**。
    ///
    /// 为什么要有这个字段：规则最常见的失效方式不是误报，而是**静默归零** ——
    /// 判据用了一个图上根本不存在的标注/边（例如 `pii`、`Capability:Authentication`），
    /// 于是永远匹配不上。此时报告"0 条违规"会被读成"代码没问题"，
    /// 比误报危险得多。把它们显式列出来，就没人能把"规则瞎了"当成"代码干净"。
    pub rules_silent: Vec<String>,
    /// **环境不匹配而跳过的规则**（声明了 `languages` / `frameworks`，本工程没有该栈）。
    ///
    /// 这是**预期行为**，不是故障：PHP 专属规则不该在纯 Java 工程上跑。
    /// 单独列出是为了和"静默归零"区分开 —— 两者都表现为 0 命中，但性质完全不同。
    #[serde(default)]
    pub rules_not_applicable: Vec<String>,
    /// **判据不成立而停用的规则**：判据提到的边/标注/能力在本工程图上一个都没有。
    ///
    /// 这时跑规则只会产出**恒真误报**（`no_incoming: X` 在 X 不存在时对所有节点成立）。
    /// 宁可不跑，也不要报一堆假的。
    #[serde(default)]
    pub rules_unavailable: Vec<String>,
    pub duration_ms: u64,
}

impl CheckReport {
    /// 按严重级别降序、再按规则 id 排序（保证输出可复现）。
    pub fn sort_violations(&mut self) {
        let rank = |s: Severity| match s {
            Severity::Critical => 0,
            Severity::Error => 1,
            Severity::Warning => 2,
            Severity::Info => 3,
        };
        self.violations.sort_by(|a, b| {
            rank(a.severity)
                .cmp(&rank(b.severity))
                .then(a.rule_id.cmp(&b.rule_id))
                .then(a.node_name.cmp(&b.node_name))
        });
    }
}
