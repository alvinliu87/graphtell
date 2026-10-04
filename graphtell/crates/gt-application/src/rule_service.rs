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
