//! Pipeline context: all the state shared by one graph-building run.

use std::collections::HashMap;

use gt_domain::model::{
    DbVerbsSpec, MethodRefSpec, Language, MagicDelegationSpec, MiddlewareCapability, SignCheckSpec,
    TaintSpec,
    NamespacePolicy, NodeId, Phase, Project, ProjectConfig, Rule, SourceFile, SubProject,
    SubProjectId,
};

use crate::workspace::GraphWorkspace;

/// A propagation seed: an edge between an "action emitter" method node and a semantic node, produced in the
/// synthesis phase.
///
/// The propagation phase (P8) walks upward along the `Calls` chain from here, replicating the same semantic
/// relation onto every caller of that method, thereby honouring the principle "if a feature ultimately calls
/// something FKB recognises, it should be resolved correctly no matter how deep".
///
/// This is a generic mechanism: it knows no framework and only carries the fact "method -> semantic node".
#[derive(Debug, Clone)]
pub struct PropSeed {
    /// The emitter of the action (the method holding the call site an FKB rule matched).
    pub source: NodeId,
    /// The semantic node (Queue / Table / Cache / ConfigKey …).
    pub target: NodeId,
    /// The semantic edge kind (e.g. `PublishesTo` / `ReadsDb` / `ReadsCache` / `ReadsConfig`).
    pub kind: String,
    pub confidence: f32,
    pub sub: Option<SubProjectId>,
    pub phase: Phase,
}

/// The context of one pipeline run.
pub struct PipelineContext {
    pub project: Project,
    pub ws: GraphWorkspace,
    pub sub_projects: Vec<SubProject>,
    pub files: Vec<SourceFile>,
    /// Sub-project -> the recognised framework id.
    pub frameworks: HashMap<i64, Vec<String>>,
    /// Sub-project -> route handler resolution rules (declared by FKB, assembled by P3, consumed by P7).
    pub method_ref_specs: HashMap<i64, MethodRefSpec>,
    /// Global fallback handler resolution rules: in a single-framework project P7 often cannot reach the
    /// sub-project context, so the first framework that declares `handler` serves as the fallback.
    pub method_ref_spec_default: Option<MethodRefSpec>,
    /// Sub-project -> the forwarding target of `@method` magic methods (FKB `magic_delegation`).
    pub magic_delegation: HashMap<i64, MagicDelegationSpec>,
    /// Global fallback forwarding target (chosen the same way as `method_ref_spec_default`).
    pub magic_delegation_default: Option<MagicDelegationSpec>,
    /// Sub-project -> the data model's read / write verbs (FKB `db_verbs`).
    pub db_verbs: HashMap<i64, DbVerbsSpec>,
    /// Global fallback read / write verbs.
    pub db_verbs_default: Option<DbVerbsSpec>,
    /// Sub-project -> signature-verification vocabulary (FKB `sign_check`).
    pub sign_check: HashMap<i64, SignCheckSpec>,
    /// Global fallback signature vocabulary.
    pub sign_check_default: Option<SignCheckSpec>,
    /// Sub-project -> SQL-injection vocabulary (FKB `taint`).
    pub taint: HashMap<i64, TaintSpec>,
    /// Global fallback taint vocabulary.
    pub taint_default: Option<TaintSpec>,
    /// Language -> namespace / member notation rules (extracted by P0 from the parser registry).
    pub lang_policies: HashMap<String, NamespacePolicy>,
    /// Fallback notation rules (used for single-language projects / unknown languages).
    pub lang_policy_default: NamespacePolicy,
    /// The external-system-call list (FKB `external_calls`); cross-framework differences are small, so one globally merged result suffices.
    pub external_calls: Vec<String>,
    /// The "middleware class -> capability" mapping (FKB `middleware_capabilities`); likewise merged globally.
    pub middleware_capabilities: Vec<MiddlewareCapability>,
    /// Sub-project -> transaction-boundary markers (FKB `tx_calls`).
    ///
    /// Kept **per sub-project** (unlike `external_calls`): P13 asks "does this method open a transaction",
    /// and answering that requires knowing what a transaction looks like *in this stack*. With a single
    /// global list, a stack that declares no markers would still be judged against another stack's, so
    /// every multi-table write in it would be reported — a false-positive flood, not a measurement.
    pub tx_calls: HashMap<i64, Vec<String>>,
    /// Global fallback transaction markers.
    pub tx_calls_default: Vec<String>,
    /// Sub-project -> candidate consumer entry method names (FKB `entry_methods`).
    pub entry_methods: HashMap<i64, Vec<String>>,
    /// Global fallback entry method names (taken from the first framework declaring `entry_methods`).
    pub entry_methods_default: Vec<String>,
    /// Sub-project -> the rules applicable to that sub-project.
    pub rules_by_sub: HashMap<i64, Vec<Rule>>,
    /// Rules shared across sub-projects (synthesised nodes may converge across projects), **carrying their source
    /// language** — global rules still have to be filtered by sub-project language, otherwise they mismatch across
    /// languages (see `rules_for`).
    pub rules_global: Vec<(Language, Rule)>,
    /// Propagation seeds: the "method -> semantic node" action edges produced by the synthesis phase, waiting for P8 to replicate them upward along the call chain.
    pub propagation_seeds: Vec<PropSeed>,
}

impl PipelineContext {
    pub fn new(project: Project) -> Self {
        let ws = GraphWorkspace::new(project.id);
        Self {
            project,
            ws,
            sub_projects: Vec::new(),
            files: Vec::new(),
            frameworks: HashMap::new(),
            method_ref_specs: HashMap::new(),
            method_ref_spec_default: None,
            magic_delegation: HashMap::new(),
            magic_delegation_default: None,
            db_verbs: HashMap::new(),
            db_verbs_default: None,
            sign_check: HashMap::new(),
            sign_check_default: None,
            taint: HashMap::new(),
            taint_default: None,
            external_calls: Vec::new(),
            middleware_capabilities: Vec::new(),
            tx_calls: HashMap::new(),
            tx_calls_default: Vec::new(),
            lang_policies: HashMap::new(),
            lang_policy_default: NamespacePolicy::default(),
            entry_methods: HashMap::new(),
            entry_methods_default: Vec::new(),
            rules_by_sub: HashMap::new(),
            rules_global: Vec::new(),
            propagation_seeds: Vec::new(),
        }
    }

    /// Take a language's namespace / member notation rules; an unknown language uses the fallback strategy.
    pub fn lang_policy(&self, lang: Option<&Language>) -> &NamespacePolicy {
        if let Some(l) = lang {
            if let Some(p) = self.lang_policies.get(l.as_str()) {
                return p;
            }
        }
        &self.lang_policy_default
    }

    /// Take the notation rules of the language a sub-project belongs to.
    pub fn lang_policy_for_sub(&self, sub: Option<SubProjectId>) -> &NamespacePolicy {
        let lang = sub.and_then(|s| {
            self.sub_projects
                .iter()
                .find(|x| x.id == s)
                .map(|x| x.language.clone())
        });
        self.lang_policy(lang.as_ref())
    }

    /// Take the rules of a sub-project; cross-project nodes (sub is empty) use the global rules.
    pub fn rules_for(&self, sub: Option<SubProjectId>, phase: &Phase) -> Vec<Rule> {
        // The language of that sub-project (global rules must be filtered by language, see below).
        let lang = sub.and_then(|s| {
            self.sub_projects
                .iter()
                .find(|x| x.id == s)
                .map(|x| x.language.clone())
        });
        let mut out: Vec<Rule> = match sub {
            Some(s) => self
                .rules_by_sub
                .get(&s.get())
                .cloned()
                .unwrap_or_default(),
            None => self.rules_global.iter().map(|(_, r)| r.clone()).collect(),
        };
        if sub.is_some() {
            out.extend(
                self.rules_global
                    .iter()
                    .filter(|(l, r)| match &lang {
                        Some(lang) => r.applies_to(l, lang),
                        None => true,
                    })
                    .map(|(_, r)| r.clone()),
            );
        }
        out.into_iter().filter(|r| r.phase == *phase).collect()
    }

    pub fn sub_of_path(&self, relative: &str) -> Option<SubProjectId> {
        let mut best: Option<SubProjectId> = None;
        let mut best_len = 0usize;
        for sub in &self.sub_projects {
            let prefix = self.relative_root(&sub.root_path);
            if (relative.starts_with(&prefix) || prefix.is_empty()) && prefix.len() >= best_len {
                best_len = prefix.len();
                best = Some(sub.id);
            }
        }
        best
    }

    fn relative_root(&self, abs: &std::path::Path) -> String {
        abs.strip_prefix(&self.project.root_path)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/")
            .trim_matches('/')
            .to_string()
    }

    pub fn config(&self) -> &ProjectConfig {
        &self.project.config
    }

    pub fn language_of(&self, sub: Option<SubProjectId>) -> Language {
        sub.and_then(|s| self.sub_projects.iter().find(|x| x.id == s))
            .map(|s| s.language.clone())
            .unwrap_or_else(|| Language(Language::UNKNOWN.to_string()))
    }
}
