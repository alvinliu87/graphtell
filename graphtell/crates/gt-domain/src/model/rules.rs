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
use serde_json::{json, Value};

use crate::model::graph::{Diagnostic, Severity};
use crate::model::ids::{NodeId, ProjectId, SubProjectId};
use crate::model::kinds::Phase;

/// 违规诊断的 code 前缀（用于与建图期诊断区分、以及重跑时清理）。
pub const RULE_CODE_PREFIX: &str = "rule:";

/// 违规诊断所处的阶段。
pub fn check_phase() -> Phase {
    Phase(Phase::CHECK.to_string())
}

/// 参数引用的前缀：`$key` 表示"取 `params` 里 key 的值"。
///
/// 必须显式带前缀，不能靠"是不是纯数字"来猜 —— 字符串型参数尤其如此：
/// `"50"` 既可能是字面量也可能是引用，猜错就是静默的错误结果。
pub const PARAM_PREFIX: char = '$';

/// 解析一个可能是"字面量 / 参数引用"的字符串。
///
/// * `"$key"` → 引用参数 `key`
/// * `"$$x"` → 字面量 `$x`（转义：先吞一个 `$`）
/// * `"$"` 或 `"50"` → 字面量（孤立的 `$` 不构成引用）
fn parse_param_ref(s: String) -> Result<String, String> {
    if let Some(rest) = s.strip_prefix("$$") {
        Ok(format!("{PARAM_PREFIX}{rest}"))
    } else if let Some(rest) = s.strip_prefix(PARAM_PREFIX) {
        if rest.is_empty() {
            Ok(s)
        } else {
            Err(rest.to_string())
        }
    } else {
        Ok(s)
    }
}

/// 规则参数可取的值：字面量，或引用 `params` 里声明的参数（写 `$key`）。
///
/// 序列化刻意**不是** untagged：untagged 对字符串无从区分字面量与引用
/// （`Str` 变体会吃掉 `"$x"`），而数值侧会把 `$` 一起留在键名里，
/// 于是 `params.get("$x")` 永远取不到值、静默退化成 0。
/// 手写 impl 把 `$` 前缀在**解析时**就吃掉，`Param` 里只留干净的键。
#[derive(Debug, Clone)]
pub enum NumOrParam {
    Num(u64),
    Param(String),
}

impl Default for NumOrParam {
    fn default() -> Self {
        NumOrParam::Num(0)
    }
}

impl Serialize for NumOrParam {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            NumOrParam::Num(n) => n.serialize(s),
            NumOrParam::Param(k) => format!("{PARAM_PREFIX}{k}").serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for NumOrParam {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(match v {
            Value::Number(n) => NumOrParam::Num(n.as_u64().unwrap_or(0)),
            Value::String(s) => match parse_param_ref(s) {
                Ok(literal) => NumOrParam::Num(literal.parse().unwrap_or(0)),
                Err(key) => NumOrParam::Param(key),
            },
            _ => NumOrParam::Num(0),
        })
    }
}

/// 同上，针对字符串型参数（名称前缀、忽略名单等）。
#[derive(Debug, Clone)]
pub enum StrOrParam {
    Str(String),
    Param(String),
}

impl Default for StrOrParam {
    fn default() -> Self {
        StrOrParam::Str(String::new())
    }
}

impl Serialize for StrOrParam {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            StrOrParam::Str(v) => v.serialize(s),
            StrOrParam::Param(k) => format!("{PARAM_PREFIX}{k}").serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for StrOrParam {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(match v {
            Value::String(s) => match parse_param_ref(s) {
                Ok(literal) => StrOrParam::Str(literal),
                Err(key) => StrOrParam::Param(key),
            },
            other => StrOrParam::Str(other.to_string()),
        })
    }
}

/// 一条规则可暴露给用户调节的参数种类。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    Number,
    String,
    Enum,
    Bool,
}

/// 一条规则暴露给用户的可调参数（在 YAML `params:` 下声明）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleParam {
    /// 参数键（在 `when` / `applies_to` 里用 `$key` 引用）。
    pub key: String,
    /// 展示名（UI 用）。
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    pub kind: ParamKind,
    /// 默认值（与 `kind` 对应的 JSON 标量）。
    pub default: Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// `kind = enum` 时的候选值。
    #[serde(default)]
    pub choices: Vec<String>,
}

/// 工程级对单条规则的配置覆盖。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRuleConfig {
    pub project_id: ProjectId,
    pub rule_id: String,
    /// `None` = 继承 YAML 里的全局 `enabled`；`Some(b)` = 工程级覆盖。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 参数覆盖（`param_key -> value`），未覆盖的取规则 `params` 的默认。
    #[serde(default)]
    pub options: Value,
}

/// 配置写入请求（部分字段可省略 = 不改动该项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfigPatch {
    pub rule_id: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub options: Option<Value>,
}

/// 解析后的参数表：`param_key -> value`（全局默认 + 工程覆盖已合并）。
pub type ParamValues = std::collections::HashMap<String, Value>;

/// 把规则的 `params` 默认值与工程覆盖合并成求值可用的参数表。
pub fn resolve_param_values(rule: &CheckRule, overrides: &Value) -> ParamValues {
    let mut m = ParamValues::new();
    for p in &rule.params {
        m.insert(p.key.clone(), p.default.clone());
    }
    if let Some(obj) = overrides.as_object() {
        for (k, v) in obj {
            m.insert(k.clone(), v.clone());
        }
    }
    m
}

/// 取出一个数值参数（字面量直接返回，引用则从参数表取；缺失回退 0）。
pub fn resolve_num(v: &NumOrParam, params: &ParamValues) -> u64 {
    match v {
        NumOrParam::Num(n) => *n,
        NumOrParam::Param(key) => match params.get(key) {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        },
    }
}

/// 取出一个字符串参数（字面量直接返回，引用则从参数表取；缺失回退空串）。
pub fn resolve_str(v: &StrOrParam, params: &ParamValues) -> String {
    match v {
        StrOrParam::Str(s) => s.clone(),
        StrOrParam::Param(key) => match params.get(key) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        },
    }
}

/// `Option<StrOrParam>` 的便捷解析：空串视为 `None`。
pub fn resolve_str_opt(v: &Option<StrOrParam>, params: &ParamValues) -> Option<String> {
    match v {
        None => None,
        Some(s) => {
            let r = resolve_str(s, params);
            if r.is_empty() {
                None
            } else {
                Some(r)
            }
        }
    }
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
    /// 暴露给用户调节的参数（在 `when` / `applies_to` 里用 `$key` 引用）。
    #[serde(default)]
    pub params: Vec<RuleParam>,
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

    /// 判据里以 `$key` 形式引用的参数键（去重、保持出现顺序）。
    pub fn referenced_params(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |k: &str| {
            if !out.iter().any(|x| x == k) {
                out.push(k.to_string());
            }
        };
        if let Some(StrOrParam::Param(k)) = &self.applies_to.name_contains {
            push(k);
        }
        if let NumOrParam::Param(k) = &self.applies_to.limit {
            push(k);
        }
        collect_params(&self.when, &mut out);
        out
    }

    /// 被引用、却没在 `params` 里声明的参数键。
    ///
    /// 这类引用在求值时会静默退化成 `0` / `""` —— 用在 `limit` 上就是
    /// **候选集直接变空**（规则静默 0 命中），正是本项目最想避免的失效方式。
    /// 因此它必须在装载阶段就被发现，而不是等用户把"0 违规"读成"代码干净"。
    pub fn undeclared_params(&self) -> Vec<String> {
        self.referenced_params()
            .into_iter()
            .filter(|k| !self.params.iter().any(|p| &p.key == k))
            .collect()
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
        self.render_with(
            node_name, node_kind, fqn, identity, file, line, None,
        )
    }

    // 占位符有 6 个（节点名 / 种类 / FQN / identity / 文件 / 行号），加参数表就是 8 个；
    // 硬拆成结构体只会让调用点更难读，这里选择保留平铺签名。
    #[allow(clippy::too_many_arguments)]
    /// 渲染违规文案（带参数表）。
    ///
    /// 除 `{name}` / `{file}` 等节点占位符外，还支持 `{param:key}` ——
    /// 把工程调过的阈值写进文案。否则用户把"热点表阈值"调成 200 之后，
    /// 报告里仍然写着"≥ 50"，读起来像规则没生效。
    pub fn render_with(
        &self,
        node_name: &str,
        node_kind: &str,
        fqn: Option<&str>,
        identity: Option<&str>,
        file: Option<&str>,
        line: Option<u32>,
        params: Option<&ParamValues>,
    ) -> String {
        let mut out = self
            .message
            .replace("{name}", node_name)
            .replace("{kind}", node_kind)
            .replace("{fqn}", fqn.unwrap_or(""))
            .replace("{identity}", identity.unwrap_or(""))
            .replace("{file}", file.unwrap_or(""))
            .replace("{line}", &line.map(|l| l.to_string()).unwrap_or_default());
        if let Some(p) = params {
            for param in &self.params {
                let ph = format!("{{param:{}}}", param.key);
                if out.contains(&ph) {
                    let v = match p.get(&param.key) {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Number(n)) => n.to_string(),
                        Some(Value::Bool(b)) => b.to_string(),
                        Some(other) => other.to_string(),
                        None => param.default.to_string(),
                    };
                    out = out.replace(&ph, &v);
                }
            }
        }
        out
    }
}

/// 规则作用的节点范围。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleScope {
    /// 节点种类白名单；为空表示不限制种类（慎用，成本高）。
    #[serde(default)]
    pub kinds: Vec<String>,
    /// 名字预筛选（大小写不敏感子串），可显著降低候选集规模。支持 `$param` 引用。
    #[serde(default)]
    pub name_contains: Option<StrOrParam>,
    /// 候选集上限，防止规则扫全图拖垮一次检查。支持 `$param` 引用。
    #[serde(default = "default_scope_limit")]
    pub limit: NumOrParam,
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

fn default_scope_limit() -> NumOrParam {
    NumOrParam::Num(20_000)
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
    /// 名字包含子串（大小写不敏感）。支持 `$param` 引用。
    NameContains(StrOrParam),
    /// 名字以给定前缀开头（大小写不敏感）。支持 `$param` 引用。
    NameStartsWith(StrOrParam),
    /// FQN 包含子串。支持 `$param` 引用。
    FqnContains(StrOrParam),
    /// identity 值包含子串（如 `POST /api/xxx`）。支持 `$param` 引用。
    IdentityContains(StrOrParam),
    /// `name` / `fqn` / `identity` 任一包含子串。支持 `$param` 引用。
    TextContains(StrOrParam),
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
    /// 扇入（语义入边数）不小于阈值。支持 `$param` 引用。
    FanInGte(NumOrParam),
    /// 扇入不大于阈值（`0` = 没有任何语义入边）。支持 `$param` 引用。
    FanInLte(NumOrParam),
    /// 扇出不小于阈值。支持 `$param` 引用。
    FanOutGte(NumOrParam),
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

/// 递归收集谓词里引用的参数键。
fn collect_params(predicates: &[CheckPredicate], out: &mut Vec<String>) {
    fn push(out: &mut Vec<String>, k: &str) {
        if !out.iter().any(|x| x == k) {
            out.push(k.to_string());
        }
    }
    for p in predicates {
        match p {
            CheckPredicate::NameContains(v)
            | CheckPredicate::NameStartsWith(v)
            | CheckPredicate::FqnContains(v)
            | CheckPredicate::IdentityContains(v)
            | CheckPredicate::TextContains(v) => {
                if let StrOrParam::Param(k) = v {
                    push(out, k);
                }
            }
            CheckPredicate::FanInGte(v)
            | CheckPredicate::FanInLte(v)
            | CheckPredicate::FanOutGte(v) => {
                if let NumOrParam::Param(k) = v {
                    push(out, k);
                }
            }
            CheckPredicate::AllOf(list) | CheckPredicate::AnyOf(list) => collect_params(list, out),
            CheckPredicate::Not(inner) => collect_params(std::slice::from_ref(inner), out),
            _ => {}
        }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_project_id: Option<SubProjectId>,
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
            sub_project_id: self.sub_project_id,
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
                "sub_project_id": self.sub_project_id.map(|s| s.get()),
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
            sub_project_id: d.sub_project_id,
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
#[cfg(test)]
mod tests {
    use super::*;

    fn rule_with_params() -> CheckRule {
        // 走一遍反序列化：参数体系的价值就在于"规则作者写的声明能被正确读进来"。
        let v = json!({
            "id": "hot-table",
            "title": "热点表",
            "severity": "info",
            "category": "architecture",
            "params": [
                { "key": "min_fan_in", "label": "扇入阈值", "kind": "number", "default": 50 },
                { "key": "name_filter", "label": "名称过滤", "kind": "string", "default": "" }
            ],
            "applies_to": {
                "kinds": ["Table"],
                "name_contains": "$name_filter",
                "limit": "$max_nodes"
            },
            "when": [{ "fan_in_gte": "$min_fan_in" }],
            "message": "表 {name} 热（入边 ≥ {param:min_fan_in}）"
        });
        serde_json::from_value(v).expect("规则声明应能解析")
    }

    #[test]
    fn 参数声明与默认解析() {
        let r = rule_with_params();
        assert_eq!(r.params.len(), 2);
        assert_eq!(r.params[0].key, "min_fan_in");
        assert!(matches!(r.params[0].kind, ParamKind::Number));

        let params = resolve_param_values(&r, &Value::Null);
        assert_eq!(resolve_num(&NumOrParam::Param("min_fan_in".into()), &params), 50);
        // 字符串参数默认空串 → `name_contains` 退化为"不过滤"。
        assert_eq!(resolve_str_opt(&r.applies_to.name_contains, &params), None);
    }

    #[test]
    fn 工程覆盖优先于默认值() {
        let r = rule_with_params();
        let params = resolve_param_values(&r, &json!({ "min_fan_in": 5, "name_filter": "order" }));
        assert_eq!(resolve_num(&NumOrParam::Param("min_fan_in".into()), &params), 5);
        assert_eq!(
            resolve_str_opt(&r.applies_to.name_contains, &params),
            Some("order".to_string())
        );
        // 覆盖里没给的键仍然取默认。
        let defaults = resolve_param_values(&r, &json!({ "min_fan_in": 5 }));
        assert_eq!(resolve_str_opt(&r.applies_to.name_contains, &defaults), None);
    }

    #[test]
    fn 未声明的参数引用会被发现() {
        let r = rule_with_params();
        // `limit: "$max_nodes"` 引用了没声明的参数 —— 求值时退化成 0，
        // 候选集直接变空（规则静默 0 命中），必须在装载阶段暴露。
        assert_eq!(r.undeclared_params(), vec!["max_nodes".to_string()]);
    }

    #[test]
    fn 美元符号的字面量与引用可以区分() {
        // `$` 既是参数引用前缀，也可能是**名字本身的一部分**（PHP 变量、
        // 配置键名）。转义规则：`$$x` 表示字面量 `$x`，孤立的 `$` 也是字面量。
        let as_str = |v: serde_json::Value| -> StrOrParam { serde_json::from_value(v).unwrap() };
        assert!(matches!(as_str(json!("$key")), StrOrParam::Param(k) if k == "key"));
        assert!(matches!(as_str(json!("$$key")), StrOrParam::Str(s) if s == "$key"));
        assert!(matches!(as_str(json!("$")), StrOrParam::Str(s) if s == "$"));
        assert!(matches!(as_str(json!("plain")), StrOrParam::Str(s) if s == "plain"));
    }

    #[test]
    fn 文案里的参数占位符按生效值渲染() {
        let r = rule_with_params();
        let params = resolve_param_values(&r, &json!({ "min_fan_in": 8 }));
        let msg = r.render_with("users", "Table", None, None, None, None, Some(&params));
        assert!(msg.contains("入边 ≥ 8"), "实际文案: {msg}");
        // 不传参数表时保持原样（老调用点不受影响）。
        let raw = r.render("users", "Table", None, None, None, None);
        assert!(raw.contains("{param:min_fan_in}"), "实际文案: {raw}");
    }
}
