//! Per-project rule config contract tests (`RuleConfigStore`).
//!
//! A project can override a rule's enabled state and parameter values. A regression here means a user's toggle
//! "silently does nothing" (or is ignored), so a disabled rule still fires or an enabled one stays quiet. These
//! guard `set` / `get` / `delete` that previously had zero tests.

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{ProjectId, ProjectRuleConfig};
use gt_domain::port::{ProjectWriter, RuleConfigStore};

fn store() -> SqliteStore {
    SqliteStore::in_memory().expect("open")
}

fn proj(s: &SqliteStore) -> ProjectId {
    s.create_project(gt_domain::model::NewProject {
        name: "rc".into(),
        root_path: "/tmp/rc".into(),
        description: None,
        config: None,
    })
    .expect("create")
    .id
}

#[test]
fn set_get_and_delete_rule_config() {
    let s = store();
    let pid = proj(&s);

    // No overrides initially.
    assert!(s.get_rule_configs(pid).expect("get").is_empty());

    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "hot-table".into(),
        enabled: Some(false),
        options: serde_json::json!({ "min_fan_in": 99 }),
    })
    .expect("set");

    let cfg = s.get_rule_configs(pid).expect("get");
    assert_eq!(cfg.len(), 1);
    let hot = cfg.get("hot-table").expect("hot-table override");
    assert_eq!(hot.enabled, Some(false));
    assert_eq!(hot.options, serde_json::json!({ "min_fan_in": 99 }));

    // Updating the same rule replaces, not appends.
    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "hot-table".into(),
        enabled: Some(true),
        options: serde_json::Value::Null,
    })
    .expect("set again");
    assert_eq!(s.get_rule_configs(pid).expect("get").len(), 1);
    let hot = s.get_rule_configs(pid).expect("get").get("hot-table").cloned().unwrap();
    assert_eq!(hot.enabled, Some(true));

    s.delete_rule_config(pid, "hot-table").expect("delete");
    assert!(
        s.get_rule_configs(pid).expect("get").is_empty(),
        "deleting the override returns to inheriting the default"
    );
}

#[test]
fn multiple_rule_overrides_are_independent() {
    let s = store();
    let pid = proj(&s);
    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "a".into(),
        enabled: Some(false),
        options: serde_json::Value::Null,
    })
    .expect("set a");
    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "b".into(),
        enabled: Some(true),
        options: serde_json::Value::Null,
    })
    .expect("set b");

    let cfg = s.get_rule_configs(pid).expect("get");
    assert_eq!(cfg.len(), 2);
    assert_eq!(cfg.get("a").unwrap().enabled, Some(false));
    assert_eq!(cfg.get("b").unwrap().enabled, Some(true));
}
