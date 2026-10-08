//! Diagnostics persistence contract tests (`DiagnosticSink`).
//!
//! Diagnostics are a first-class product (violations / missing links). The re-run correctness depends on
//! `clear_diagnostics` scoping by code prefix: a regression that clears the wrong prefix (or ignores it) leaves
//! "historical conclusions" on screen, or wipes the wrong category. These guard the push / list / clear / count
//! surface.

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{Diagnostic, Phase, ProjectId, Severity, SubProjectId};
use gt_domain::port::{DiagnosticSink, ProjectWriter};

fn store() -> SqliteStore {
    SqliteStore::in_memory().expect("open")
}

fn proj(s: &SqliteStore) -> ProjectId {
    proj_named(s, "diag")
}

fn proj_named(s: &SqliteStore, name: &str) -> ProjectId {
    s.create_project(gt_domain::model::NewProject {
        name: name.into(),
        root_path: "/tmp/diag".into(),
        description: None,
        config: None,
    })
    .expect("create")
    .id
}

fn diag(pid: ProjectId, code: &str, sev: Severity) -> Diagnostic {
    diag_with(pid, None, code, sev)
}

fn diag_with(pid: ProjectId, sp: Option<SubProjectId>, code: &str, sev: Severity) -> Diagnostic {
    Diagnostic {
        project_id: pid,
        sub_project_id: sp,
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

/// Re-run correctness and multi-tenancy depend on `project_id` scoping: a project must only ever see its own
/// diagnostics, and clearing one project's `rule:` violations must not touch another's. Guarded in `src/store.rs`
/// directly; this pins it through the public `DiagnosticSink` surface.
#[test]
fn diagnostics_are_isolated_across_projects() {
    let s = store();
    let p1 = proj_named(&s, "a");
    let p2 = proj_named(&s, "b");
    s.push_diagnostics(&[
        diag(p1, "rule:hot-table", Severity::Warning),
        diag(p2, "rule:hot-table", Severity::Error),
        diag(p2, "engine:missing-root", Severity::Error),
    ])
    .expect("push");

    let a = s.list_diagnostics(p1, 100).expect("list a");
    assert_eq!(a.len(), 1, "project a sees only its own diagnostics: {a:?}");
    assert_eq!(a[0].code, "rule:hot-table");

    // Clearing project a's rule: violations must not leak into project b.
    let removed = s.clear_diagnostics(p1, "rule:").expect("clear a");
    assert_eq!(removed, 1);
    let b_remaining = s.list_diagnostics(p2, 100).expect("list b");
    assert_eq!(
        b_remaining.len(),
        2,
        "clearing project a must not leak into project b: {b_remaining:?}"
    );
}

/// `list_diagnostics_by_code` with `Some(sub_project_id)` must return that sub-project's own violations **plus**
/// shared (`sub_project_id IS NULL`) ones, while excluding every other sub-project's. The integration test only
/// ever passed `None`; this pins the sub-project scoping that the graph view's "shared nodes show under any filter"
/// semantics relies on. Guarded in `src/store.rs` directly; this pins it through the public surface.
#[test]
fn by_code_scopes_to_sub_project_but_keeps_shared() {
    let s = store();
    let pid = proj(&s);
    let sp1 = SubProjectId(10);
    let sp2 = SubProjectId(20);
    s.push_diagnostics(&[
        diag_with(pid, Some(sp1), "rule:hot-table", Severity::Warning),
        diag_with(pid, Some(sp2), "rule:pii-leak", Severity::Error),
        diag(pid, "rule:shared-table", Severity::Error), // shared: sub_project_id IS NULL
        diag(pid, "engine:missing-root", Severity::Error),
    ])
    .expect("push");

    let sp1_only = s
        .list_diagnostics_by_code(pid, "rule:", Some(&[sp1]), 100)
        .expect("by_code sp1");
    let codes: Vec<&str> = sp1_only.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&"rule:hot-table"),
        "the requesting sub-project's own violation must appear: {codes:?}"
    );
    assert!(
        codes.contains(&"rule:shared-table"),
        "shared (NULL) diagnostics are kept under any filter: {codes:?}"
    );
    assert!(
        !codes.contains(&"rule:pii-leak"),
        "another sub-project's violation must be excluded: {codes:?}"
    );
}

/// The diagnostics page shows the "non-rule" badge via `count_diagnostics_excluding` but the table via
/// `list_diagnostics_excluding`; if the two disagree the user sees a badge of 0 next to a full table. The trait
/// documents this must not happen — pinned here through the public surface (only covered in `src/store.rs`).
#[test]
fn count_excluding_matches_list_excluding_definition() {
    let s = store();
    let pid = proj(&s);
    s.push_diagnostics(&[
        diag(pid, "rule:hot-table", Severity::Warning),
        diag(pid, "rule:pii-leak", Severity::Error),
        diag(pid, "engine:missing-root", Severity::Error),
        diag(pid, "engine:broken-link", Severity::Warning),
    ])
    .expect("push");

    let listed = s.list_diagnostics_excluding(pid, "rule:", 100).expect("list excluding");
    let counted: u64 = s
        .count_diagnostics_excluding(pid, "rule:") // (severity, count)
        .expect("count excluding")
        .into_iter()
        .map(|(_, n)| n)
        .sum();
    assert_eq!(
        listed.len() as u64,
        counted,
        "list_excluding and count_excluding must share the same definition"
    );
    assert!(
        listed.iter().all(|d| !d.code.starts_with("rule:")),
        "no rule: diagnostic must appear in the excluded view: {listed:?}"
    );
}
