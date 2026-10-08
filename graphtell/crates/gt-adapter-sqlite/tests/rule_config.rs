//! Per-project rule config contract tests (`RuleConfigStore`).
//!
//! A project can override a rule's enabled state and parameter values. A regression here means a user's toggle
//! "silently does nothing" (or is ignored), so a disabled rule still fires or an enabled one stays quiet. These
//! guard `set` / `get` / `delete`.

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

/// A "real override" is anything other than `enabled = None` **and** empty options. Two branches must be stored
/// (not deleted as a no-op): `enabled = Some(false)` (the user disabled the rule, distinct from "inherit default")
/// and `enabled = None` with pinned `options` (only the parameters are overridden). `set` upserts, so re-setting
/// the same rule replaces rather than appends. Only pinned in `src/store.rs`; this guards it on the public surface
/// the settings UI calls — a disabled rule must actually store `false`, not be silently dropped.
#[test]
fn real_overrides_persist_and_upsert() {
    let s = store();
    let pid = proj(&s);

    // Disabled is a real override: must be stored and round-trip as `false` (not treated as "clear").
    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "r".into(),
        enabled: Some(false),
        options: serde_json::json!({}),
    })
    .expect("set disabled");
    let got = s.get_rule_configs(pid).expect("get");
    assert_eq!(got.len(), 1, "a disabled override must be stored, not deleted");
    assert_eq!(
        got.get("r").unwrap().enabled,
        Some(false),
        "enabled=false must round-trip as false, not be dropped or flipped"
    );

    // Options-only override (enabled=None but parameters pinned) is also a real override: must be stored.
    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "r".into(),
        enabled: None,
        options: serde_json::json!({ "k": "v" }),
    })
    .expect("set options-only");
    let got = s.get_rule_configs(pid).expect("get");
    assert_eq!(got.len(), 1, "re-setting the same rule must upsert, not append a second row");
    assert_eq!(got.get("r").unwrap().enabled, None, "enabled=None must round-trip as None");
    assert_eq!(got.get("r").unwrap().options, serde_json::json!({ "k": "v" }));
}

/// An "empty override" (`enabled = None` **and** empty options) means "back to inherited default", so `set` must
/// delete the row rather than leave a tombstone. Only pinned in `src/store.rs`; this guards it on the public surface
/// — clearing a toggle in the UI must actually remove the override, otherwise the rule keeps firing on stale data.
#[test]
fn empty_override_deletes_the_row() {
    let s = store();
    let pid = proj(&s);
    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "r".into(),
        enabled: Some(false),
        options: serde_json::json!({ "x": 1 }),
    })
    .expect("set");
    assert_eq!(s.get_rule_configs(pid).expect("get").len(), 1);

    s.set_rule_config(&ProjectRuleConfig {
        project_id: pid,
        rule_id: "r".into(),
        enabled: None,
        options: serde_json::json!({}),
    })
    .expect("set empty override");
    assert!(
        s.get_rule_configs(pid).expect("get").is_empty(),
        "an empty override (enabled=None with empty options) must delete the row"
    );
}
