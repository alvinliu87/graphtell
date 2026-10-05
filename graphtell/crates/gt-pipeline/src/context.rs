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
            // Match on **whole path segments**: `crmeb/app` must not claim `crmeb/app2/…`, which is a sibling
            // directory rather than this sub-project. An empty prefix (a sub-project sitting at the project
            // root) stays a catch-all, so every file still belongs to *some* sub-project.
            let hit = if prefix.is_empty() {
                true
            } else {
                relative.starts_with(&prefix)
                    && (relative.len() == prefix.len()
                        || relative.as_bytes().get(prefix.len()) == Some(&b'/'))
            };
            if hit && prefix.len() >= best_len {
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

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::{NodeKind, ProjectId, ProjectStatus, Selector};
    use std::path::PathBuf;

    const SUB_PHP: i64 = 1;
    const SUB_JS: i64 = 2;

    fn project(root: &str) -> Project {
        Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: PathBuf::from(root),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ctx_at(root: &str) -> PipelineContext {
        PipelineContext::new(project(root))
    }

    fn sub(id: i64, root: &str, language: &str) -> SubProject {
        SubProject {
            id: SubProjectId::new(id),
            project_id: ProjectId(1),
            name: format!("sub{id}"),
            root_path: PathBuf::from(root),
            language: Language::new(language),
            role: "backend".into(),
            detected_by: "test".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        }
    }

    /// A rule carrying no binding: `rules_for` only ever reads `id` / `phase` / `languages`.
    fn rule(id: &str, phase: &str, languages: Option<Vec<&str>>) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Node {
                node_kind: Some(NodeKind::from("Method")),
                r#where: vec![],
            },
            binding: vec![],
            confidence: 1.0,
            languages: languages.map(|l| l.into_iter().map(Language::new).collect()),
        }
    }

    fn ids(rules: &[Rule]) -> Vec<String> {
        rules.iter().map(|r| r.id.clone()).collect()
    }

    /// A policy distinguished only by its member separator.
    fn policy(member_separator: &str) -> NamespacePolicy {
        NamespacePolicy {
            member_separator: member_separator.to_string(),
            ..Default::default()
        }
    }

    // ---------------------------------------------------------------- `new()`

    /// Everything is unset at construction: no sub-project, no FKB vocabulary, and above all **no notation
    /// knowledge** — the fallback policy is the empty one, never PHP's.
    #[test]
    fn new_starts_empty_and_declares_no_notation_knowledge() {
        let ctx = ctx_at("/p");

        assert!(ctx.sub_projects.is_empty());
        assert!(ctx.files.is_empty());
        assert!(ctx.frameworks.is_empty());
        assert!(ctx.method_ref_specs.is_empty());
        assert!(ctx.magic_delegation.is_empty());
        assert!(ctx.db_verbs.is_empty());
        assert!(ctx.sign_check.is_empty());
        assert!(ctx.taint.is_empty());
        assert!(ctx.tx_calls.is_empty());
        assert!(ctx.entry_methods.is_empty());
        assert!(ctx.rules_by_sub.is_empty());
        assert!(ctx.rules_global.is_empty());
        assert!(ctx.propagation_seeds.is_empty());
        assert!(ctx.external_calls.is_empty());
        assert!(ctx.middleware_capabilities.is_empty());
        assert!(ctx.lang_policies.is_empty());

        // Every `Option` fallback starts unset: a stack has to declare its own vocabulary.
        assert!(ctx.method_ref_spec_default.is_none());
        assert!(ctx.magic_delegation_default.is_none());
        assert!(ctx.db_verbs_default.is_none());
        assert!(ctx.sign_check_default.is_none());
        assert!(ctx.taint_default.is_none());
        // `Vec` fallbacks start empty, which the phases read as "declares nothing".
        assert!(ctx.tx_calls_default.is_empty());
        assert!(ctx.entry_methods_default.is_empty());

        // The fallback notation policy is the **empty** one, not PHP's.
        assert_eq!(ctx.lang_policy_default, NamespacePolicy::default());
        assert!(ctx.lang_policy_default.member_separator.is_empty());
        assert!(ctx.lang_policy_default.variable_prefixes.is_empty());
        assert!(ctx.lang_policy_default.ns_separator.is_none());
    }

    // ---------------------------------------------------------------- notation policies

    #[test]
    fn lang_policy_prefers_the_registered_language_then_the_default() {
        let mut ctx = ctx_at("/p");
        ctx.lang_policies
            .insert(Language::PHP.to_string(), policy("::"));
        ctx.lang_policies
            .insert(Language::JAVA.to_string(), policy("."));
        ctx.lang_policy_default = policy("#");

        assert_eq!(
            ctx.lang_policy(Some(&Language::new(Language::PHP))).member_separator,
            "::"
        );
        assert_eq!(
            ctx.lang_policy(Some(&Language::new(Language::JAVA))).member_separator,
            "."
        );
        // An unregistered language must not silently inherit another stack's notation.
        assert_eq!(
            ctx.lang_policy(Some(&Language::new("ruby"))).member_separator,
            "#"
        );
        assert_eq!(ctx.lang_policy(None).member_separator, "#");
    }

    /// `lang_policy_for_sub` is the form every phase actually calls: sub -> language -> policy.
    #[test]
    fn lang_policy_for_sub_resolves_through_the_sub_projects_language() {
        let mut ctx = ctx_at("/p");
        ctx.lang_policies
            .insert(Language::PHP.to_string(), policy("::"));
        ctx.lang_policies
            .insert(Language::JAVASCRIPT.to_string(), policy("."));
        ctx.lang_policy_default = policy("#");
        ctx.sub_projects.push(sub(SUB_PHP, "/p/app", Language::PHP));
        ctx.sub_projects.push(sub(SUB_JS, "/p/web", Language::JAVASCRIPT));

        assert_eq!(
            ctx.lang_policy_for_sub(Some(SubProjectId::new(SUB_PHP)))
                .member_separator,
            "::"
        );
        assert_eq!(
            ctx.lang_policy_for_sub(Some(SubProjectId::new(SUB_JS)))
                .member_separator,
            "."
        );
        // An unknown sub or no sub at all leaves no language to look up, hence the fallback.
        assert_eq!(
            ctx.lang_policy_for_sub(Some(SubProjectId::new(99)))
                .member_separator,
            "#"
        );
        assert_eq!(ctx.lang_policy_for_sub(None).member_separator, "#");
    }

    // ---------------------------------------------------------------- `rules_for()`

    #[test]
    fn rules_for_returns_only_the_requested_phase() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(SUB_PHP, "/p/app", Language::PHP));
        ctx.rules_by_sub.insert(
            SUB_PHP,
            vec![
                rule("own_pre", "AnnotatePre", None),
                rule("own_post", "AnnotatePost", None),
            ],
        );
        ctx.rules_global
            .push((Language::new(Language::PHP), rule("g_pre", "AnnotatePre", None)));

        let pre = ctx.rules_for(Some(SubProjectId::new(SUB_PHP)), &Phase::new("AnnotatePre"));
        assert_eq!(ids(&pre), vec!["own_pre".to_string(), "g_pre".to_string()]);
        let post = ctx.rules_for(Some(SubProjectId::new(SUB_PHP)), &Phase::new("AnnotatePost"));
        assert_eq!(ids(&post), vec!["own_post".to_string()]);
    }

    /// Global rules are filtered by the sub-project's language — the reason `rules_global` carries its
    /// declaring language at all.
    #[test]
    fn rules_for_filters_global_rules_by_the_sub_projects_language() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(SUB_PHP, "/p/app", Language::PHP));
        ctx.rules_by_sub
            .insert(SUB_PHP, vec![rule("own", "P", None)]);
        ctx.rules_global
            .push((Language::new(Language::PHP), rule("g_php", "P", None)));
        ctx.rules_global
            .push((Language::new(Language::JAVA), rule("g_java", "P", None)));
        ctx.rules_global.push((Language::new("*"), rule("g_any", "P", None)));

        let got = ids(&ctx.rules_for(Some(SubProjectId::new(SUB_PHP)), &Phase::new("P")));
        assert!(got.contains(&"own".to_string()), "子项目自己的规则总在里面");
        assert!(got.contains(&"g_php".to_string()));
        assert!(
            !got.contains(&"g_java".to_string()),
            "Java 的全局规则不应进入 PHP 子项目"
        );
        assert!(
            got.contains(&"g_any".to_string()),
            "声明语言为 * 的 FKB 对任何语言都适用"
        );
    }

    /// A rule's own `languages` overrides the language of the FKB that declared it.
    #[test]
    fn rules_for_honours_a_rules_own_language_scoping() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(SUB_PHP, "/p/app", Language::PHP));
        ctx.rules_global.push((
            Language::new(Language::JAVA),
            rule("java_fk_php_rule", "P", Some(vec![Language::PHP])),
        ));
        ctx.rules_global.push((
            Language::new(Language::JAVA),
            rule("java_fk_inherited", "P", None),
        ));
        ctx.rules_global.push((
            Language::new(Language::JAVA),
            rule("wildcard", "P", Some(vec!["*"])),
        ));

        let got = ids(&ctx.rules_for(Some(SubProjectId::new(SUB_PHP)), &Phase::new("P")));
        assert!(
            got.contains(&"java_fk_php_rule".to_string()),
            "规则自身声明 php 时即生效（覆盖 FKB 的语言）"
        );
        assert!(
            !got.contains(&"java_fk_inherited".to_string()),
            "继承 FKB 语言 java 的规则对 php 子项目无效"
        );
        assert!(
            got.contains(&"wildcard".to_string()),
            "* 哨兵表示全语言适用"
        );
    }

    /// Cross-project nodes (no sub) take every global rule and **no** sub-project rule: there is no language
    /// to filter by, and per-sub rules belong to their sub.
    #[test]
    fn rules_for_without_a_sub_takes_every_global_rule() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(SUB_PHP, "/p/app", Language::PHP));
        ctx.rules_by_sub
            .insert(SUB_PHP, vec![rule("own", "P", None)]);
        ctx.rules_global
            .push((Language::new(Language::PHP), rule("g_php", "P", None)));
        ctx.rules_global
            .push((Language::new(Language::JAVA), rule("g_java", "P", None)));

        assert_eq!(
            ids(&ctx.rules_for(None, &Phase::new("P"))),
            vec!["g_php".to_string(), "g_java".to_string()]
        );
    }

    /// A sub id that is not a known sub-project yields no language, so the language filter is skipped and
    /// every global rule is taken. Pinned because the alternative reading ("an unknown sub gets nothing") is
    /// equally plausible.
    #[test]
    fn rules_for_unknown_sub_takes_every_global_rule() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(SUB_PHP, "/p/app", Language::PHP));
        ctx.rules_global
            .push((Language::new(Language::PHP), rule("g_php", "P", None)));
        ctx.rules_global
            .push((Language::new(Language::JAVA), rule("g_java", "P", None)));

        assert_eq!(
            ids(&ctx.rules_for(Some(SubProjectId::new(99)), &Phase::new("P"))),
            vec!["g_php".to_string(), "g_java".to_string()]
        );
    }

    // ---------------------------------------------------------------- `sub_of_path()`

    /// Longest prefix wins, and the result must not depend on registration order.
    #[test]
    fn sub_of_path_picks_the_longest_matching_root() {
        let mut ctx = ctx_at("/p");
        // Deepest first, on purpose.
        ctx.sub_projects
            .push(sub(1, "/p/crmeb/app/admin", Language::PHP));
        ctx.sub_projects.push(sub(2, "/p/crmeb/app", Language::PHP));
        ctx.sub_projects.push(sub(3, "/p/crmeb", Language::PHP));

        assert_eq!(
            ctx.sub_of_path("crmeb/app/admin/C.php"),
            Some(SubProjectId::new(1))
        );
        assert_eq!(
            ctx.sub_of_path("crmeb/app/S.php"),
            Some(SubProjectId::new(2))
        );
        assert_eq!(
            ctx.sub_of_path("crmeb/composer.json"),
            Some(SubProjectId::new(3))
        );
    }

    /// A sub-project sitting at the project root has an empty prefix and acts as a catch-all, but a deeper
    /// match still wins.
    #[test]
    fn sub_of_path_treats_a_root_level_sub_project_as_a_catch_all() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(2, "/p/crmeb/app", Language::PHP));
        ctx.sub_projects.push(sub(1, "/p", Language::PHP));

        assert_eq!(
            ctx.sub_of_path("crmeb/app/S.php"),
            Some(SubProjectId::new(2)),
            "更长的前缀仍优先"
        );
        assert_eq!(
            ctx.sub_of_path("anything/else.php"),
            Some(SubProjectId::new(1)),
            "根级子项目兜底"
        );
    }

    /// Regression: matching is on whole path segments. `crmeb/app2/…` shares the textual prefix `crmeb/app`
    /// but is a **sibling** directory, not that sub-project.
    #[test]
    fn sub_of_path_matches_whole_path_segments_only() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(1, "/p/crmeb/app", Language::PHP));

        assert_eq!(ctx.sub_of_path("crmeb/app2/S.php"), None);
        assert_eq!(ctx.sub_of_path("crmeb/application/S.php"), None);
        // The sub-project root itself still belongs to it.
        assert_eq!(ctx.sub_of_path("crmeb/app"), Some(SubProjectId::new(1)));
        assert_eq!(
            ctx.sub_of_path("crmeb/app/S.php"),
            Some(SubProjectId::new(1))
        );
    }

    #[test]
    fn sub_of_path_returns_none_when_nothing_matches() {
        let mut ctx = ctx_at("/p");
        assert_eq!(ctx.sub_of_path("crmeb/app/S.php"), None, "没有子项目时无从归属");

        ctx.sub_projects.push(sub(1, "/p/crmeb/app", Language::PHP));
        assert_eq!(ctx.sub_of_path("other/lib.php"), None, "落在所有子项目之外");
    }

    /// `relative_root` normalises separators, and keeps a root that is not under the project root verbatim.
    #[test]
    fn sub_of_path_normalises_separators_and_keeps_foreign_roots() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(1, "/p/crmeb\\app", Language::PHP));
        assert_eq!(
            ctx.sub_of_path("crmeb/app/S.php"),
            Some(SubProjectId::new(1)),
            "以反斜杠记录的路径仍应匹配"
        );

        ctx.sub_projects.push(sub(2, "/elsewhere/lib", Language::PHP));
        assert_eq!(
            ctx.sub_of_path("elsewhere/lib/S.php"),
            Some(SubProjectId::new(2)),
            "不在项目根下的路径原样使用"
        );
    }

    // ---------------------------------------------------------------- the remaining accessors

    #[test]
    fn language_of_falls_back_to_unknown() {
        let mut ctx = ctx_at("/p");
        ctx.sub_projects.push(sub(SUB_PHP, "/p/app", Language::PHP));

        assert_eq!(
            ctx.language_of(Some(SubProjectId::new(SUB_PHP))).0,
            Language::PHP
        );
        assert_eq!(
            ctx.language_of(Some(SubProjectId::new(99))).0,
            Language::UNKNOWN
        );
        assert_eq!(ctx.language_of(None).0, Language::UNKNOWN);
    }

    #[test]
    fn config_exposes_the_project_config() {
        let mut ctx = ctx_at("/p");
        assert_eq!(
            ctx.config().required_locales,
            vec!["zh-cn".to_string(), "en-us".to_string()]
        );
        ctx.project.config.table_prefixes = vec!["eb_".to_string()];
        assert_eq!(ctx.config().table_prefixes, vec!["eb_".to_string()]);
    }
}
