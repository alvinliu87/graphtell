//! Diagnostics persistence contract tests (`DiagnosticSink`).
//!
//! Diagnostics are a first-class product (violations / missing links). The re-run correctness depends on
//! `clear_diagnostics` scoping by code prefix: a regression that clears the wrong prefix (or ignores it) leaves
//! "historical conclusions" on screen, or wipes the wrong category. These guard the push / list / clear / count
//! surface.

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{Diagnostic, Phase, ProjectId, Severity};
use gt_domain::port::{DiagnosticSink, ProjectWriter};

fn store() -> SqliteStore {
    SqliteStore::in_memory().expect("open")
}

fn proj(s: &SqliteStore) -> ProjectId {
    s.create_project(gt_domain::model::NewProject {
        name: "diag".into(),
        root_path: "/tmp/diag".into(),
        description: None,
        config: None,
    })
    .expect("create")
    .id
}

fn diag(pid: ProjectId, code: &str, sev: Severity) -> Diagnostic {
    Diagnostic {
        project_id: pid,
        sub_project_id: None,
        phase: Phase("Check".to_string()),
        code: code.into(),
        severity: sev,
        message: format!("msg {code}"),
        location: None,
        payload: serde_json::Value::Null,
    }
}

#[test]
fn push_list_and_count_by_code() {
    let s = store();
    let pid = proj(&s);
    s.push_diagnostics(&[
        diag(pid, "rule:hot-table", Severity::Warning),
        diag(pid, "rule:pii-leak", Severity::Error),
        diag(pid, "engine:missing-root", Severity::Error),
    ])
    .expect("push");

    let all = s.list_diagnostics(pid, 100).expect("list");
    assert_eq!(all.len(), 3);

    let rules = s
        .list_diagnostics_by_code(pid, "rule:", None, 100)
        .expect("by_code");
    assert_eq!(rules.len(), 2, "only rule: violations");

    let counts = s.count_diagnostics_by_code(pid, "rule:", None).expect("count");
    // (severity, count): warning 1, error 1
    let by_sev: std::collections::HashMap<&str, u64> =
        counts.iter().map(|(sev, n)| (sev.as_str(), *n)).collect();
    assert_eq!(by_sev.get("warning").copied(), Some(1));
    assert_eq!(by_sev.get("error").copied(), Some(1));
}

#[test]
fn list_excluding_prefix_omits_rule_violations() {
    let s = store();
    let pid = proj(&s);
    s.push_diagnostics(&[
        diag(pid, "rule:hot-table", Severity::Warning),
        diag(pid, "engine:missing-root", Severity::Error),
    ])
    .expect("push");

    let non_rule = s
        .list_diagnostics_excluding(pid, "rule:", 100)
        .expect("excluding");
    assert_eq!(non_rule.len(), 1);
    assert_eq!(non_rule[0].code, "engine:missing-root");
}

#[test]
fn clear_diagnostics_scopes_by_prefix() {
    let s = store();
    let pid = proj(&s);
    s.push_diagnostics(&[
        diag(pid, "rule:hot-table", Severity::Warning),
        diag(pid, "rule:pii-leak", Severity::Error),
        diag(pid, "engine:missing-root", Severity::Error),
    ])
    .expect("push");

    let removed = s.clear_diagnostics(pid, "rule:").expect("clear");
    assert_eq!(removed, 2, "only rule: violations cleared");

    let remaining = s.list_diagnostics(pid, 100).expect("list");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].code, "engine:missing-root");
    assert!(
        s.list_diagnostics_by_code(pid, "rule:", None, 100)
            .expect("by_code")
            .is_empty(),
        "rule: bucket emptied"
    );
}
