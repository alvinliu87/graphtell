//! 流水线上下文：一次建图运行所共享的全部状态。

use std::collections::HashMap;

use em_domain::model::{
    Language, NodeId, Phase, Project, ProjectConfig, Rule, SourceFile, SubProject, SubProjectId,
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
    /// 子工程 → 该子工程适用的规则。
    pub rules_by_sub: HashMap<i64, Vec<Rule>>,
    /// 跨子工程共享的规则（合成节点可能跨工程汇聚）。
    pub rules_global: Vec<Rule>,
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
            rules_by_sub: HashMap::new(),
            rules_global: Vec::new(),
            propagation_seeds: Vec::new(),
        }
    }

    /// 取某子工程的规则；跨工程节点（sub 为空）用全局规则。
    pub fn rules_for(&self, sub: Option<SubProjectId>, phase: &Phase) -> Vec<Rule> {
        let mut out: Vec<Rule> = match sub {
            Some(s) => self
                .rules_by_sub
                .get(&s.get())
                .cloned()
                .unwrap_or_default(),
            None => self.rules_global.clone(),
        };
        // 合成节点（Table / HttpContract）没有归属子工程时，也要应用全局规则
        if sub.is_some() {
            out.extend(self.rules_global.iter().cloned());
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
