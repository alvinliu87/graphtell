//! The rule-provider port.
//!
//! The kernel knows no concrete rule: rules are declared in YAML, loaded by an outbound adapter and injected here.
//! Adding a rule = adding one YAML file, no kernel change (open-closed principle).

use crate::model::CheckRule;

/// Rule provider.
pub trait RuleProvider: Send + Sync {
    /// Every loaded rule (in the declaration order from YAML).
    fn rules(&self) -> &[CheckRule];
    /// Look up by id.
    fn rule(&self, id: &str) -> Option<&CheckRule> {
        self.rules().iter().find(|r| r.id == id)
    }
    /// The enabled rules.
    fn enabled_rules(&self) -> Vec<&CheckRule> {
        self.rules().iter().filter(|r| r.enabled).collect()
    }
    /// Number of loaded rules.
    fn len(&self) -> usize {
        self.rules().len()
    }
    fn is_empty(&self) -> bool {
        self.rules().is_empty()
    }
}
