//! 流水线上下文：一次建图运行所共享的全部状态。

use std::collections::HashMap;

use gt_domain::model::{
    HandlerSpec, Language, NamespacePolicy, NodeId, Phase, Project, ProjectConfig, Rule,
    SourceFile, SubProject, SubProjectId,
};

use crate::workspace::GraphWorkspace;

/// 传播种子：合成阶段里，某个「动作发出方」方法节点与语义节点之间的边。
///
/// 传播阶段（P8）据此沿 `Calls` 调用链向上，把同一语义关系复刻到该方法的每一个调用方，
/// 从而兑现「一个功能最终调用了 FKB 认得的东西，就该被正确解析，无论多深」的原则。
///
/// 这是通用机制：不认识任何框架，只搬运「方法 → 语义节点」这一事实。
#[derive(Debug, Clone)]
pub struct PropSeed {
    /// 动作的发出方（被 FKB 规则命中的调用点所在方法）。
    pub source: NodeId,
    /// 语义节点（Queue / Table / Cache / ConfigKey …）。
    pub target: NodeId,
    /// 语义边种类（如 `PublishesTo` / `ReadsDb` / `ReadsCache` / `ReadsConfig`）。
    pub kind: String,
    pub confidence: f32,
    pub sub: Option<SubProjectId>,
    pub phase: Phase,
}

/// 一次流水线运行的上下文。
pub struct PipelineContext {
    pub project: Project,
    pub ws: GraphWorkspace,
    pub sub_projects: Vec<SubProject>,
    pub files: Vec<SourceFile>,
    /// 子工程 → 识别出的框架 id。
    pub frameworks: HashMap<i64, Vec<String>>,
    /// 子工程 → 路由 handler 解析规则（由 FKB 声明，P3 装配、P7 消费）。
    pub handler_specs: HashMap<i64, HandlerSpec>,
    /// 全局兜底的 handler 解析规则：单框架工程里 P7 常常拿不到子工程上下文，
    /// 用第一个声明了 `handler` 的框架兜底。
    pub handler_spec_default: Option<HandlerSpec>,
    /// 语言 → 命名空间 / 成员书写规则（P0 从解析器注册表抽取）。
    pub lang_policies: HashMap<String, NamespacePolicy>,
    /// 兜底书写规则（单语言工程 / 语言未知时使用）。
    pub lang_policy_default: NamespacePolicy,
    /// 子工程 → 消费入口方法名候选（FKB `entry_methods`）。
    pub entry_methods: HashMap<i64, Vec<String>>,
    /// 全局兜底的入口方法名（取第一个声明了 `entry_methods` 的框架）。
    pub entry_methods_default: Vec<String>,
    /// 子工程 → 该子工程适用的规则。
    pub rules_by_sub: HashMap<i64, Vec<Rule>>,
    /// 跨子工程共享的规则（合成节点可能跨工程汇聚），**带来源语言** ——
    /// 全局规则仍要按子工程语言过滤，否则会跨语言错配（见 `rules_for`）。
    pub rules_global: Vec<(Language, Rule)>,
    /// 传播种子：合成阶段产出的「方法 → 语义节点」动作边，待 P8 沿调用链向上复刻。
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
            handler_specs: HashMap::new(),
            handler_spec_default: None,
            lang_policies: HashMap::new(),
            lang_policy_default: NamespacePolicy::default(),
            entry_methods: HashMap::new(),
            entry_methods_default: Vec::new(),
            rules_by_sub: HashMap::new(),
            rules_global: Vec::new(),
            propagation_seeds: Vec::new(),
        }
    }

    /// 取某语言的命名空间 / 成员书写规则；未知语言用兜底策略。
    pub fn lang_policy(&self, lang: Option<&Language>) -> &NamespacePolicy {
        if let Some(l) = lang {
            if let Some(p) = self.lang_policies.get(l.as_str()) {
                return p;
            }
        }
        &self.lang_policy_default
    }

    /// 取某子工程所属语言的书写规则。
    pub fn lang_policy_for_sub(&self, sub: Option<SubProjectId>) -> &NamespacePolicy {
        let lang = sub.and_then(|s| {
            self.sub_projects
                .iter()
                .find(|x| x.id == s)
                .map(|x| x.language.clone())
        });
        self.lang_policy(lang.as_ref())
    }

    /// 取某子工程的规则；跨工程节点（sub 为空）用全局规则。
    pub fn rules_for(&self, sub: Option<SubProjectId>, phase: &Phase) -> Vec<Rule> {
        // 该子工程的语言（全局规则要按语言过滤，见下）。
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
        // 合成节点（Table / HttpContract）没有归属子工程时，也要应用全局规则。
        //
        // **按语言过滤**：全局规则来自各语言的框架 FKB（`rules_by_sub` 已经按语言
        // 装配过），若不过滤就会把 PHP 的 `config('key')` 规则套到 JS 的
        // `config(...)` 调用上 —— 用 A 语言的知识解释 B 语言的代码，凭空造节点。
        if sub.is_some() {
            out.extend(
                self.rules_global
                    .iter()
                    .filter(|(l, _)| lang.as_ref().map(|x| *l == *x).unwrap_or(true))
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
