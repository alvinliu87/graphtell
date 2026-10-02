//! `gt-adapter-rules` — the compliance-rule (Check) adapter.
//!
//! Loads check rules declared in YAML into the domain model [`CheckRule`] and implements the
//! [`RuleProvider`] outbound port.
//!
//! The kernel knows no concrete rule; adding a rule = adding one YAML file under `rules/`.

pub mod loader;

pub use loader::YamlRuleSet;
