//! The compliance-check use case: run [`CheckRule`] on the graph to produce [`Violation`].
//!
//! # Position
//!
//! This is a **read-only** use case after graph building: it changes no nodes, no edges, only writes `rule:*` violations to the diagnostics table.
//! The reason it lands directly as a diagnostic is that diagnostics are already a first-class product (with severity / location / payload,
//! the frontend DiagnosticsPage renders them directly) — so the rules engine needs no new storage and no new page.
//!
//! # Why preload rather than query the DB node by node
//!
//! One rule must judge "does this node have a certain annotation / a certain edge" for thousands of nodes.
//! Per-node `annotations_of` / `edges_of` is N+1 round-trips (tens of thousands of nodes => second-level jitter), so here we load annotations,
//! in-edges, out-edges and file paths into memory once, and evaluation makes zero round-trips throughout.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use gt_domain::error::Result;
use gt_domain::model::{
    Annotation, CheckPredicate, CheckReport, CheckRule, Diagnostic, Edge, Node, NodeId, NodeKind,
    ParamValues, ProjectId, ProjectRuleConfig, RuleConfigPatch, RuleRequirements, Severity,
    SubProjectId, Violation, resolve_num, resolve_param_values, resolve_str, resolve_str_opt,
    RULE_CODE_PREFIX,
};
use serde_json::Value;
use gt_domain::model::kinds::{is_semantic_edge, AnnotationChannel};
use gt_domain::port::{NodeFilter, Persistence, RuleProvider};

// ---------------------------------------------------------------- Rules engine

/// The check service.
pub struct RuleService {
    store: Arc<dyn Persistence>,
    rules: Arc<dyn RuleProvider>,
}

impl RuleService {
    pub fn new(store: Arc<dyn Persistence>, rules: Arc<dyn RuleProvider>) -> Self {
        Self { store, rules }
    }

    /// All loaded rules.
    pub fn rules(&self) -> Vec<CheckRule> {
        self.rules.rules().to_vec()
    }

    /// Run the check on a project.
    ///
    /// * `only` — run only rules with the given ids; empty means run all enabled rules.
    /// * `persist` — whether to write violations back to the diagnostics table (can be turned off for a CLI preview).
    pub fn check(
        &self,
        project_id: ProjectId,
        only: Option<&[String]>,
        persist: bool,
    ) -> Result<CheckReport> {
        let started = Instant::now();
        let all = self.rules.rules();
        // Project-level rule config: overrides the global enabled flag and the rule parameters (options).
        let configs = self.store.get_rule_configs(project_id)?;
        // Effective enabled state: config override ?? YAML global default.
        let is_enabled = |r: &CheckRule| -> bool {
            configs.get(&r.id).and_then(|c| c.enabled).unwrap_or(r.enabled)
        };
        let selected: Vec<&CheckRule> = all
            .iter()
            .filter(|r| match only {
                // When only certain rules are explicitly named (e.g. "run only this rule"), ignore the enabled switch,
                // because that is the user's temporary preview intent.
                Some(ids) => ids.iter().any(|id| id == &r.id),
                None => is_enabled(r),
            })
            .collect();

        let env = ProjectEnv::load(&*self.store, project_id)?;
        let mut runnable: Vec<(&CheckRule, ParamValues)> = Vec::new();
        let mut not_applicable: Vec<String> = Vec::new();
        let mut unavailable: Vec<String> = Vec::new();
        // Rules whose environment matches but whose predicate does not hold: on an explicit re-run, clear their old violations too,
        // otherwise "could not run this time" would be read as "the last result is the current one".
        let mut unavailable_rules: Vec<&CheckRule> = Vec::new();
        for rule in selected {
            // Merge the global default and the project override to get this rule's final parameters.
            let opts = configs
                .get(&rule.id)
                .map(|c| c.options.clone())
                .unwrap_or(Value::Null);
            let params = resolve_param_values(rule, &opts);
            if !rule.applies_to_env(&env.languages, &env.frameworks) {
                not_applicable.push(format!(
                    "{} (needs {}, this project is {})",
                    rule.id,
                    rule.applies_to.languages.join("/"),
                    env.describe()
                ));
                continue;
            }
            if let Some(why) = env.missing_requirement(&rule.requirements()) {
                unavailable.push(format!("{}（{}）", rule.id, why));
                unavailable_rules.push(rule);
                continue;
            }
            runnable.push((rule, params));
        }

        let mut report = CheckReport {
            project_id,
            rules_total: all.len(),
            rules_run: runnable.len(),
            rules_not_applicable: not_applicable,
            rules_unavailable: unavailable,
            ..Default::default()
        };
        if runnable.is_empty() {
            report.duration_ms = started.elapsed().as_millis() as u64;
            return Ok(report);
        }

        // ---- 1) Candidate set: cached by (kind, name_contains), rules share the same query ----
        let root = self
            .store
            .get_project(project_id)?
            .map(|p| std::path::PathBuf::from(p.root_path));
        let mut facts = Facts::new(root);
        let mut cache: HashMap<(String, Option<String>), Vec<Node>> = HashMap::new();
        for (rule, params) in &runnable {
            let limit = resolve_num(&rule.applies_to.limit, params);
            let name_contains = resolve_str_opt(&rule.applies_to.name_contains, params);
            for kind in scope_kinds(rule) {
                let key = (kind.clone(), name_contains.clone());
                if cache.contains_key(&key) {
                    continue;
                }
                let nodes = self.store.query_nodes(&NodeFilter {
                    project_id,
                    kind: if kind.is_empty() {
                        None
                    } else {
                        Some(NodeKind::new(kind.clone()))
                    },
                    name_contains: name_contains.clone(),
                    limit: Some(limit as u32),
                    offset: None,
                })?;
                cache.insert(key, nodes);
            }
        }

        // ---- 2) Preload: annotations / in-edges / out-edges / file paths ----
        let mut ids: Vec<NodeId> = Vec::new();
        for nodes in cache.values() {
            for n in nodes {
                ids.push(n.id);
            }
        }
        facts.annotations = self.store.annotations_of_project(project_id)?;
        facts.files = self.store.file_paths(project_id)?;
        facts.incoming = self.store.edges_incoming(&ids)?;
        facts.outgoing = self.store.edges_outgoing(&ids)?;

        let mut violations: Vec<Violation> = Vec::new();
        let mut silent: Vec<String> = Vec::new();
        for (rule, params) in &runnable {
            let name_contains = resolve_str_opt(&rule.applies_to.name_contains, params);
            let mut hit = 0usize;
            let mut candidates = 0usize;
            for kind in scope_kinds(rule) {
                let key = (kind.clone(), name_contains.clone());
                let Some(nodes) = cache.get(&key) else { continue };
                candidates += nodes.len();
                for node in nodes {
                    if !matches_all(&rule.when, node, &facts, params) {
                        continue;
                    }
                    hit += 1;
                    violations.push(build_violation(project_id, rule, node, &facts, params));
                }
            }
            if hit == 0 {
                silent.push(if candidates == 0 {
                    format!("{} (candidate set empty: no such nodes on the graph)", rule.id)
                } else {
                    format!("{} (none of {} candidates hit)", rule.id, candidates)
                });
            }
        }

        if persist {
            match only {
                None => {
                    self.store.clear_diagnostics(project_id, RULE_CODE_PREFIX)?;
                }
                Some(_) => {
                    // `runnable` is now a (rule, params) pair, a different type from `unavailable_rules`'s `&CheckRule`;
                    // unify them into rule references first, then chain the iteration.
                    let runnable_rules: Vec<&CheckRule> =
                        runnable.iter().map(|(r, _)| *r).collect();
                    for rule in runnable_rules.iter().chain(unavailable_rules.iter()) {
                        self.store.clear_diagnostics(project_id, &rule.code())?;
                    }
                }
            }
            let diags: Vec<Diagnostic> = violations.iter().map(|v| v.to_diagnostic()).collect();
            if !diags.is_empty() {
                // Batch write: a single SQLite statement has a limit on the number of bound variables.
                for chunk in diags.chunks(500) {
                    self.store.push_diagnostics(chunk)?;
                }
            }
        }

        let mut by_severity: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut by_rule: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        for v in &violations {
            *by_severity.entry(severity_key(v.severity).to_string()).or_default() += 1;
            *by_rule.entry(v.rule_id.clone()).or_default() += 1;
        }
        report.by_severity = by_severity;
        report.by_rule = by_rule;
        report.violations = violations;
        report.rules_silent = silent;
        report.sort_violations();
        report.duration_ms = started.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// Read the already-persisted violations (without re-running rules).
    pub fn violations(
        &self,
        project_id: ProjectId,
        limit: u32,
        sub_project_id: Option<&[SubProjectId]>,
    ) -> Result<Vec<Violation>> {
        let diags = self
            .store
            .list_diagnostics_by_code(project_id, RULE_CODE_PREFIX, sub_project_id, limit)?;
        Ok(diags.iter().filter_map(Violation::from_diagnostic).collect())
    }

    /// A lightweight rollup of persisted violations (counted by severity), for things like menu badges.
    ///
    /// Does not re-run rules or pull all violations, only takes grouped counts; the build auto-runs the check and persists,
    /// so what is seen here is the conclusion of the last automatic (or manual) check.
    pub fn summary(
        &self,
        project_id: ProjectId,
        sub_project_id: Option<&[SubProjectId]>,
    ) -> Result<CheckSummary> {
        let counts = self
            .store
            .count_diagnostics_by_code(project_id, RULE_CODE_PREFIX, sub_project_id)?;
        let mut s = CheckSummary::default();
        for (sev, n) in counts {
            match sev.as_str() {
                "critical" => s.critical = n,
                "error" => s.error = n,
                "warning" => s.warning = n,
                "info" => s.info = n,
                _ => {}
            }
        }
        Ok(s)
    }

    /// Take all of a project's rule-config overrides.
    pub fn rule_configs(
        &self,
        project_id: ProjectId,
    ) -> Result<std::collections::HashMap<String, ProjectRuleConfig>> {
        self.store.get_rule_configs(project_id)
    }

    /// Write a single rule's config (whole-row override).
    pub fn set_rule_config(&self, cfg: ProjectRuleConfig) -> Result<()> {
        self.store.set_rule_config(&cfg)
    }

    /// Apply a **patch**: omitted fields inherit the existing override.
    ///
    /// A whole-row override would hit a real pitfall: on the UI, "enable/disable" and "tune parameters" are two independent operations,
    /// and if every write is whole-row, tuning the threshold first then clicking the switch silently clears the params.
    pub fn apply_rule_config(&self, project_id: ProjectId, patch: RuleConfigPatch) -> Result<()> {
        let existing = self
            .store
            .get_rule_configs(project_id)?
            .get(&patch.rule_id)
            .cloned();
        let mut enabled = patch.enabled;
        let mut options = patch.options.unwrap_or(Value::Object(Default::default()));
        if let Some(old) = existing {
            if enabled.is_none() {
                enabled = old.enabled;
            }
            // When the patch carries no options, keep the old value; when it does, merge by key (only overwrite the keys present).
            if let (Some(old_obj), Some(new_obj)) = (old.options.as_object(), options.as_object()) {
                if new_obj.is_empty() {
                    options = old.options;
                } else {
                    let mut merged = old_obj.clone();
                    for (k, v) in new_obj {
                        merged.insert(k.clone(), v.clone());
                    }
                    options = Value::Object(merged);
                }
            }
        }
        self.store.set_rule_config(&ProjectRuleConfig {
            project_id,
            rule_id: patch.rule_id,
            enabled,
            options,
        })
    }

    /// Reset a rule's project override (back to the YAML global default + default params).
    pub fn reset_rule_config(&self, project_id: ProjectId, rule_id: &str) -> Result<()> {
        self.store.delete_rule_config(project_id, rule_id)
    }

    /// Apply config patches in bulk (when enabling/disabling a whole group or category).
    pub fn batch_rule_config(
        &self,
        project_id: ProjectId,
        items: Vec<RuleConfigPatch>,
    ) -> Result<()> {
        for it in items {
            self.apply_rule_config(project_id, it)?;
        }
        Ok(())
    }
}

/// A severity rollup of compliance-check violations (for menu badges).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckSummary {
    pub critical: u64,
    pub error: u64,
    pub warning: u64,
    pub info: u64,
}

/// A project's tech-stack environment + the kinds of graph facts actually present on the graph.
///
/// Both are **applicability criteria** for a rule, but come from different sources:
/// * `languages` / `frameworks` come from sub-project recognition (what stack this project "is");
/// * `edge_kinds` / `annotation_kinds` come from the already-built graph (what facts this project "produced").
struct ProjectEnv {
    languages: Vec<String>,
    frameworks: Vec<String>,
    edge_kinds: Vec<String>,
    /// `(channel, kind)`。
    annotation_kinds: Vec<(String, String)>,
}

impl ProjectEnv {
    fn load(store: &dyn Persistence, project_id: ProjectId) -> Result<Self> {
        let mut languages: Vec<String> = Vec::new();
        let mut frameworks: Vec<String> = Vec::new();
        for sub in store.list_sub_projects(project_id)? {
            let l = sub.language.to_string();
            if !languages.iter().any(|x| x.eq_ignore_ascii_case(&l)) {
                languages.push(l);
            }
            for f in sub.frameworks {
                if !frameworks.iter().any(|x| x.eq_ignore_ascii_case(&f)) {
                    frameworks.push(f);
                }
            }
        }
        Ok(Self {
            languages,
            frameworks,
            edge_kinds: store.edge_kinds(project_id)?,
            annotation_kinds: store.annotation_kinds(project_id)?,
        })
    }

    fn describe(&self) -> String {
        let l = if self.languages.is_empty() {
            "unknown".to_string()
        } else {
            self.languages.join("/")
        };
        if self.frameworks.is_empty() {
            l
        } else {
            format!("{l}（{}）", self.frameworks.join("/"))
        }
    }

    fn has_edge(&self, kind: &str) -> bool {
        self.edge_kinds.iter().any(|k| k.eq_ignore_ascii_case(kind))
    }

    fn has_annotation(&self, kind: &str) -> bool {
        self.annotation_kinds.iter().any(|(_, k)| k.eq_ignore_ascii_case(kind))
    }

    /// Whether the criterion holds; returning `Some(reason)` means this rule **cannot run** on this project.
    ///
    /// Only blocks the case "not a single one exists on the graph": as long as even one same-named fact exists,
    /// the production chain is confirmed working, and the rest (e.g. only 3 were produced) is a coverage issue,
    /// which should be surfaced by a "silent zero" warning rather than stopping the rule outright.
    fn missing_requirement(&self, req: &RuleRequirements) -> Option<String> {
        for e in &req.edges {
            if !self.has_edge(e) {
                return Some(format!("no {e} edges on the graph, the `no/has_{e}` criterion does not hold"));
            }
        }
        for a in &req.annotations {
            if !self.has_annotation(a) {
                return Some(format!("no {a} annotations on the graph, the `no/has_annotation` criterion does not hold"));
            }
        }
        if !req.capabilities.is_empty() {
            let any_cap = self
                .annotation_kinds
                .iter()
                .any(|(ch, _)| ch == AnnotationChannel::CAPABILITY);
            if !any_cap {
                return Some(format!(
                    "no annotation on the {} channel exists on the graph, the capability criterion ({}) does not hold",
                    AnnotationChannel::CAPABILITY,
                    req.capabilities.join("/")
                ));
            }
        }
        None
    }
}

fn severity_key(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

/// The node kind a rule acts on; empty falls back to "any kind" (uses the empty string as the cache key).
fn scope_kinds(rule: &CheckRule) -> Vec<String> {
    if rule.applies_to.kinds.is_empty() {
        vec![String::new()]
    } else {
        rule.applies_to.kinds.clone()
    }
}

fn build_violation(
    project_id: ProjectId,
    rule: &CheckRule,
    node: &Node,
    facts: &Facts,
    params: &ParamValues,
) -> Violation {
    let (file, line) = crate::location::node_location(node, &facts.files, facts.root.as_deref());
    let identity = node.identity.as_ref().map(|i| i.value.clone());
    let message = rule.render_with(
        &node.name,
        node.kind.as_str(),
        node.fqn.as_deref(),
        identity.as_deref(),
        file.as_deref(),
        line,
        Some(params),
    );
    Violation {
        project_id,
        rule_id: rule.id.clone(),
        title: rule.title.clone(),
        category: rule.category.clone(),
        severity: rule.severity,
        node_id: node.id,
        node_name: node.name.clone(),
        node_kind: node.kind.to_string(),
        sub_project_id: node.sub_project_id,
        message,
        remediation: rule.remediation.clone(),
        file,
        line,
    }
}

// ---------------------------------------------------------------- Evaluation context

/// Graph facts preloaded for one check.
///
/// Read-only, immutable — every rule's evaluation runs on this snapshot, guaranteeing the same input yields the same output.
#[derive(Default)]
struct Facts {
    annotations: HashMap<i64, Vec<Annotation>>,
    incoming: HashMap<i64, Vec<Edge>>,
    outgoing: HashMap<i64, Vec<Edge>>,
    files: HashMap<i64, String>,
    /// The project root path (synthesised nodes' `locations` are relative paths, needing it to assemble an absolute path).
    root: Option<std::path::PathBuf>,
}

impl Facts {
    fn new(root: Option<std::path::PathBuf>) -> Self {
        Self { root, ..Default::default() }
    }

    /// Whether the node has an annotation of a given kind (case-insensitive).
    fn has_annotation(&self, node: NodeId, kind: &str) -> bool {
        self.annotations
            .get(&node.get())
            .map(|list| list.iter().any(|a| a.kind.eq_ignore_ascii_case(kind)))
            .unwrap_or(false)
    }

    /// Whether the node (and its scope chain) has a given capability.
    ///
    /// Capabilities are annotated on the `Capability` channel (`Authentication` / `RateLimiting`).
    fn has_capability(&self, node: NodeId, cap: &str) -> bool {
        self.annotations
            .get(&node.get())
            .map(|list| {
                list.iter()
                    .any(|a| a.channel.as_str() == AnnotationChannel::CAPABILITY
                        && a.kind.eq_ignore_ascii_case(cap))
            })
            .unwrap_or(false)
    }

    /// Fan-in: the number of semantic in-edges (same definition as the view layer, so "N in-edges" matches the canvas).
    fn fan_in(&self, node: NodeId) -> u64 {
        self.incoming
            .get(&node.get())
            .map(|es| es.iter().filter(|e| is_semantic_edge(e.kind.as_str())).count() as u64)
            .unwrap_or(0)
    }

    fn fan_out(&self, node: NodeId) -> u64 {
        self.outgoing
            .get(&node.get())
            .map(|es| es.iter().filter(|e| is_semantic_edge(e.kind.as_str())).count() as u64)
            .unwrap_or(0)
    }

    fn has_incoming_kind(&self, node: NodeId, kind: &str) -> bool {
        self.incoming
            .get(&node.get())
            .map(|es| es.iter().any(|e| e.kind.as_str().eq_ignore_ascii_case(kind)))
            .unwrap_or(false)
    }

    fn has_outgoing_kind(&self, node: NodeId, kind: &str) -> bool {
        self.outgoing
            .get(&node.get())
            .map(|es| es.iter().any(|e| e.kind.as_str().eq_ignore_ascii_case(kind)))
            .unwrap_or(false)
    }
}

/// A node's "retrievable text": name / fqn / identity.
fn node_text(node: &Node) -> String {
    let mut s = node.name.clone();
    if let Some(fqn) = &node.fqn {
        s.push('\n');
        s.push_str(fqn);
    }
    if let Some(id) = &node.identity {
        s.push('\n');
        s.push_str(&id.value);
    }
    s.to_lowercase()
}

fn property_value(node: &Node, name: &str) -> Option<String> {
    let v = node.properties.get(name)?;
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// All predicates must hold to be a hit (`when` empty = all in scope are hits).
fn matches_all(when: &[CheckPredicate], node: &Node, facts: &Facts, params: &ParamValues) -> bool {
    when.iter().all(|p| eval(p, node, facts, params))
}

fn eval(p: &CheckPredicate, node: &Node, facts: &Facts, params: &ParamValues) -> bool {
    match p {
        CheckPredicate::KindIn(kinds) => kinds
            .iter()
            .any(|k| node.kind.as_str().eq_ignore_ascii_case(k)),
        CheckPredicate::NameContains(sub) => {
            node.name.to_lowercase().contains(&resolve_str(sub, params).to_lowercase())
        }
        CheckPredicate::NameStartsWith(prefix) => node
            .name
            .to_lowercase()
            .starts_with(&resolve_str(prefix, params).to_lowercase()),
        CheckPredicate::FqnContains(sub) => node
            .fqn
            .as_deref()
            .map(|f| f.to_lowercase().contains(&resolve_str(sub, params).to_lowercase()))
            .unwrap_or(false),
        CheckPredicate::IdentityContains(sub) => node
            .identity
            .as_ref()
            .map(|i| i.value.to_lowercase().contains(&resolve_str(sub, params).to_lowercase()))
            .unwrap_or(false),
        CheckPredicate::TextContains(sub) => {
            node_text(node).contains(&resolve_str(sub, params).to_lowercase())
        }
        CheckPredicate::HasAnnotation(kind) => facts.has_annotation(node.id, kind),
        CheckPredicate::NoAnnotation(kind) => !facts.has_annotation(node.id, kind),
        CheckPredicate::PropertyIs { name, value } => property_value(node, name)
            .map(|v| v.eq_ignore_ascii_case(value))
            .unwrap_or(false),
        CheckPredicate::PropertyContains { name, substring } => property_value(node, name)
            .map(|v| v.to_lowercase().contains(&substring.to_lowercase()))
            .unwrap_or(false),
        CheckPredicate::PropertyMissing(name) => property_value(node, name).is_none(),
        CheckPredicate::NoCapability(caps) => {
            !caps.iter().any(|c| facts.has_capability(node.id, c))
        }
        CheckPredicate::FanInGte(n) => facts.fan_in(node.id) >= resolve_num(n, params),
        CheckPredicate::FanInLte(n) => facts.fan_in(node.id) <= resolve_num(n, params),
        CheckPredicate::FanOutGte(n) => facts.fan_out(node.id) >= resolve_num(n, params),
        CheckPredicate::NoIncoming(kind) => !facts.has_incoming_kind(node.id, kind),
        CheckPredicate::HasIncoming(kind) => facts.has_incoming_kind(node.id, kind),
        CheckPredicate::NoOutgoing(kind) => !facts.has_outgoing_kind(node.id, kind),
        CheckPredicate::HasOutgoing(kind) => facts.has_outgoing_kind(node.id, kind),
        CheckPredicate::AllOf(list) => list.iter().all(|p| eval(p, node, facts, params)),
        CheckPredicate::AnyOf(list) => list.iter().any(|p| eval(p, node, facts, params)),
        CheckPredicate::Not(inner) => !eval(inner, node, facts, params),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::{
        AnnotationChannel, EdgeId, EdgeKind, GraphDelta, IdentityKey, Language, NewEdge, NewNode,
        NewProject, NewSubProject, NumOrParam, Phase, RuleRequirements, RuleScope, Severity, Span,
        StrOrParam,
    };
    use gt_domain::port::RuleProvider;
    use gt_adapter_sqlite::SqliteStore;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use serde_json::json;

    // ---- builders ----

    fn node(id: i64, kind: &str, name: &str) -> Node {
        Node {
            id: NodeId::new(id),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            kind: NodeKind::new(kind),
            name: name.to_string(),
            fqn: None,
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    fn edge(kind: &str, from: i64, to: i64) -> Edge {
        Edge {
            id: EdgeId::new(0),
            project_id: ProjectId::new(1),
            kind: EdgeKind::new(kind),
            from_id: NodeId::new(from),
            to_id: NodeId::new(to),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    fn ann(channel: &str, kind: &str) -> Annotation {
        Annotation {
            id: 0,
            node_id: NodeId::new(0),
            channel: AnnotationChannel::from(channel),
            kind: kind.to_string(),
            subkind: None,
            confidence: 1.0,
            evidence: Value::Null,
            phase: Phase::new("Synthesize"),
        }
    }

    fn facts(
        annotations: HashMap<i64, Vec<Annotation>>,
        incoming: HashMap<i64, Vec<Edge>>,
        outgoing: HashMap<i64, Vec<Edge>>,
        files: HashMap<i64, String>,
    ) -> Facts {
        Facts {
            annotations,
            incoming,
            outgoing,
            files,
            root: None,
        }
    }

    fn empty_params() -> ParamValues {
        ParamValues::new()
    }

    // ---- eval: node-shape predicates ----

    #[test]
    fn eval_kind_in_matches_set() {
        let n = node(1, "Class", "User");
        assert!(eval(&CheckPredicate::KindIn(vec!["Class".into()]), &n, &Facts::default(), &empty_params()));
        assert!(!eval(&CheckPredicate::KindIn(vec!["Method".into()]), &n, &Facts::default(), &empty_params()));
    }

    #[test]
    fn eval_name_contains_is_case_insensitive() {
        let n = node(1, "Method", "getUserOrders");
        assert!(eval(&CheckPredicate::NameContains(StrOrParam::Str("GETUSERORDERS".into())), &n, &Facts::default(), &empty_params()));
        assert!(!eval(&CheckPredicate::NameContains(StrOrParam::Str("zzz".into())), &n, &Facts::default(), &empty_params()));
    }

    #[test]
    fn eval_name_starts_with() {
        let n = node(1, "Method", "getUserOrders");
        assert!(eval(&CheckPredicate::NameStartsWith(StrOrParam::Str("getUser".into())), &n, &Facts::default(), &empty_params()));
        assert!(!eval(&CheckPredicate::NameStartsWith(StrOrParam::Str("Orders".into())), &n, &Facts::default(), &empty_params()));
    }

    #[test]
    fn eval_fqn_contains_and_missing() {
        let mut n = node(1, "Class", "User");
        n.fqn = Some("App\\Service\\UserSvc".into());
        assert!(eval(&CheckPredicate::FqnContains(StrOrParam::Str("usersvc".into())), &n, &Facts::default(), &empty_params()));
        let no_fqn = node(2, "Class", "User");
        assert!(!eval(&CheckPredicate::FqnContains(StrOrParam::Str("usersvc".into())), &no_fqn, &Facts::default(), &empty_params()));
    }

    #[test]
    fn eval_identity_contains() {
        let mut n = node(1, "HttpContract", "order");
        n.identity = Some(IdentityKey::fqn("POST /api/order".to_string()));
        assert!(eval(&CheckPredicate::IdentityContains(StrOrParam::Str("post /api".into())), &n, &Facts::default(), &empty_params()));
        assert!(!eval(&CheckPredicate::IdentityContains(StrOrParam::Str("DELETE".into())), &n, &Facts::default(), &empty_params()));
    }

    #[test]
    fn eval_text_contains_spans_name_fqn_identity() {
        let mut n = node(1, "Class", "OrderService");
        n.fqn = Some("App\\Order".into());
        n.identity = Some(IdentityKey::fqn("svc.order".to_string()));
        assert!(eval(&CheckPredicate::TextContains(StrOrParam::Str("orderservice".into())), &n, &Facts::default(), &empty_params()));
        assert!(eval(&CheckPredicate::TextContains(StrOrParam::Str("app\\order".into())), &n, &Facts::default(), &empty_params()));
        assert!(eval(&CheckPredicate::TextContains(StrOrParam::Str("svc.order".into())), &n, &Facts::default(), &empty_params()));
    }

    #[test]
    fn eval_has_and_no_annotation() {
        let n = node(1, "Class", "User");
        let mut a: HashMap<i64, Vec<Annotation>> = HashMap::new();
        a.insert(n.id.get(), vec![ann(AnnotationChannel::FKB_MARK, "pii")]);
        let f = facts(a, HashMap::new(), HashMap::new(), HashMap::new());
        assert!(eval(&CheckPredicate::HasAnnotation("pii".into()), &n, &f, &empty_params()));
        assert!(!eval(&CheckPredicate::HasAnnotation("auth".into()), &n, &f, &empty_params()));
        assert!(eval(&CheckPredicate::NoAnnotation("auth".into()), &n, &f, &empty_params()));
        assert!(!eval(&CheckPredicate::NoAnnotation("pii".into()), &n, &f, &empty_params()));
    }

    #[test]
    fn eval_property_predicates() {
        let mut n = node(1, "Class", "User");
        n.properties = json!({ "role": "admin", "side": "Frontend" });
        let f = Facts::default();
        assert!(eval(&CheckPredicate::PropertyIs { name: "role".into(), value: "admin".into() }, &n, &f, &empty_params()));
        assert!(!eval(&CheckPredicate::PropertyIs { name: "role".into(), value: "user".into() }, &n, &f, &empty_params()));
        assert!(eval(&CheckPredicate::PropertyContains { name: "side".into(), substring: "FRONT".into() }, &n, &f, &empty_params()));
        assert!(eval(&CheckPredicate::PropertyMissing("missing".into()), &n, &f, &empty_params()));
        assert!(!eval(&CheckPredicate::PropertyMissing("role".into()), &n, &f, &empty_params()));
    }

    #[test]
    fn eval_no_capability_negates_present_capability() {
        let n = node(1, "Class", "Order");
        let mut a: HashMap<i64, Vec<Annotation>> = HashMap::new();
        a.insert(n.id.get(), vec![ann(AnnotationChannel::CAPABILITY, "Authentication")]);
        let f = facts(a, HashMap::new(), HashMap::new(), HashMap::new());
        assert!(!eval(&CheckPredicate::NoCapability(vec!["Authentication".into()]), &n, &f, &empty_params()), "有该能力时 NoCapability 应为 false");
        assert!(eval(&CheckPredicate::NoCapability(vec!["RateLimiting".into()]), &n, &f, &empty_params()), "缺该能力时 NoCapability 应为 true");
        assert!(eval(&CheckPredicate::NoCapability(vec!["Authentication".into()]), &node(2, "Class", "Guest"), &Facts::default(), &empty_params()));
    }

    // ---- eval: edge / fan-in-out (semantic filter) ----

    #[test]
    fn eval_fan_in_counts_only_semantic_edges() {
        let n = node(1, "Method", "m");
        let mut inc: HashMap<i64, Vec<Edge>> = HashMap::new();
        inc.insert(n.id.get(), vec![edge("ReadsDb", 99, n.id.get()), edge("Calls", 98, n.id.get())]);
        let f = facts(HashMap::new(), inc, HashMap::new(), HashMap::new());
        assert!(eval(&CheckPredicate::FanInGte(NumOrParam::Num(1)), &n, &f, &empty_params()), "一条语义入边 → fan_in=1");
        assert!(!eval(&CheckPredicate::FanInGte(NumOrParam::Num(2)), &n, &f, &empty_params()), "非语义入边不计入 → fan_in 仍为 1");
        assert!(eval(&CheckPredicate::HasIncoming("ReadsDb".into()), &n, &f, &empty_params()));
        assert!(eval(&CheckPredicate::NoIncoming("HandledBy".into()), &n, &f, &empty_params()));
    }

    #[test]
    fn eval_fan_out_and_outgoing() {
        let n = node(1, "Method", "m");
        let mut out: HashMap<i64, Vec<Edge>> = HashMap::new();
        out.insert(n.id.get(), vec![edge("WritesDb", n.id.get(), 7), edge("Calls", n.id.get(), 8)]);
        let f = facts(HashMap::new(), HashMap::new(), out, HashMap::new());
        assert!(eval(&CheckPredicate::FanOutGte(NumOrParam::Num(1)), &n, &f, &empty_params()));
        assert!(!eval(&CheckPredicate::FanOutGte(NumOrParam::Num(2)), &n, &f, &empty_params()), "非语义出边不计入 fan_out");
        assert!(eval(&CheckPredicate::HasOutgoing("WritesDb".into()), &n, &f, &empty_params()));
        assert!(eval(&CheckPredicate::NoOutgoing("Triggers".into()), &n, &f, &empty_params()));
    }

    #[test]
    fn eval_combinators_all_any_not() {
        let n = node(1, "Class", "UserService");
        let all = CheckPredicate::AllOf(vec![
            CheckPredicate::KindIn(vec!["Class".into()]),
            CheckPredicate::NameContains(StrOrParam::Str("service".into())),
        ]);
        assert!(eval(&all, &n, &Facts::default(), &empty_params()));
        let any = CheckPredicate::AnyOf(vec![
            CheckPredicate::KindIn(vec!["Method".into()]),
            CheckPredicate::NameContains(StrOrParam::Str("service".into())),
        ]);
        assert!(eval(&any, &n, &Facts::default(), &empty_params()));
        assert!(eval(&CheckPredicate::Not(Box::new(CheckPredicate::KindIn(vec!["Method".into()]))), &n, &Facts::default(), &empty_params()));
        assert!(!eval(&CheckPredicate::Not(Box::new(CheckPredicate::KindIn(vec!["Class".into()]))), &n, &Facts::default(), &empty_params()));
    }

    #[test]
    fn matches_all_empty_when_is_always_true() {
        assert!(matches_all(&[], &node(1, "Class", "X"), &Facts::default(), &empty_params()), "空 when = 范围内全部命中");
    }

    #[test]
    fn matches_all_requires_every_predicate() {
        let n = node(1, "Class", "UserService");
        let when = vec![
            CheckPredicate::KindIn(vec!["Class".into()]),
            CheckPredicate::NameContains(StrOrParam::Str("nope".into())),
        ];
        assert!(!matches_all(&when, &n, &Facts::default(), &empty_params()));
    }

    // ---- ProjectEnv::missing_requirement ----

    #[test]
    fn missing_requirement_blocks_when_graph_lacks_fact() {
        let env = ProjectEnv {
            languages: vec!["php".into()],
            frameworks: vec!["thinkphp".into()],
            edge_kinds: vec!["ReadsDb".into()],
            annotation_kinds: vec![
                ("FkbMark".into(), "pii".into()),
                ("Capability".into(), "Authentication".into()),
            ],
        };
        assert!(env.missing_requirement(&RuleRequirements { edges: vec!["ReadsDb".into()], ..Default::default() }).is_none());
        assert!(env.missing_requirement(&RuleRequirements { annotations: vec!["pii".into()], ..Default::default() }).is_none());
        // A capability on the channel exists → the requirement is satisfied (it checks the *channel*, not the
        // specific capability name — so any Capability-channel annotation clears it).
        assert!(env.missing_requirement(&RuleRequirements { capabilities: vec!["Authentication".into()], ..Default::default() }).is_none());
        // A fact absent from the graph blocks the rule.
        assert!(env.missing_requirement(&RuleRequirements { edges: vec!["Triggers".into()], ..Default::default() }).is_some());
        assert!(env.missing_requirement(&RuleRequirements { annotations: vec!["auth".into()], ..Default::default() }).is_some());

        // An env with NO Capability-channel annotation at all: any capability requirement blocks.
        let env_no_cap = ProjectEnv {
            languages: vec!["php".into()],
            frameworks: vec!["thinkphp".into()],
            edge_kinds: vec!["ReadsDb".into()],
            annotation_kinds: vec![("FkbMark".into(), "pii".into())],
        };
        assert!(env_no_cap.missing_requirement(&RuleRequirements { capabilities: vec!["Authentication".into()], ..Default::default() }).is_some());
    }

    // ---- small pure helpers ----

    #[test]
    fn severity_key_maps_lowercase() {
        assert_eq!(severity_key(Severity::Critical), "critical");
        assert_eq!(severity_key(Severity::Error), "error");
        assert_eq!(severity_key(Severity::Warning), "warning");
        assert_eq!(severity_key(Severity::Info), "info");
    }

    #[test]
    fn node_text_joins_name_fqn_identity_lowercased() {
        let mut n = node(1, "Class", "OrderService");
        n.fqn = Some("App\\Order".into());
        n.identity = Some(IdentityKey::fqn("svc.order".to_string()));
        let t = node_text(&n);
        assert!(t.contains("orderservice"));
        assert!(t.contains("app\\order"));
        assert!(t.contains("svc.order"));
    }

    #[test]
    fn property_value_reads_string_and_other() {
        let mut n = node(1, "Class", "X");
        n.properties = json!({ "role": "admin", "count": 3 });
        assert_eq!(property_value(&n, "role"), Some("admin".into()));
        assert_eq!(property_value(&n, "count"), Some("3".into()));
        assert_eq!(property_value(&n, "missing"), None);
    }

    // ---- apply_rule_config: the documented whole-row-override pitfall ----

    struct StubProvider;
    impl RuleProvider for StubProvider {
        fn rules(&self) -> &[CheckRule] {
            &[]
        }
    }

    #[test]
    fn apply_rule_config_patch_keeps_old_options_when_omitted() {
        let store = Arc::new(SqliteStore::in_memory().expect("in-memory store"));
        let svc = RuleService::new(store, Arc::new(StubProvider));
        let pid = ProjectId::new(1);
        svc.set_rule_config(ProjectRuleConfig {
            project_id: pid,
            rule_id: "r1".into(),
            enabled: Some(true),
            options: json!({ "threshold": 5, "ignore": "x" }),
        })
        .unwrap();
        // patch only flips `enabled`; must NOT clear the existing options (the whole-row-override pitfall).
        svc.apply_rule_config(pid, RuleConfigPatch { rule_id: "r1".into(), enabled: Some(false), options: None })
            .unwrap();
        let cfg = svc.rule_configs(pid).unwrap().get("r1").cloned().expect("config exists");
        assert_eq!(cfg.enabled, Some(false));
        let opts = cfg.options.as_object().expect("options preserved");
        assert_eq!(opts.get("threshold").and_then(|v| v.as_i64()), Some(5));
        assert_eq!(opts.get("ignore").and_then(|v| v.as_str()), Some("x"));
    }

    #[test]
    fn apply_rule_config_patch_merges_options_by_key() {
        let store = Arc::new(SqliteStore::in_memory().expect("in-memory store"));
        let svc = RuleService::new(store, Arc::new(StubProvider));
        let pid = ProjectId::new(1);
        svc.set_rule_config(ProjectRuleConfig {
            project_id: pid,
            rule_id: "r1".into(),
            enabled: Some(true),
            options: json!({ "threshold": 5, "ignore": "x" }),
        })
        .unwrap();
        // patch carries only `threshold`; `ignore` must survive and `enabled` is left untouched.
        svc.apply_rule_config(pid, RuleConfigPatch { rule_id: "r1".into(), enabled: None, options: Some(json!({ "threshold": 9 })) })
            .unwrap();
        let cfg = svc.rule_configs(pid).unwrap().get("r1").cloned().expect("config exists");
        assert_eq!(cfg.enabled, Some(true), "未传 enabled 应保留原值");
        let opts = cfg.options.as_object().unwrap();
        assert_eq!(opts.get("threshold").and_then(|v| v.as_i64()), Some(9), "传入的 threshold 应覆盖");
        assert_eq!(opts.get("ignore").and_then(|v| v.as_str()), Some("x"), "未传的 ignore 应保留");
    }

    // A `RuleProvider` that serves an explicit in-test rule set (the existing `StubProvider` serves none).
    struct TestProvider {
        rules: Vec<CheckRule>,
    }
    impl RuleProvider for TestProvider {
        fn rules(&self) -> &[CheckRule] {
            &self.rules
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gt_rule_it_{}_{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Build a `CheckRule` from the pieces the orchestration tests care about.
    fn rule(id: &str, kinds: &[&str], languages: &[&str], when: Vec<CheckPredicate>) -> CheckRule {
        CheckRule {
            id: id.to_string(),
            title: id.to_string(),
            description: None,
            severity: Severity::Error,
            category: "contract".to_string(),
            enabled: true,
            applies_to: RuleScope {
                kinds: kinds.iter().map(|s| s.to_string()).collect(),
                name_contains: None,
                limit: NumOrParam::Num(20_000),
                languages: languages.iter().map(|s| s.to_string()).collect(),
                frameworks: vec![],
            },
            params: vec![],
            when,
            message: "{name} is a {kind} with no handler".to_string(),
            remediation: None,
        }
    }

    fn new_node(
        pid: ProjectId,
        id: i64,
        kind: &str,
        name: &str,
        identity: Option<IdentityKey>,
    ) -> NewNode {
        NewNode {
            id: Some(NodeId::new(id)),
            project_id: pid,
            sub_project_id: None,
            kind: NodeKind::new(kind),
            name: name.to_string(),
            fqn: None,
            identity,
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    /// Seed a project whose graph has two `HttpContract`s — node 2 is `HandledBy` a Method, node 1 is not — plus a php
    /// sub-project so env / framework gates open.
    fn seed_store() -> (Arc<dyn Persistence>, ProjectId) {
        let store: Arc<dyn Persistence> =
            Arc::new(SqliteStore::in_memory().expect("in-memory store"));
        let p = store
            .create_project(NewProject {
                name: "p".into(),
                root_path: tmpdir("seed"),
                description: None,
                config: None,
            })
            .unwrap();
        store
            .replace_sub_projects(p.id, vec![NewSubProject {
                project_id: p.id,
                name: "be".into(),
                root_path: p.root_path.clone(),
                language: Language::new("php"),
                role: "backend".into(),
                detected_by: "composer.json".into(),
                frameworks: vec!["thinkphp".into()],
                facts: Value::Null,
            }])
            .unwrap();
        let pid = p.id;
        let mut d = GraphDelta::new(pid);
        d.nodes.push(new_node(
            pid,
            1,
            "HttpContract",
            "POST /api/order",
            Some(IdentityKey::contract("POST", "/api/order")),
        ));
        d.nodes.push(new_node(
            pid,
            2,
            "HttpContract",
            "GET /api/user",
            Some(IdentityKey::contract("GET", "/api/user")),
        ));
        d.nodes.push(new_node(pid, 3, "Method", "handleOrder", None));
        // node 2 is handled; node 1 is not -> "no incoming HandledBy" should flag only node 1
        d.edges.push(NewEdge::new(
            pid,
            EdgeKind::new("HandledBy"),
            NodeId::new(3),
            NodeId::new(2),
        ));
        store.apply(&d).unwrap();
        (store, pid)
    }

    /// The end-to-end path: `check(persist=true)` runs the rule, writes `rule:*` diagnostics, and `violations` /
    /// `summary` read them back. This also exercises `ProjectEnv::load` (languages / edge kinds) and `build_violation`.
    #[test]
    fn check_finds_contract_without_handler_and_persists() {
        let (store, pid) = seed_store();
        let svc = RuleService::new(
            store,
            Arc::new(TestProvider {
                rules: vec![rule(
                    "contract-no-handler",
                    &["HttpContract"],
                    &["php"],
                    vec![CheckPredicate::NoIncoming("HandledBy".into())],
                )],
            }),
        );
        let rep = svc.check(pid, None, true).unwrap();
        assert_eq!(rep.rules_total, 1);
        assert_eq!(rep.rules_run, 1, "规则应被选中并执行");
        assert_eq!(rep.violations.len(), 1, "只有未处理的契约应被标红");
        assert_eq!(rep.violations[0].node_id, NodeId::new(1));
        assert_eq!(rep.by_rule.get("contract-no-handler").copied(), Some(1));
        assert_eq!(rep.by_severity.get("error").copied(), Some(1));

        // persisted violations read back through the read-only accessor
        let vs = svc.violations(pid, 100, None).unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].rule_id, "contract-no-handler");

        // and rolled up by severity for the menu badge
        let s = svc.summary(pid, None).unwrap();
        assert_eq!(s.error, 1);
    }

    /// `only` selects by id (ignoring the enabled switch) and a bogus id runs nothing.
    #[test]
    fn check_only_runs_named_rules() {
        let (store, pid) = seed_store();
        let svc = RuleService::new(
            store,
            Arc::new(TestProvider {
                rules: vec![
                    rule(
                        "contract-no-handler",
                        &["HttpContract"],
                        &["php"],
                        vec![CheckPredicate::NoIncoming("HandledBy".into())],
                    ),
                    rule("always-on", &["Method"], &["php"], vec![]),
                ],
            }),
        );
        let rep = svc
            .check(pid, Some(&["contract-no-handler".to_string()]), false)
            .unwrap();
        assert_eq!(rep.rules_run, 1);
        assert_eq!(rep.violations.len(), 1);

        let rep2 = svc
            .check(pid, Some(&["nope".to_string()]), false)
            .unwrap();
        assert_eq!(rep2.rules_run, 0);
        assert!(rep2.violations.is_empty());
    }

    /// A language-mismatched rule is `not_applicable`; a rule whose candidate set is empty is `silent` (so the UI can
    /// warn "no such nodes exist") rather than silently reporting zero.
    #[test]
    fn check_reports_not_applicable_and_silent() {
        let (store, pid) = seed_store();
        let svc = RuleService::new(
            Arc::clone(&store),
            Arc::new(TestProvider {
                rules: vec![rule(
                    "java-only",
                    &["Class"],
                    &["java"],
                    vec![CheckPredicate::KindIn(vec!["Class".into()])],
                )],
            }),
        );
        let rep = svc.check(pid, None, false).unwrap();
        assert_eq!(rep.rules_run, 0, "语言不匹配应跳过");
        assert!(
            !rep.rules_not_applicable.is_empty(),
            "应记录 not_applicable 原因"
        );

        let svc2 = RuleService::new(
            Arc::clone(&store),
            Arc::new(TestProvider {
                rules: vec![rule("ghost", &["GhostKind"], &["php"], vec![])],
            }),
        );
        let rep2 = svc2.check(pid, None, false).unwrap();
        assert_eq!(rep2.rules_run, 1, "需求满足仍应被选入 runnable");
        assert_eq!(rep2.violations.len(), 0);
        assert!(
            !rep2.rules_silent.is_empty(),
            "空候选集应标记 silent"
        );
    }

    #[test]
    fn rules_returns_provider_rules() {
        let store = Arc::new(SqliteStore::in_memory().expect("in-memory store"));
        let svc = RuleService::new(
            store,
            Arc::new(TestProvider {
                rules: vec![rule("a", &[], &[], vec![])],
            }),
        );
        assert_eq!(svc.rules().len(), 1);
        assert_eq!(svc.rules()[0].id, "a");
    }

    #[test]
    fn scope_kinds_falls_back_to_any_when_empty() {
        assert_eq!(
            scope_kinds(&rule("x", &[], &["php"], vec![])),
            vec!["".to_string()]
        );
        assert_eq!(
            scope_kinds(&rule("y", &["Class"], &["php"], vec![])),
            vec!["Class".to_string()]
        );
    }

    #[test]
    fn project_env_describe_formats_stack() {
        let env = ProjectEnv {
            languages: vec!["php".into()],
            frameworks: vec![],
            edge_kinds: vec![],
            annotation_kinds: vec![],
        };
        assert_eq!(env.describe(), "php");
        let env2 = ProjectEnv {
            languages: vec!["php".into()],
            frameworks: vec!["thinkphp".into()],
            edge_kinds: vec![],
            annotation_kinds: vec![],
        };
        assert_eq!(env2.describe(), "php（thinkphp）");
        let env3 = ProjectEnv {
            languages: vec![],
            frameworks: vec![],
            edge_kinds: vec![],
            annotation_kinds: vec![],
        };
        assert_eq!(env3.describe(), "unknown");
    }

    #[test]
    fn reset_and_batch_rule_config() {
        let store = Arc::new(SqliteStore::in_memory().expect("in-memory store"));
        let svc = RuleService::new(store, Arc::new(TestProvider { rules: vec![] }));
        let pid = ProjectId::new(1);
        svc.set_rule_config(ProjectRuleConfig {
            project_id: pid,
            rule_id: "r1".into(),
            enabled: Some(true),
            options: json!({}),
        })
        .unwrap();
        svc.reset_rule_config(pid, "r1").unwrap();
        assert!(
            svc.rule_configs(pid).unwrap().get("r1").is_none(),
            "reset 后配置应消失"
        );

        svc.batch_rule_config(
            pid,
            vec![
                RuleConfigPatch {
                    rule_id: "a".into(),
                    enabled: Some(false),
                    options: None,
                },
                RuleConfigPatch {
                    rule_id: "b".into(),
                    enabled: Some(true),
                    options: Some(json!({ "k": 1 })),
                },
            ],
        )
        .unwrap();
        let cfgs = svc.rule_configs(pid).unwrap();
        assert!(cfgs.contains_key("a"));
        assert!(cfgs.contains_key("b"));
    }
}
