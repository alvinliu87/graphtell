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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CheckRule;
    use serde_json::json;

    /// A rule needs many fields, but the default methods below only read `id` / `enabled`; build one from JSON
    /// and let every other field take its serde default.
    fn rule(id: &str, enabled: bool) -> CheckRule {
        serde_json::from_value(json!({
            "id": id,
            "title": id,
            "message": "m",
            "enabled": enabled,
        }))
        .unwrap_or_else(|e| panic!("cannot build rule {id}: {e}"))
    }

    /// A minimal in-memory `RuleProvider` so the trait's default `rule` / `enabled_rules` / `len` / `is_empty`
    /// methods can be exercised without a YAML loader.
    struct StubProvider {
        rules: Vec<CheckRule>,
    }
    impl RuleProvider for StubProvider {
        fn rules(&self) -> &[CheckRule] {
            &self.rules
        }
    }

    #[test]
    fn rule_lookup_by_id() {
        let p = StubProvider {
            rules: vec![rule("a", true), rule("b", false)],
        };
        assert_eq!(p.rule("a").map(|r| r.id.as_str()), Some("a"));
        assert_eq!(p.rule("b").map(|r| r.id.as_str()), Some("b"));
        assert!(p.rule("missing").is_none(), "an unknown id returns None");
    }

    #[test]
    fn enabled_rules_filters_by_enabled_flag() {
        let p = StubProvider {
            rules: vec![rule("a", true), rule("b", false), rule("c", true)],
        };
        let ids: Vec<&str> = p.enabled_rules().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"], "only enabled rules are kept, preserving their order");
    }

    #[test]
    fn len_and_is_empty() {
        let empty = StubProvider { rules: vec![] };
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let p = StubProvider {
            rules: vec![rule("a", true)],
        };
        assert!(!p.is_empty());
        assert_eq!(p.len(), 1);
    }
}
