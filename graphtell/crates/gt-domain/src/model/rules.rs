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
            // A literal that itself starts with `$` has to be **escaped on the way out**: written raw it comes
            // back in as a parameter reference (`Str("$x")` -> `"$x"` -> `Param("x")`), i.e. one round-trip
            // through storage / the API silently turns a literal into a reference.
            StrOrParam::Str(v) if v.starts_with(PARAM_PREFIX) => format!("{PARAM_PREFIX}{v}").serialize(s),
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
    /// A project is often multi-language (sample_project = php + javascript, litemall = java + javascript), so this is
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
    /// A node property contains the given substring (case-insensitive). Useful for matching a call-site's
    /// `snippet` / a fact node's `rhs` against a literal without enumerating exact values.
    PropertyContains { name: String, substring: String },
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

    // ===== Gaps the original 5 tests left open: numeric-param resolution, `requirements` derivation
    // (the defense against "negative predicate trivially true"), `referenced_params` / `applies_to_env`,
    // `CheckRule` defaults, predicate round-trips (incl. the CORS `property_contains`), and
    // `Violation` <-> `Diagnostic` round-trip. =====

    /// `NumOrParam`: a `$key` is a reference, a bare number is a literal; a reference resolves against the
    /// parameter table (number / numeric-string), and degrades to 0 when missing. The scope default cap is 20_000.
    #[test]
    fn num_or_param_resolves_against_the_table() {
        let num: NumOrParam = serde_json::from_value(json!(50)).unwrap();
        assert!(matches!(num, NumOrParam::Num(50)));
        let param: NumOrParam = serde_json::from_value(json!("$threshold")).unwrap();
        assert!(matches!(param, NumOrParam::Param(ref k) if k == "threshold"));

        let mut table = ParamValues::new();
        table.insert("threshold".into(), json!(80));
        assert_eq!(resolve_num(&param, &table), 80);
        // A numeric string in the table still counts.
        table.insert("threshold".into(), json!("120"));
        assert_eq!(resolve_num(&param, &table), 120);
        // Missing reference degrades to 0 (the silent-zero trap surface elsewhere, not here).
        assert_eq!(resolve_num(&param, &ParamValues::new()), 0);

        // The 20_000 cap is the *serde* default (used when `limit` is omitted from YAML); `Default::default()` of
        // the struct gives 0, so the documented cap only kicks in through deserialization.
        let scope: RuleScope = serde_json::from_value(json!({})).unwrap();
        assert!(matches!(scope.limit, NumOrParam::Num(20_000)), "the default limit is 20000 through serde");
        assert!(matches!(RuleScope::default().limit, NumOrParam::Num(0)), "derive Default gives 0 (only the serde path picks up default_scope_limit)");
    }

    /// `CheckRule` defaults: `severity = warning`, `category = general`, `enabled = true`, and an omitted
    /// `applies_to` gives an empty scope with the default limit.
    #[test]
    fn check_rule_defaults() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r1", "title": "Rule", "message": "broken {name}"
        }))
        .expect("minimal CheckRule must deserialize (message is required)");
        assert_eq!(r.severity, Severity::Warning);
        assert_eq!(r.category, "general");
        assert!(r.enabled);
        assert!(r.applies_to.kinds.is_empty());
        assert!(matches!(r.applies_to.limit, NumOrParam::Num(0)), "omitting applies_to ⇒ RuleScope::default(), limit=0; 20000 needs an explicit empty applies_to: {{}}");
        assert_eq!(r.code(), "rule:r1");
    }

    /// `requirements()` derives the graph facts a predicate depends on — recursively through `AllOf` / `AnyOf` /
    /// `Not` — so a rule that mentions an edge / annotation / capability the graph lacks can be skipped instead of
    /// reporting trivially-true false positives. Case-insensitive and de-duplicated.
    #[test]
    fn requirements_derive_edges_annotations_capabilities_recursively() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r",
            "title": "T",
            "message": "m",
            "when": [
                { "all_of": [
                    { "no_incoming": "Triggers" },
                    { "has_annotation": "pii" }
                ]},
                { "any_of": [
                    { "no_capability": ["Authentication", "RateLimiting"] }
                ]},
                { "not": { "has_outgoing": "callshttp" } }
            ]
        }))
        .unwrap();
        let req = r.requirements();
        assert!(req.edges.iter().any(|e| e.eq_ignore_ascii_case("Triggers")));
        assert!(req.edges.iter().any(|e| e.eq_ignore_ascii_case("CallsHttp")), "Not does not reduce polarity and is still collected");
        assert!(req.annotations.iter().any(|a| a.eq_ignore_ascii_case("pii")));
        assert!(req.capabilities.iter().any(|c| c.eq_ignore_ascii_case("Authentication")));
        assert!(req.capabilities.iter().any(|c| c.eq_ignore_ascii_case("RateLimiting")));
        assert!(!req.is_empty());
    }

    /// `referenced_params` walks `applies_to` (name_contains, limit) then `when`, de-duplicating while preserving
    /// first-seen order; `undeclared_params` is the subset not declared in `params`.
    #[test]
    fn referenced_and_undeclared_params_walk_scope_and_when() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r",
            "title": "T",
            "message": "m",
            "params": [ { "key": "declared", "label": "L", "kind": "number", "default": 1 },
                        { "key": "a", "label": "A", "kind": "string", "default": "" } ],
            "applies_to": { "name_contains": "$a", "limit": "$b" },
            "when": [
                { "all_of": [ { "fan_in_gte": "$a" }, { "name_contains": "$c" } ] }
            ]
        }))
        .unwrap();
        assert_eq!(r.referenced_params(), vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(r.undeclared_params(), vec!["b".to_string(), "c".to_string()], "only declared is declared");
    }

    /// `applies_to_env` is "a match exists" (multi-language projects), case-insensitive, on both the language and
    /// framework allowlists; empty allowlist means language-agnostic.
    #[test]
    fn applies_to_env_is_any_match_and_case_insensitive() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r", "title": "T", "message": "m",
            "applies_to": { "languages": ["php"], "frameworks": ["thinkphp"] }
        }))
        .unwrap();

        assert!(r.applies_to_env(&["php".into(), "javascript".into()], &["thinkphp".into()]),
            "an existing PHP sub-project matches");
        assert!(r.applies_to_env(&["PHP".into()], &["ThinkPHP".into()]), "case-insensitive");
        assert!(!r.applies_to_env(&["java".into()], &["thinkphp".into()]), "a language mismatch is skipped");
        assert!(!r.applies_to_env(&["php".into()], &["laravel".into()]), "a framework mismatch is skipped");

        // Empty allowlist = language-agnostic, always applicable.
        let any: CheckRule = serde_json::from_value(json!({ "id": "r", "title": "T", "message": "m" })).unwrap();
        assert!(any.applies_to_env(&[], &[]));
    }

    /// `CheckPredicate` round-trips its snake_case tags, including the `property_contains` variant added for the
    /// CORS detection; `$param` references become `StrOrParam::Param` / `NumOrParam::Param`.
    #[test]
    fn check_predicate_roundtrips_tags_and_param_refs() {
        let k: CheckPredicate = serde_json::from_value(json!({ "kind_in": ["Table", "View"] })).unwrap();
        assert!(matches!(k, CheckPredicate::KindIn(ref v) if v.len() == 2));

        // CORS: `property_contains { name, substring }`.
        let pc: CheckPredicate = serde_json::from_value(json!({
            "property_contains": { "name": "rhs", "substring": "origin" }
        }))
        .unwrap();
        assert!(matches!(pc, CheckPredicate::PropertyContains { name, substring }
            if name == "rhs" && substring == "origin"));

        let nc: CheckPredicate = serde_json::from_value(json!({ "name_contains": "$needle" })).unwrap();
        assert!(matches!(nc, CheckPredicate::NameContains(StrOrParam::Param(ref k)) if k == "needle"));

        let fi: CheckPredicate = serde_json::from_value(json!({ "fan_in_gte": "$t" })).unwrap();
        assert!(matches!(fi, CheckPredicate::FanInGte(NumOrParam::Param(ref k)) if k == "t"));

        let all: CheckPredicate = serde_json::from_value(json!({
            "all_of": [ { "no_incoming": "HandledBy" }, { "has_annotation": "pii" } ]
        }))
        .unwrap();
        assert!(matches!(all, CheckPredicate::AllOf(_)));

        let not: CheckPredicate = serde_json::from_value(json!({ "not": { "has_outgoing": "X" } })).unwrap();
        assert!(matches!(not, CheckPredicate::Not(_)));
    }

    /// `Violation` round-trips through `to_diagnostic` / `from_diagnostic`, and the basic copy placeholders
    /// (`{name}` / `{kind}` / `{fqn}` / `{identity}` / `{file}` / `{line}`) render.
    #[test]
    fn violation_diagnostic_roundtrip_and_location() {
        let v = Violation {
            project_id: ProjectId(1),
            rule_id: "r1".into(),
            title: "T".into(),
            category: "c".into(),
            severity: Severity::Error,
            node_id: NodeId(7),
            node_name: "users".into(),
            node_kind: "Table".into(),
            message: "table {name} ({kind}) at {file}:{line}".into(),
            remediation: Some("add index".into()),
            file: Some("a.php".into()),
            line: Some(10),
            sub_project_id: Some(SubProjectId(2)),
        };
        assert_eq!(v.code(), "rule:r1");
        assert_eq!(v.location().as_deref(), Some("a.php:10"));

        let d = v.to_diagnostic();
        assert_eq!(d.code, "rule:r1");
        assert_eq!(d.payload.get("rule_id").and_then(|x| x.as_str()), Some("r1"));

        let back = Violation::from_diagnostic(&d).expect("diagnostic must rebuild the violation");
        assert_eq!(back.node_id, NodeId(7));
        assert_eq!(back.rule_id, "r1");
        assert_eq!(back.file.as_deref(), Some("a.php"));
        assert_eq!(back.line, Some(10));
        assert_eq!(back.sub_project_id, Some(SubProjectId(2)));
    }

    /// The violation copy placeholders (`{name}` / `{kind}` / `{fqn}` / `{identity}` / `{file}` / `{line}`)
    /// are substituted by `CheckRule::render` (location info comes from the matched node).
    #[test]
    fn check_rule_render_fills_node_placeholders() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r",
            "title": "T",
            "message": "node {name} kind {kind} fqn {fqn} id {identity} at {file}:{line}"
        }))
        .unwrap();
        let out = r.render("orders", "Table", Some("db.orders"), Some("POST /api/orders"), Some("a.php"), Some(12));
        assert!(out.contains("node orders"));
        assert!(out.contains("kind Table"));
        assert!(out.contains("fqn db.orders"));
        assert!(out.contains("id POST /api/orders"));
        assert!(out.contains("at a.php:12"));
        // Missing optional fields degenerate to empty strings.
        let bare = r.render("orders", "Table", None, None, None, None);
        assert!(bare.contains("fqn  "));
        assert!(bare.contains("at "));
    }

    /// `CheckReport::sort_violations` orders by severity desc, then rule id, then node name — for reproducible output.
    #[test]
    fn check_report_sorts_by_severity_then_rule() {
        let mut rep = CheckReport {
            project_id: ProjectId(1),
            ..Default::default()
        };
        rep.violations.push(Violation {
            project_id: ProjectId(1), rule_id: "b".into(), title: "B".into(), category: "c".into(),
            severity: Severity::Warning, node_id: NodeId(2), node_name: "n2".into(), node_kind: "K".into(),
            message: "m".into(), remediation: None, file: None, line: None, sub_project_id: None,
        });
        rep.violations.push(Violation {
            project_id: ProjectId(1), rule_id: "a".into(), title: "A".into(), category: "c".into(),
            severity: Severity::Error, node_id: NodeId(1), node_name: "n1".into(), node_kind: "K".into(),
            message: "m".into(), remediation: None, file: None, line: None, sub_project_id: None,
        });
        rep.violations.push(Violation {
            project_id: ProjectId(1), rule_id: "c".into(), title: "C".into(), category: "c".into(),
            severity: Severity::Warning, node_id: NodeId(3), node_name: "n3".into(), node_kind: "K".into(),
            message: "m".into(), remediation: None, file: None, line: None, sub_project_id: None,
        });
        rep.sort_violations();
        // Error ("a") first, then the two Warnings in rule-id order ("b" then "c").
        assert_eq!(rep.violations[0].rule_id, "a");
        assert_eq!(rep.violations[1].rule_id, "b");
        assert_eq!(rep.violations[2].rule_id, "c");
    }

    // ===== Third pass: what the 14 tests above still leave open — the parameter **declaration** types
    // (`ParamKind` / `RuleParam`), the two project-config DTOs, the `$`-escaping round-trip through storage,
    // `resolve_str` / `render_with` fallback paths, every `CheckPredicate` variant having a YAML spelling, and
    // the `Violation` <-> `Diagnostic` conversion on *malformed* input. =====

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
        serde_json::from_value(serde_json::to_value(v).expect("serialize")).expect("deserialize")
    }

    /// `$$x` is the documented escape for the literal `$x` — but writing it out **unescaped** would read back as
    /// a reference to parameter `x`, so serialization has to re-escape. This is the round-trip fidelity of every
    /// rule that is stored and re-read (API edit, re-export, snapshot).
    #[test]
    fn str_or_param_escapes_dollar_literals_so_a_round_trip_is_stable() {
        let cases = [
            (StrOrParam::Str("$x".into()), json!("$$x")),
            (StrOrParam::Str("$$x".into()), json!("$$$x")),
            (StrOrParam::Str("plain".into()), json!("plain")),
            (StrOrParam::Str("".into()), json!("")),
            (StrOrParam::Param("needle".into()), json!("$needle")),
        ];
        for (v, wire) in cases {
            assert_eq!(serde_json::to_value(&v).unwrap(), wire, "the written form of {v:?}");
            assert!(matches!(&serde_json::from_value::<StrOrParam>(wire.clone()).unwrap(), got if std::mem::discriminant(got) == std::mem::discriminant(&v)));
            // The invariant itself: writing then reading back yields the same value.
            assert!(matches!(round_trip(&v), _), "the round-trip must be parsable again");
        }
        let again: StrOrParam = round_trip(&StrOrParam::Str("$x".into()));
        assert!(matches!(again, StrOrParam::Str(ref s) if s == "$x"), "a literal must not be silently turned into a parameter reference");
        let again: StrOrParam = round_trip(&StrOrParam::Param("x".into()));
        assert!(matches!(again, StrOrParam::Param(ref k) if k == "x"));

        // Non-string scalars are accepted and kept as text rather than rejected.
        assert!(matches!(
            serde_json::from_value::<StrOrParam>(json!(true)).unwrap(),
            StrOrParam::Str(ref s) if s == "true"
        ));
        assert!(matches!(
            serde_json::from_value::<StrOrParam>(json!(null)).unwrap(),
            StrOrParam::Str(ref s) if s == "null"
        ));
    }

    /// Anything that is not a natural number degrades to `0` — a silent degradation, pinned so it stays a
    /// deliberate choice rather than an accident somebody "fixes" into a panic.
    #[test]
    fn num_or_param_degrades_non_numeric_input_to_zero() {
        let n: NumOrParam = serde_json::from_value(json!(-1)).unwrap();
        assert!(matches!(n, NumOrParam::Num(0)), "a negative number degrades to 0");
        assert!(matches!(serde_json::from_value::<NumOrParam>(json!(1.5)).unwrap(), NumOrParam::Num(0)));
        assert!(matches!(serde_json::from_value::<NumOrParam>(json!(true)).unwrap(), NumOrParam::Num(0)));
        assert!(matches!(serde_json::from_value::<NumOrParam>(json!(null)).unwrap(), NumOrParam::Num(0)));
        // A numeric **string** is still a number (YAML authors quote it by accident).
        assert!(matches!(serde_json::from_value::<NumOrParam>(json!("42")).unwrap(), NumOrParam::Num(42)));
        assert!(matches!(
            serde_json::from_value::<NumOrParam>(json!("not-a-number")).unwrap(),
            NumOrParam::Num(0)
        ));
    }

    /// The declaration side of the parameter system: `ParamKind` spelling, and which `RuleParam` fields are
    /// mandatory (a UI-served declaration with a missing `default` would silently evaluate as nothing).
    #[test]
    fn param_kind_and_rule_param_declaration_contract() {
        for (kind, text) in [
            (ParamKind::Number, "number"),
            (ParamKind::String, "string"),
            (ParamKind::Enum, "enum"),
            (ParamKind::Bool, "bool"),
        ] {
            assert_eq!(serde_json::to_value(&kind).unwrap(), json!(text), "the serialised form of {text}");
            let back: ParamKind = serde_json::from_value(json!(text)).unwrap();
            assert!(
                std::mem::discriminant(&back) == std::mem::discriminant(&kind),
                "{text} must be read back as the same variant"
            );
        }
        assert!(serde_json::from_value::<ParamKind>(json!("int")).is_err());

        let p: RuleParam = serde_json::from_value(json!({
            "key": "min_fan_in", "label": "Fan-in threshold", "kind": "number", "default": 50
        }))
        .unwrap();
        assert_eq!(p.key, "min_fan_in");
        assert!(p.description.is_none());
        assert!(p.min.is_none() && p.max.is_none());
        assert!(p.choices.is_empty());

        let full: RuleParam = serde_json::from_value(json!({
            "key": "level", "label": "Level", "description": "d", "kind": "enum",
            "default": "warn", "min": 0.0, "max": 10.0, "choices": ["warn", "error"]
        }))
        .unwrap();
        let back = round_trip(&full);
        assert_eq!(back.description.as_deref(), Some("d"));
        assert_eq!(back.min, Some(0.0));
        assert_eq!(back.max, Some(10.0));
        assert_eq!(back.choices, vec!["warn".to_string(), "error".to_string()]);

        // `label` / `kind` / `default` are the floor: without them the UI has nothing to render.
        assert!(serde_json::from_value::<RuleParam>(json!({ "key": "k" })).is_err());
        assert!(serde_json::from_value::<RuleParam>(json!({ "key": "k", "label": "L", "kind": "number" })).is_err());
    }

    /// The two DTOs the rules API accepts / stores. In both, `Option` means "not decided": for
    /// `ProjectRuleConfig.enabled` specifically, `None` is **inherit from YAML**, which is different from false.
    #[test]
    fn project_rule_config_and_patch_keep_none_and_false_apart() {
        let c: ProjectRuleConfig = serde_json::from_value(json!({
            "project_id": 1, "rule_id": "hot-table"
        }))
        .unwrap();
        assert!(c.enabled.is_none(), "None = inherit the global YAML switch");
        assert_eq!(c.options, Value::Null);

        let explicit: ProjectRuleConfig = serde_json::from_value(json!({
            "project_id": 1, "rule_id": "hot-table", "enabled": false, "options": { "min_fan_in": 5 }
        }))
        .unwrap();
        assert_eq!(explicit.enabled, Some(false), "an explicit off must be distinguishable from 'inherit'");
        let back = round_trip(&explicit);
        assert_eq!(back.enabled, Some(false));
        assert_eq!(back.options, json!({ "min_fan_in": 5 }));

        let patch: RuleConfigPatch = serde_json::from_value(json!({ "rule_id": "hot-table" })).unwrap();
        assert!(patch.enabled.is_none() && patch.options.is_none());
        let patch: RuleConfigPatch = serde_json::from_value(json!({
            "rule_id": "hot-table", "enabled": true, "options": null
        }))
        .unwrap();
        assert_eq!(patch.enabled, Some(true));
        assert!(patch.options.is_none(), "a null options is the same as not submitting it");
        assert!(serde_json::from_value::<RuleConfigPatch>(json!({})).is_err(), "rule_id is required");
    }

    /// Resolution edges: a reference only reads **strings**; `resolve_str_opt` maps the empty string to `None`
    /// because an empty name filter means "no filtering", not "match the empty name".
    #[test]
    fn string_resolution_reads_strings_only_and_empty_means_none() {
        let mut table = ParamValues::new();
        table.insert("name".into(), json!("order"));
        table.insert("numeric".into(), json!(7));
        table.insert("other".into(), json!({ "a": 1 }));

        assert_eq!(resolve_str(&StrOrParam::Param("name".into()), &table), "order");
        assert_eq!(
            resolve_str(&StrOrParam::Param("numeric".into()), &table),
            "",
            "when the parameter is a number the string parse yields nothing and degrades to empty"
        );
        assert_eq!(resolve_str(&StrOrParam::Param("missing".into()), &table), "");
        assert_eq!(resolve_str(&StrOrParam::Str("literal".into()), &table), "literal");

        assert_eq!(resolve_str_opt(&None, &table), None);
        assert_eq!(resolve_str_opt(&Some(StrOrParam::Str(String::new())), &table), None);
        assert_eq!(resolve_str_opt(&Some(StrOrParam::Param("other".into())), &table), None);
        assert_eq!(resolve_str_opt(&Some(StrOrParam::Param("name".into())), &table), Some("order".into()));
    }

    /// `{param:key}` falls back to the **declared default** when the effective table has no value for it — so a
    /// report never claims a threshold that was not actually applied. A placeholder for a parameter the rule
    /// does not declare is left verbatim (better visible than silently empty).
    #[test]
    fn render_with_falls_back_to_the_declared_default() {
        let r = rule_with_params();
        let empty_table = ParamValues::new();
        let out = r.render_with("users", "Table", None, None, None, None, Some(&empty_table));
        assert!(out.contains("in-edges >= 50"), "a missing value falls back to the declared default: {out}");

        let undeclared: CheckRule = serde_json::from_value(json!({
            "id": "r", "title": "T", "message": "limit {param:nope}"
        }))
        .unwrap();
        let out = undeclared.render_with("n", "K", None, None, None, None, Some(&empty_table));
        assert!(out.contains("{param:nope}"), "an undeclared parameter placeholder is kept verbatim: {out}");
    }

    /// Every `CheckPredicate` variant must keep a parseable YAML spelling — this list doubles as the inventory:
    /// adding a variant means adding one row.
    #[test]
    fn every_check_predicate_variant_has_a_yaml_spelling() {
        let cases: Vec<Value> = vec![
            json!({ "kind_in": ["Table"] }),
            json!({ "name_contains": "foo" }),
            json!({ "name_starts_with": "get" }),
            json!({ "fqn_contains": "controller" }),
            json!({ "identity_contains": "POST /" }),
            json!({ "text_contains": "order" }),
            json!({ "has_annotation": "pii" }),
            json!({ "no_annotation": "pii" }),
            json!({ "property_is": { "name": "side", "value": "frontend" } }),
            json!({ "property_contains": { "name": "rhs", "substring": "origin" } }),
            json!({ "property_missing": "columns" }),
            json!({ "no_capability": ["Authentication"] }),
            json!({ "fan_in_gte": 5 }),
            json!({ "fan_in_lte": 0 }),
            json!({ "fan_out_gte": "$fan_out" }),
            json!({ "no_incoming": "HandledBy" }),
            json!({ "has_incoming": "CallsHttp" }),
            json!({ "no_outgoing": "ReadsDb" }),
            json!({ "has_outgoing": "Calls" }),
            json!({ "all_of": [{ "kind_in": ["Table"] }] }),
            json!({ "any_of": [{ "kind_in": ["Table"] }] }),
            json!({ "not": { "has_outgoing": "Calls" } }),
        ];
        for c in cases {
            let p: CheckPredicate =
                serde_json::from_value(c.clone()).unwrap_or_else(|e| panic!("{c} must deserialize: {e}"));
            // Each one also has to write back out, so re-exporting / re-reading a rule is possible.
            let _ = round_trip(&p);
        }
        // `property_contains` needs both halves — half a condition would match nothing or everything.
        assert!(serde_json::from_value::<CheckPredicate>(json!({ "property_is": { "name": "side" } })).is_err());
        assert!(serde_json::from_value::<CheckPredicate>(json!({ "made_up": 1 })).is_err());
    }

    /// `Violation::location` tolerates partial information (a file without a line is legitimate), and absent
    /// optional fields are **omitted** from the wire rather than written as `null`.
    #[test]
    fn violation_location_tolerates_partial_file_information() {
        let base = Violation {
            project_id: ProjectId(1),
            rule_id: "r".into(),
            title: "T".into(),
            category: "c".into(),
            severity: Severity::Info,
            node_id: NodeId(1),
            node_name: "n".into(),
            node_kind: "K".into(),
            message: "m".into(),
            remediation: None,
            file: None,
            line: None,
            sub_project_id: None,
        };
        assert_eq!(base.location(), None, "no file means no location");
        assert_eq!(Violation { line: Some(3), ..base.clone() }.location(), None, "a line number without a file path still cannot be located");
        assert_eq!(Violation { file: Some("a.php".into()), ..base.clone() }.location().as_deref(), Some("a.php"));

        let v = serde_json::to_value(&base).unwrap();
        for f in ["remediation", "file", "line", "sub_project_id"] {
            assert!(v.get(f).is_none(), "{f} must not be written when it is None");
        }
    }

    /// The conversion is the bridge to the diagnostic product; it has to be lossless **and** refuse foreign
    /// payloads rather than inventing defaults (a silently rebuilt violation would look like a rule matched).
    #[test]
    fn violation_diagnostic_conversion_is_lossless_and_refuses_foreign_payloads() {
        let v = Violation {
            project_id: ProjectId(3),
            rule_id: "no-handler".into(),
            title: "No handler".into(),
            category: "correctness".into(),
            severity: Severity::Critical,
            node_id: NodeId(11),
            node_name: "Login/appleLogin".into(),
            node_kind: "HttpContract".into(),
            message: "no handler".into(),
            remediation: Some("declare Route::post".into()),
            file: Some("app/api/route/pc.php".into()),
            line: Some(42),
            sub_project_id: Some(SubProjectId(9)),
        };
        let d = v.to_diagnostic();
        assert_eq!(d.code, format!("{RULE_CODE_PREFIX}no-handler"));
        assert_eq!(d.phase, check_phase(), "a violation diagnostic belongs to the Check phase");
        assert_eq!(d.phase.as_str(), Phase::CHECK);
        assert_eq!(d.location.as_deref(), Some("app/api/route/pc.php:42"));

        let back = Violation::from_diagnostic(&d).expect("it must be restorable from the diagnostic");
        assert_eq!(back.project_id, v.project_id);
        assert_eq!(back.rule_id, v.rule_id);
        assert_eq!(back.title, v.title);
        assert_eq!(back.category, v.category);
        assert_eq!(back.severity, Severity::Critical);
        assert_eq!(back.node_id, NodeId(11));
        assert_eq!(back.node_name, v.node_name);
        assert_eq!(back.node_kind, v.node_kind);
        assert_eq!(back.message, v.message);
        assert_eq!(back.remediation.as_deref(), Some("declare Route::post"));

        // Foreign / truncated payloads must yield None instead of a half-built violation.
        assert!(Violation::from_diagnostic(&Diagnostic {
            payload: Value::Null,
            ..d.clone()
        })
        .is_none());
        assert!(Violation::from_diagnostic(&Diagnostic { payload: json!({}), ..d.clone() }).is_none());
        assert!(Violation::from_diagnostic(&Diagnostic {
            payload: json!({ "rule_id": "r" }),
            ..d.clone()
        })
        .is_none(), "a missing node_id refuses to restore");
    }

    /// `CheckReport` counts start at zero (a non-zero default would invent statistics), and the ordering is
    /// total: severity desc, then rule id, then node name, including Critical / Info at the ends.
    #[test]
    fn check_report_defaults_are_zero_and_ordering_is_total() {
        let r = CheckReport::default();
        assert_eq!(r.rules_total, 0);
        assert_eq!(r.rules_run, 0);
        assert!(r.violations.is_empty());
        assert!(r.by_severity.is_empty() && r.by_rule.is_empty());
        assert!(r.rules_silent.is_empty());
        assert!(r.rules_not_applicable.is_empty());
        assert!(r.rules_unavailable.is_empty());
        assert_eq!(r.duration_ms, 0);

        let mk = |rule: &str, name: &str, sev: Severity| Violation {
            project_id: ProjectId(1),
            rule_id: rule.into(),
            title: "T".into(),
            category: "c".into(),
            severity: sev,
            node_id: NodeId(1),
            node_name: name.into(),
            node_kind: "K".into(),
            message: "m".into(),
            remediation: None,
            file: None,
            line: None,
            sub_project_id: None,
        };
        let mut rep = CheckReport { violations: vec![
            mk("z", "b-node", Severity::Info),
            mk("z", "a-node", Severity::Info),
            mk("a", "x", Severity::Critical),
            mk("m", "y", Severity::Warning),
        ], ..Default::default() };
        rep.sort_violations();
        let order: Vec<_> = rep.violations.iter().map(|v| (v.severity, v.rule_id.as_str(), v.node_name.as_str())).collect();
        assert_eq!(
            order,
            vec![
                (Severity::Critical, "a", "x"),
                (Severity::Warning, "m", "y"),
                (Severity::Info, "z", "a-node"),
                (Severity::Info, "z", "b-node"),
            ],
            "the ordering must be fully deterministic: severity descending → rule id → node name"
        );
    }

    // ===== Fourth pass: residual contract gaps — `RuleRequirements` dedup / case-insensitivity / `is_empty`
    // (the gate the engine uses to decide a rule can run), `referenced_params` covering every string / numeric
    // parameter variant (incl. nested `not` / `all_of` / `any_of`), the serde round-trip of the persisted
    // records `Violation` and `CheckReport` (None omission + the asymmetric `#[serde(default)]` on the two
    // "skipped" lists), `render_with` substitution for non-number parameter values, and the `Default` impls of
    // the two parameter enums. =====

    /// `RuleRequirements` de-duplicates edges / annotations / capabilities **case-insensitively** (an FKB author
    /// writing `Triggers` vs `triggers` must not split the evidence set), and `is_empty` is the gate the engine
    /// uses to decide "this rule can still run".
    #[test]
    fn rule_requirements_dedup_case_insensitive_and_is_empty() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r", "title": "T", "message": "m",
            "when": [
                { "no_incoming": "Triggers" },
                { "has_incoming": "triggers" },   // the same edge with different casing -> de-duplicated
                { "no_incoming": "Triggers" },     // an exact duplicate -> de-duplicated
                { "has_annotation": "pii" },
                { "no_annotation": "PII" },        // the same annotation with different casing -> de-duplicated
                { "no_capability": ["Auth", "auth"] }
            ]
        }))
        .unwrap();
        let req = r.requirements();
        assert_eq!(req.edges.len(), 1, "edges are de-duplicated case-insensitively");
        assert_eq!(req.annotations.len(), 1, "annotations are de-duplicated case-insensitively");
        assert_eq!(req.capabilities.len(), 1);
        assert!(!req.is_empty());

        // A predicate-free rule depends on nothing and must report empty (so the engine can still run it).
        let empty: CheckRule = serde_json::from_value(json!({ "id": "e", "title": "T", "message": "m" })).unwrap();
        assert!(empty.requirements().is_empty());
        assert!(RuleRequirements::default().is_empty());
    }

    /// `referenced_params` must collect a `$key` from **every** string-bearing variant (`name_starts_with` /
    /// `fqn_contains` / `identity_contains` / `text_contains`), the two numeric variants (`fan_in_lte` /
    /// `fan_out_gte`), and recursively through `not` / `all_of` / `any_of`. The existing cases only exercised
    /// `name_contains`, which is exactly the gap that lets a silently-undeclared param slip through.
    #[test]
    fn referenced_params_collects_every_string_and_numeric_variant() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r", "title": "T", "message": "m",
            "params": [ { "key": "declared", "label": "L", "kind": "string", "default": "" } ],
            "applies_to": { "name_contains": "$scope_name", "limit": "$scope_limit" },
            "when": [
                { "name_starts_with": "$p_nsw" },
                { "fqn_contains": "$p_fqn" },
                { "identity_contains": "$p_ident" },
                { "text_contains": "$p_text" },
                { "fan_in_lte": "$p_fil" },
                { "fan_out_gte": "$p_fog" },
                { "not": { "name_contains": "$p_not" } },
                { "all_of": [ { "any_of": [ { "name_contains": "$p_nested" } ] } ] }
            ]
        }))
        .unwrap();
        let refs = r.referenced_params();
        for expected in [
            "scope_name", "scope_limit", "p_nsw", "p_fqn", "p_ident", "p_text",
            "p_fil", "p_fog", "p_not", "p_nested",
        ] {
            assert!(refs.iter().any(|x| x == expected), "missed collecting {expected}: {refs:?}");
        }
        // `declared` is never referenced, so every collected key is undeclared.
        let undeclared: std::collections::HashSet<_> = r.undeclared_params().into_iter().collect();
        assert_eq!(undeclared.len(), 10);
        assert!(!undeclared.contains("declared"));
    }

    /// `Violation` is a persisted / transmitted record: `Option` fields marked `skip_serializing_if` must be
    /// omitted from the wire, and a full / partial violation must round-trip exactly (otherwise a stored
    /// violation silently drops its `remediation` / `sub_project_id`).
    #[test]
    fn violation_serde_round_trip_skips_none_and_round_trips() {
        let v = Violation {
            project_id: ProjectId(5),
            rule_id: "r5".into(),
            title: "T5".into(),
            category: "security".into(),
            severity: Severity::Warning,
            node_id: NodeId(42),
            node_name: "login".into(),
            node_kind: "Method".into(),
            message: "m".into(),
            remediation: None,
            file: None,
            line: None,
            sub_project_id: None,
        };
        let wire = serde_json::to_value(&v).unwrap();
        for f in ["remediation", "file", "line", "sub_project_id"] {
            assert!(wire.get(f).is_none(), "{f} is not written when None");
        }
        let back: Violation = serde_json::from_value(wire).unwrap();
        assert_eq!(back.project_id, v.project_id);
        assert_eq!(back.rule_id, v.rule_id);
        assert_eq!(back.node_id, v.node_id);
        assert_eq!(back.remediation, None);
        assert_eq!(back.file, None);
        assert_eq!(back.line, None);
        assert_eq!(back.sub_project_id, None);

        // A fully-populated violation also round-trips, and the None-skip fields are present on the wire.
        let full = Violation {
            remediation: Some("fix".into()),
            file: Some("a.php".into()),
            line: Some(7),
            sub_project_id: Some(SubProjectId(3)),
            ..v.clone()
        };
        let wire_full = serde_json::to_value(&full).unwrap();
        for f in ["remediation", "file", "line", "sub_project_id"] {
            assert!(wire_full.get(f).is_some(), "{f} must be written when not None");
        }
        let back: Violation = serde_json::from_value(wire_full).unwrap();
        assert_eq!(back.remediation.as_deref(), Some("fix"));
        assert_eq!(back.file.as_deref(), Some("a.php"));
        assert_eq!(back.line, Some(7));
        assert_eq!(back.sub_project_id, Some(SubProjectId(3)));
    }

    /// `CheckReport` is the persisted summary. The two "skipped" lists (`rules_not_applicable` /
    /// `rules_unavailable`) carry `#[serde(default)]` and must survive a round-trip *and* default to empty when
    /// omitted on the wire — while `rules_silent` carries no default and is therefore **required** on the wire.
    #[test]
    fn check_report_serde_round_trip_keeps_default_lists_empty() {
        let rep = CheckReport {
            project_id: ProjectId(2),
            rules_total: 3,
            rules_run: 2,
            violations: vec![],
            by_severity: BTreeMap::new(),
            by_rule: BTreeMap::new(),
            rules_silent: vec![],
            rules_not_applicable: vec!["php-only".into()],
            rules_unavailable: vec!["needs-pii".into()],
            duration_ms: 11,
        };
        let back: CheckReport = round_trip(&rep);
        assert_eq!(back.rules_not_applicable, vec!["php-only".to_string()]);
        assert_eq!(back.rules_unavailable, vec!["needs-pii".to_string()]);

        // When the default-bearing lists are omitted on the wire they deserialize to empty (not absent / panic).
        // `rules_silent` (no default) must still be present for deserialize to succeed.
        let minimal: CheckReport = serde_json::from_value(json!({
            "project_id": 2, "rules_total": 0, "rules_run": 0, "violations": [],
            "by_severity": {}, "by_rule": {}, "rules_silent": [], "duration_ms": 0
        }))
        .unwrap();
        assert!(minimal.rules_not_applicable.is_empty());
        assert!(minimal.rules_unavailable.is_empty());
    }

    /// `render_with` substitutes `{param:key}` for **non-number** effective values too — a `bool` renders as its
    /// text, a `string` verbatim, and any other JSON value as its `to_string()` — so a tuned toggle / label shows
    /// up in the copy rather than a stale default.
    #[test]
    fn render_with_substitutes_bool_string_and_other_param_values() {
        let r: CheckRule = serde_json::from_value(json!({
            "id": "r", "title": "T",
            "params": [
                { "key": "b", "label": "B", "kind": "bool", "default": false },
                { "key": "s", "label": "S", "kind": "string", "default": "" },
                { "key": "o", "label": "O", "kind": "string", "default": "" }
            ],
            "message": "enable {param:b} level {param:s} meta {param:o}"
        }))
        .unwrap();
        let mut table = ParamValues::new();
        table.insert("b".into(), json!(true));
        table.insert("s".into(), json!("high"));
        table.insert("o".into(), json!({ "k": 1 }));
        let out = r.render_with("n", "K", None, None, None, None, Some(&table));
        assert!(out.contains("enable true"), "actual: {out}");
        assert!(out.contains("level high"), "actual: {out}");
        assert!(out.contains("meta {\"k\":1}"), "actual: {out}");
    }

    /// The Rust-side `Default` of the two parameter enums is the literal / zero baseline, and must not drift from
    /// the value the engine treats as "no parameter" (e.g. `resolve_num`/`resolve_str` fall back to 0 / "").
    #[test]
    fn param_enum_defaults_are_the_literal_baseline() {
        assert!(matches!(NumOrParam::default(), NumOrParam::Num(0)));
        assert!(matches!(StrOrParam::default(), StrOrParam::Str(ref s) if s.is_empty()));
    }
}
