//! The framework knowledge base port.

use crate::model::{FrameworkKnowledge, Language, Phase, Rule};

/// The FKB provider port.
///
/// The kernel does not care whether FKB comes from a YAML directory, a database or the network — only that it can be fetched.
pub trait KnowledgeProvider: Send + Sync {
    /// Every known framework.
    fn all(&self) -> Vec<&FrameworkKnowledge>;
    fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge>;

    /// Take all rules of a language for a phase (filtered by the framework id list).
    fn rules_for(&self, framework_ids: &[String], phase: &Phase) -> Vec<Rule> {
        let mut out = Vec::new();
        for id in framework_ids {
            if let Some(fk) = self.by_id(id) {
                out.extend(fk.rules.iter().filter(|r| r.phase == *phase).cloned());
            }
        }
        out
    }

    /// Frameworks supporting a language.
    fn for_language(&self, language: &Language) -> Vec<&FrameworkKnowledge> {
        self.all()
            .into_iter()
            .filter(|fk| fk.language == *language)
            .collect()
    }
}
