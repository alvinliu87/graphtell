//! Compliance rules (Check) — answering "does this codebase violate anything" on the built graph.
//!
//! # How this differs from the FKB `Rule`
//!
//! The FKB [`crate::model::Rule`] is a **synthesis rule**: it "adds things" to the graph during P4/P6 (tagging
//! annotations, synthesising semantic nodes). [`CheckRule`] in this module is a **check rule**: after the graph is
//! built it walks the graph read-only and produces "violations" ([`Violation`]).
//!
//! Both are declared in YAML and both are driven by knowledge outside the kernel — the kernel knows no concrete rule.
//!
//! # Why a violation lands as a `Diagnostic`
//!
//! [`Violation::to_diagnostic`] turns a violation into a diagnostic (code = `rule:<id>`). Diagnostics are already a
//! first-class product (they carry `severity` / `location` / `payload` and DiagnosticsPage renders them directly),
//! so reusing them means the rule engine needs **no new storage and no new page**.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::graph::{Diagnostic, Severity};
use crate::model::ids::{NodeId, ProjectId, SubProjectId};
use crate::model::kinds::Phase;

/// The code prefix of violation diagnostics (to distinguish them from build-time diagnostics and to clean up on re-run).
pub const RULE_CODE_PREFIX: &str = "rule:";

/// The phase a violation diagnostic belongs to.
pub fn check_phase() -> Phase {
    Phase(Phase::CHECK.to_string())
}

/// The prefix of a parameter reference: `$key` means "take the value of key from `params`".
///
/// The prefix must be explicit; guessing from "is it a pure number" is not acceptable — especially for string
/// parameters: `"50"` could be either a literal or a reference, and guessing wrong is a silently wrong result.
pub const PARAM_PREFIX: char = '$';

/// Parse a string that may be a "literal / parameter reference".
///
/// * `"$key"` -> reference to parameter `key`
/// * `"$$x"` -> the literal `$x` (escaping: one `$` is swallowed)
/// * `"$"` or `"50"` -> a literal (a lone `$` is not a reference)
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

/// Values a rule parameter can take: a literal, or a reference to a parameter declared in `params` (written `$key`).
///
/// Serialization is deliberately **not** untagged: untagged cannot tell a literal from a reference for strings (the
/// `Str` variant would swallow `"$x"`), while on the numeric side the `$` stays inside the key name, so
/// `params.get("$x")` never finds a value and silently degrades to 0.
/// The hand-written impl swallows the `$` prefix at **parse time**, leaving only a clean key inside `Param`.
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

/// Same as above, for string parameters (name prefixes, ignore lists, etc.).
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

/// The kinds of parameter a rule may expose for the user to tune.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    Number,
    String,
    Enum,
    Bool,
}

/// A tunable parameter a rule exposes to the user (declared under `params:` in YAML).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleParam {
    /// Parameter key (referenced as `$key` inside `when` / `applies_to`).
    pub key: String,
    /// Display name (for the UI).
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    pub kind: ParamKind,
    /// Default value (a JSON scalar matching `kind`).
    pub default: Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Candidate values when `kind = enum`.
    #[serde(default)]
    pub choices: Vec<String>,
}

/// A project-level config override for one rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRuleConfig {
    pub project_id: ProjectId,
    pub rule_id: String,
    /// `None` = inherit the global `enabled` from YAML; `Some(b)` = a project-level override.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Parameter overrides (`param_key -> value`); ones not overridden take the rule `params` default.
    #[serde(default)]
    pub options: Value,
}

/// A config write request (fields may be omitted = leave that item unchanged).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfigPatch {
    pub rule_id: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub options: Option<Value>,
}

/// The resolved parameter table: `param_key -> value` (global defaults + project overrides already merged).
pub type ParamValues = std::collections::HashMap<String, Value>;

/// Merge a rule's `params` defaults with the project overrides into a parameter table usable for evaluation.
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

/// Read a numeric parameter (a literal is returned directly, a reference is read from the table; missing falls back to 0).
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

/// Read a string parameter (a literal is returned directly, a reference is read from the table; missing falls back to an empty string).
pub fn resolve_str(v: &StrOrParam, params: &ParamValues) -> String {
    match v {
        StrOrParam::Str(s) => s.clone(),
        StrOrParam::Param(key) => match params.get(key) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        },
    }
}

/// Convenience parsing for `Option<StrOrParam>`: an empty string counts as `None`.
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

/// A check rule.
///
/// ```yaml
/// - id: http-contract-without-handler
///   title: HTTP contract without a handler
///   severity: error
///   category: correctness
///   applies_to: { kinds: [HttpContract] }
///   when:
///     - no_incoming: HandledBy
///   message: "contract {name} has no handler, calling it fails at runtime"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRule {
    /// Stable identifier, also the suffix of the diagnostic code (`rule:<id>`).
    pub id: String,
    /// Human-readable rule name.
    pub title: String,
    /// Description of the rule's intent (for the UI).
    #[serde(default)]
    pub description: Option<String>,
    /// Severity level when it matches.
    #[serde(default = "default_severity")]
    pub severity: Severity,
    /// Grouping tag (architecture / security / deadcode / contract …).
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The node scope the rule applies to (decides both the candidate set and the evaluation cost).
    #[serde(default)]
    pub applies_to: RuleScope,
    /// Parameters exposed for the user to tune (referenced as `$key` in `when` / `applies_to`).
    #[serde(default)]
    pub params: Vec<RuleParam>,
    /// Match condition: **all** must hold before it is a violation (an empty `when` matches everything in scope).
    #[serde(default)]
    pub when: Vec<CheckPredicate>,
    /// Violation copy; supports the placeholders `{name}` / `{kind}` / `{fqn}` / `{identity}` / `{file}` / `{line}`.
    pub message: String,
    /// Remediation advice.
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
    /// Diagnostic code: `rule:<id>`.
    pub fn code(&self) -> String {
        format!("{RULE_CODE_PREFIX}{}", self.id)
    }

    /// Graph facts the predicate depends on (derived automatically, see [`RuleRequirements`]).
    pub fn requirements(&self) -> RuleRequirements {
        let mut out = RuleRequirements::default();
        for p in &self.when {
            collect(p, &mut out);
        }
        out
    }

    /// Parameter keys referenced as `$key` in the predicate (deduplicated, order of appearance preserved).
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

    /// Parameter keys that are referenced but not declared in `params`.
    ///
    /// Such a reference silently degrades to `0` / `""` at evaluation time — used on a `limit` that makes
    /// **the candidate set empty outright** (the rule silently matches 0 times), exactly the failure mode this
    /// project most wants to avoid. So it has to be caught at load time, rather than waiting for a user to read
    /// "0 violations" as "the code is clean".
    pub fn undeclared_params(&self) -> Vec<String> {
        self.referenced_params()
            .into_iter()
            .filter(|k| !self.params.iter().any(|p| &p.key == k))
            .collect()
    }

    /// Whether the environment matches: **any** sub-project matching the language is enough, and likewise for
    /// frameworks.
    ///
    /// A project is often multi-language (CRMEB = php + javascript, litemall = java + javascript), so this is
    /// "a match exists" rather than "all match" — as long as there is a PHP sub-project, PHP rules should run.
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

    /// Render the violation copy.
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

    // There are 6 placeholders (node name / kind / FQN / identity / file / line), plus the parameter table makes 8;
    // splitting them into a struct would only make the call site harder to read, so the flat signature is kept here.
    #[allow(clippy::too_many_arguments)]

    /// Render the violation copy (with a parameter table).
    ///
    /// Besides node placeholders like `{name}` / `{file}`, this also supports `{param:key}` — writing the
    /// threshold the project tuned into the copy. Otherwise, after the user raises the "hot table threshold" to
    /// 200, the report still says ">= 50" and reads as if the rule had not taken effect.
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

/// The node scope a rule applies to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleScope {
    /// Node-kind allowlist; empty means no kind restriction (use sparingly, it is expensive).
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Name pre-filter (case-insensitive substring); can shrink the candidate set significantly. Supports `$param` references.
    #[serde(default)]
    pub name_contains: Option<StrOrParam>,
    /// Candidate-set cap, so a rule cannot scan the whole graph and stall a check. Supports `$param` references.
    #[serde(default = "default_scope_limit")]
    pub limit: NumOrParam,
    /// **Applicable language allowlist** (`php` / `java` / `javascript` / `typescript` …).
    ///
    /// Empty means language-agnostic — such a rule's predicate must depend only on **graph topology** (edge kinds /
    /// fan-in fan-out), because topology is normalised: both PHP and Java produce
    /// `HttpContract --HandledBy--> Method`.
    ///
    /// As soon as a rule's predicate depends on an annotation or edge produced by some FKB (e.g. the PHP event
    /// semantics `Triggers` / `Emits`), it must declare the language: a Java project has none of those edges, and
    /// the rule would report every event node as "nobody triggers it" — the environment gate exists to block
    /// exactly that kind of false positive.
    #[serde(default)]
    pub languages: Vec<String>,
    /// **Applicable framework allowlist** (`thinkphp` / `laravel` / `spring-boot` / `uni-app` …).
    ///
    /// A finer granularity than `languages`: even within one language, edge semantics can differ per framework.
    /// Empty means no framework restriction.
    #[serde(default)]
    pub frameworks: Vec<String>,
}

fn default_scope_limit() -> NumOrParam {
    NumOrParam::Num(20_000)
}

/// A check predicate.
///
/// The naming deliberately mirrors [`crate::model::Predicate`] (used for FKB synthesis), but the semantics are
/// **about the already-built graph**: edge kinds, fan-in / fan-out, annotations, properties — not call sites in
/// source code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckPredicate {
    /// The node kind is in the given set.
    KindIn(Vec<String>),
    /// The name contains the substring (case-insensitive). Supports `$param` references.
    NameContains(StrOrParam),
    /// The name starts with the given prefix (case-insensitive). Supports `$param` references.
    NameStartsWith(StrOrParam),
    /// The FQN contains the substring. Supports `$param` references.
    FqnContains(StrOrParam),
    /// The identity value contains the substring (e.g. `POST /api/xxx`). Supports `$param` references.
    IdentityContains(StrOrParam),
    /// Any of `name` / `fqn` / `identity` contains the substring. Supports `$param` references.
    TextContains(StrOrParam),
    /// The node carries an annotation of the given kind.
    HasAnnotation(String),
    /// The node does **not** carry an annotation of the given kind.
    NoAnnotation(String),
    /// A node property equals the given value (e.g. `side = frontend`).
    PropertyIs { name: String, value: String },
    /// The node lacks the given property.
    PropertyMissing(String),
    /// The scope chain lacks the given capability (e.g. `Authentication` / `RateLimiting`).
    NoCapability(Vec<String>),
    /// Fan-in (semantic in-edge count) is at least the threshold. Supports `$param` references.
    FanInGte(NumOrParam),
    /// Fan-in is at most the threshold (`0` = no semantic in-edges at all). Supports `$param` references.
    FanInLte(NumOrParam),
    /// Fan-out is at least the threshold. Supports `$param` references.
    FanOutGte(NumOrParam),
    /// **No** in-edge of the given kind (e.g. a contract has no `HandledBy`).
    NoIncoming(String),
    /// Has an in-edge of the given kind.

    HasIncoming(String),
    /// **No** out-edge of the given kind.
    NoOutgoing(String),
    /// Has an out-edge of the given kind.
    HasOutgoing(String),
    /// All hold.
    AllOf(Vec<CheckPredicate>),
    /// Any holds.
    AnyOf(Vec<CheckPredicate>),
    /// Negation.
    Not(Box<CheckPredicate>),
}

/// The graph facts a rule predicate depends on.
///
/// Derived **automatically** from the predicate by [`CheckRule::requirements`]; it need not be declared by hand in
/// YAML — a hand-written `requires` drifts away from `when` (you change `when` and forget `requires`), while the
/// derived one always agrees with the predicate.
///
/// # Why this has to exist
///
/// **A negative predicate is trivially true when the evidence does not exist.** When the graph has never had a
/// `Triggers` edge, `no_incoming: Triggers` holds for every node — so "nobody triggers it" matches every event
/// node. That is exactly the pair of traps this project has already fallen into (2683 phantom calls, 28 EventBus
/// nodes all false positives).
/// So before running a rule we must confirm: **the edges / annotations the predicate mentions really do occur in
/// the graph**.
#[derive(Debug, Clone, Default)]
pub struct RuleRequirements {
    /// Edge kinds used by the predicate (`HasIncoming` / `NoIncoming` / `HasOutgoing` / `NoOutgoing`).
    pub edges: Vec<String>,
    /// Annotation kinds used by the predicate (`HasAnnotation` / `NoAnnotation`).
    pub annotations: Vec<String>,
    /// Capabilities used by the predicate (`NoCapability`); requires a matching annotation on the `Capability` channel.
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

/// Recursively collect the parameter keys referenced inside a predicate.
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

/// Recursively collect the dependencies inside a predicate.
///
/// `Not` is collected as-is, without flipping polarity: this is a **conservative** check of "does the predicate
/// hold", and missing one block (failing to stop something that should have been stopped) is far better than
/// wrongly blocking a rule that could have run.
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

/// One matched violation.
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
    /// Diagnostic code (`rule:<id>`).
    pub fn code(&self) -> String {
        format!("{RULE_CODE_PREFIX}{}", self.rule_id)
    }

    /// A `path:line` locator string; `None` when there is no file information.
    pub fn location(&self) -> Option<String> {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => Some(format!("{f}:{l}")),
            (Some(f), None) => Some(f.clone()),
            _ => None,
        }
    }

    /// Land it as a diagnostic (reusing the existing first-class product channel).
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

    /// Restore from a diagnostic (used to rebuild a violation when the diagnostic table only has a payload).
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

/// A report of one rule check.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckReport {
    pub project_id: ProjectId,
    /// Total number of loaded rules (including disabled ones).
    pub rules_total: usize,
    /// Number of rules actually executed.
    pub rules_run: usize,
    /// Violations matched.
    pub violations: Vec<Violation>,
    /// Count by severity.
    pub by_severity: BTreeMap<String, u64>,
    /// Count by rule.
    pub by_rule: BTreeMap<String, u64>,
    /// Ids of rules that **ran but matched nothing**.
    ///
    /// Why this field exists: the most common way a rule fails is not a false positive but a **silent zero** — the
    /// predicate uses an annotation / edge that does not exist on the graph at all (e.g. `pii`,
    /// `Capability:Authentication`), so it never matches. A report of "0 violations" then reads as "the code is
    /// fine", which is far more dangerous than a false positive. Listing them explicitly means nobody can mistake
    /// "the rule went blind" for "the code is clean".
    pub rules_silent: Vec<String>,
    /// Rules **skipped because the environment does not match** (they declared `languages` / `frameworks` and this
    /// project lacks that stack).
    ///
    /// That is **expected behaviour**, not a fault: a PHP-only rule should not run on a pure Java project.
    /// They are listed separately to tell them apart from "silent zero" — both look like 0 matches, but they are
    /// completely different in nature.
    #[serde(default)]
    pub rules_not_applicable: Vec<String>,
    /// Rules **disabled because their predicate does not hold**: the edges / annotations / capabilities the
    /// predicate mentions do not occur once in this project's graph.
    ///
    /// Running such a rule would only produce **trivially-true false positives** (`no_incoming: X` holds for every
    /// node when X does not exist). Better not to run it than to report a pile of fakes.
    #[serde(default)]
    pub rules_unavailable: Vec<String>,
    pub duration_ms: u64,
}

impl CheckReport {
    /// Sort by severity descending, then by rule id (so the output is reproducible).
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
        // Go through deserialization once: the whole value of the parameter system is that "the declaration the rule author wrote can be read back correctly".
        let v = json!({
            "id": "hot-table",
            "title": "Hot table",
            "severity": "info",
            "category": "architecture",
            "params": [
                { "key": "min_fan_in", "label": "Fan-in threshold", "kind": "number", "default": 50 },
                { "key": "name_filter", "label": "Name filter", "kind": "string", "default": "" }
            ],
            "applies_to": {
                "kinds": ["Table"],
                "name_contains": "$name_filter",
                "limit": "$max_nodes"
            },
            "when": [{ "fan_in_gte": "$min_fan_in" }],
            "message": "table {name} is hot (in-edges >= {param:min_fan_in})"
        });
        serde_json::from_value(v).expect("the rule declaration should parse")
    }

    #[test]
    fn parameter_declaration_and_default_resolution() {
        let r = rule_with_params();
        assert_eq!(r.params.len(), 2);
        assert_eq!(r.params[0].key, "min_fan_in");
        assert!(matches!(r.params[0].kind, ParamKind::Number));

        let params = resolve_param_values(&r, &Value::Null);
        assert_eq!(resolve_num(&NumOrParam::Param("min_fan_in".into()), &params), 50);
        // A string parameter defaults to an empty string, so `name_contains` degenerates into "no filtering".
        assert_eq!(resolve_str_opt(&r.applies_to.name_contains, &params), None);
    }

    #[test]
    fn project_override_takes_priority_over_default() {
        let r = rule_with_params();
        let params = resolve_param_values(&r, &json!({ "min_fan_in": 5, "name_filter": "order" }));
        assert_eq!(resolve_num(&NumOrParam::Param("min_fan_in".into()), &params), 5);
        assert_eq!(
            resolve_str_opt(&r.applies_to.name_contains, &params),
            Some("order".to_string())
        );
        // Keys not given in the override still take the default.
        let defaults = resolve_param_values(&r, &json!({ "min_fan_in": 5 }));
        assert_eq!(resolve_str_opt(&r.applies_to.name_contains, &defaults), None);
    }

    #[test]
    fn undeclared_parameter_reference_is_detected() {
        let r = rule_with_params();
        // `limit: "$max_nodes"` references an undeclared parameter — it degrades to 0 at evaluation time and the
        // candidate set becomes empty outright (the rule silently matches 0 times); this must surface at load time.
        assert_eq!(r.undeclared_params(), vec!["max_nodes".to_string()]);
    }

    #[test]
    fn dollar_sign_literal_and_reference_are_distinguishable() {
        // `$` is both the parameter-reference prefix and possibly **part of a name itself** (a PHP variable, a
        // config key). Escaping rule: `$$x` means the literal `$x`, and a lone `$` is also a literal.
        let as_str = |v: serde_json::Value| -> StrOrParam { serde_json::from_value(v).unwrap() };
        assert!(matches!(as_str(json!("$key")), StrOrParam::Param(k) if k == "key"));
        assert!(matches!(as_str(json!("$$key")), StrOrParam::Str(s) if s == "$key"));
        assert!(matches!(as_str(json!("$")), StrOrParam::Str(s) if s == "$"));
        assert!(matches!(as_str(json!("plain")), StrOrParam::Str(s) if s == "plain"));
    }

    #[test]
    fn parameter_placeholders_in_copy_render_with_the_effective_value() {
        let r = rule_with_params();
        let params = resolve_param_values(&r, &json!({ "min_fan_in": 8 }));
        let msg = r.render_with("users", "Table", None, None, None, None, Some(&params));
        assert!(msg.contains("in-edges >= 8"), "actual copy: {msg}");
        // Without a parameter table, keep the original behaviour (old call sites are unaffected).

        let raw = r.render("users", "Table", None, None, None, None);
        assert!(raw.contains("{param:min_fan_in}"), "actual copy: {raw}");
    }
}
