//! Sub-project persistence contract tests.
//!
//! Sub-projects are the unit the whole pipeline revolves around (the gt-pipeline/tests work centred on them), yet
//! `replace_sub_projects` / `list_sub_projects` / `update_sub_project_facts` / `set_sub_project_frameworks` had no
//! tests. A regression silently produces a wrong sub-project view — wrong language, role or framework — and every
//! downstream phase inherits it.

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{Language, NewSubProject};
use gt_domain::port::ProjectWriter;
use gt_domain::port::ProjectReader;

fn store() -> SqliteStore {
    SqliteStore::in_memory().expect("open")
}

fn new_proj(s: &SqliteStore) -> gt_domain::model::ProjectId {
    s.create_project(gt_domain::model::NewProject {
        name: "sp".into(),
        root_path: "/tmp/sp".into(),
        description: None,
        config: None,
    })
    .expect("create")
    .id
}

#[test]
fn replace_and_list_sub_projects() {
    let s = store();
    let pid = new_proj(&s);
    let subs = s
        .replace_sub_projects(
            pid,
            vec![
                NewSubProject {
                    project_id: pid,
                    name: "backend".into(),
                    root_path: "/tmp/sp/backend".into(),
                    language: Language::new("php"),
                    role: "backend".into(),
                    detected_by: "composer.json".into(),
                    frameworks: vec!["thinkphp".into()],
                    facts: serde_json::Value::Null,
                },
                NewSubProject {
                    project_id: pid,
                    name: "frontend".into(),
                    root_path: "/tmp/sp/frontend".into(),
                    language: Language::new("javascript"),
                    role: "frontend".into(),
                    detected_by: "package.json".into(),
                    frameworks: vec!["uni-app".into()],
                    facts: serde_json::Value::Null,
                },
            ],
        )
        .expect("replace_sub_projects");
    assert_eq!(subs.len(), 2);

    let listed = s.list_sub_projects(pid).expect("list_sub_projects");
    assert_eq!(listed.len(), 2);
    let names: Vec<&str> = listed.iter().map(|x| x.name.as_str()).collect();
    assert!(names.contains(&"backend"));
    assert!(names.contains(&"frontend"));
    let backend = listed.iter().find(|x| x.name == "backend").expect("backend");
    assert_eq!(backend.language.as_str(), "php");
    assert_eq!(backend.role, "backend");
}

#[test]
fn update_sub_project_facts_and_frameworks() {
    let s = store();
    let pid = new_proj(&s);
    let subs = s
        .replace_sub_projects(
            pid,
            vec![NewSubProject {
                project_id: pid,
                name: "api".into(),
                root_path: "/tmp/sp/api".into(),
                language: Language::new("php"),
                role: "backend".into(),
                detected_by: "composer.json".into(),
                frameworks: vec!["laravel".into()],
                facts: serde_json::Value::Null,
            }],
        )
        .expect("replace");
    let sid = subs[0].id;

    s.update_sub_project_facts(sid, serde_json::json!({ "app_roots": ["app"] }))
        .expect("facts");
    s.set_sub_project_frameworks(sid, vec!["laravel".into(), "passport".into()])
        .expect("frameworks");

    let listed = s.list_sub_projects(pid).expect("list");
    let got = listed.iter().find(|x| x.id == sid).expect("the sub");
    assert_eq!(got.facts, serde_json::json!({ "app_roots": ["app"] }));
    assert_eq!(got.frameworks, vec!["laravel", "passport"]);
}

#[test]
fn replace_sub_projects_replaces_not_appends() {
    let s = store();
    let pid = new_proj(&s);
    let mk = |name: &str| NewSubProject {
        project_id: pid,
        name: name.into(),
        root_path: format!("/tmp/sp/{name}").into(),
        language: Language::new("php"),
        role: "backend".into(),
        detected_by: "composer.json".into(),
        frameworks: vec![],
        facts: serde_json::Value::Null,
    };
    s.replace_sub_projects(pid, vec![mk("a")]).expect("first");
    s.replace_sub_projects(pid, vec![mk("b")]).expect("second");
    let listed = s.list_sub_projects(pid).expect("list");
    assert_eq!(listed.len(), 1, "replacement must not accumulate");
    assert_eq!(listed[0].name, "b");
}
