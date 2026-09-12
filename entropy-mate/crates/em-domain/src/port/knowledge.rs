//! 框架知识库端口。

use crate::model::{FrameworkKnowledge, Language, Phase, Rule};

/// FKB 供给端口。
///
/// 内核不关心 FKB 来自 YAML 目录、数据库还是网络 —— 只要能取到即可。
pub trait KnowledgeProvider: Send + Sync {
    /// 全部已知框架。
    fn all(&self) -> Vec<&FrameworkKnowledge>;
    fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge>;

    /// 取某语言下、某阶段的所有规则（按框架 id 列表过滤）。
    fn rules_for(&self, framework_ids: &[String], phase: &Phase) -> Vec<Rule> {
        let mut out = Vec::new();
        for id in framework_ids {
            if let Some(fk) = self.by_id(id) {
                out.extend(fk.rules.iter().filter(|r| r.phase == *phase).cloned());
            }
        }
        out
    }

    /// 支持某语言的框架。
    fn for_language(&self, language: &Language) -> Vec<&FrameworkKnowledge> {
        self.all()
            .into_iter()
            .filter(|fk| fk.language == *language)
            .collect()
    }
}
