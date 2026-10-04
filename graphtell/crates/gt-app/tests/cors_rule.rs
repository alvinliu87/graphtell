//! Live (serve-layer) verification that the `cors-reflect-origin` FKB rule actually fires on a **real**
//! small PHP sample built through the full pipeline — not a hand-seeded graph. This guards against the
//! CORS detection silently breaking after it was moved out of the kernel `cors.rs` phase and into
//! `rules/php/security.yaml` (the kernel now only promotes a `HeaderAssignment` node; the rule does the judging).
//!
//! It also proves the negative case: a fixed allowlist value does **not** trigger the rule (no false positive).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, RuleService};
use gt_domain::model::{NewProject, ProjectId};
use gt_domain::port::{NoopObserver, Persistence, RuleProvider, SystemClock};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}

struct Built {
    container: Container,
    project_id: ProjectId,
}

/// Build one complete graph on a real (tiny) on-disk PHP project, exactly like `view_columns.rs`'s `build`.
fn build(root: PathBuf) -> Built {
    let data_dir = std::env::temp_dir().join(format!(
        "graphtell-cors-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&data_dir).expect("mkdir data");

    let config = AppConfig {
        data_dir,
        fkb_dir: Some(workspace_root().join("fkb")),
        views_dir: Some(workspace_root().join("views")),
        rules_dir: Some(workspace_root().join("rules")),
        bind: "127.0.0.1".into(),
        port: 0,
        ui_dir: None,
    };
    let container = Container::new(config).expect("container assembly should not fail");
    let projects = ProjectService::new(
        container.store.clone() as Arc<dyn Persistence>,
        Arc::new(SystemClock),
    );
    let pipeline = PipelineService::new(
        container.store.clone() as Arc<dyn Persistence>,
        Arc::clone(&container.deps),
        Arc::clone(&container.rules) as Arc<dyn RuleProvider>,
    );
    let project = projects
        .create(NewProject {
            name: "cors-sample".into(),
            root_path: root,
            description: None,
            config: None,
        })
        .expect("creating the project should not fail");
    pipeline
        .run(project.id, &NoopObserver)
        .expect("the graph build should not fail");
    Built { container, project_id: project.id }
}

#[test]
fn cors_reflect_origin_fires_on_real_php_sample() {
    let d = std::env::temp_dir().join(format!(
        "graphtell-cors-src-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("app")).expect("mkdir app");
    // composer.json makes the pipeline detect a PHP / Laravel sub-project.
    std::fs::write(
        d.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("composer");
    std::fs::write(
        d.join("app/CorsMiddleware.php"),
        r#"<?php
namespace App;

class CorsMiddleware
{
    public function handle($request, $next)
    {
        // reflected origin (vulnerable): the rule MUST fire
        $header['Access-Control-Allow-Origin'] = app()->request->header('origin');

        // fixed allowlist (safe control): the rule MUST NOT fire
        $header['Access-Control-Allow-Origin'] = 'https://trusted.example.com';

        return $next($request);
    }
}
"#,
    )
    .expect("php");

    let Built { container, project_id } = build(d);

    // This is exactly what the serve layer's `GET /api/projects/{id}/violations` calls.
    let svc = RuleService::new(
        container.store.clone() as Arc<dyn Persistence>,
        container.rules.clone() as Arc<dyn RuleProvider>,
    );
    let report = svc.check(project_id, None, false).expect("check should not fail");

    let cors: Vec<_> = report
        .violations
        .iter()
        .filter(|v| v.rule_id == "cors-reflect-origin")
        .collect();
    assert!(
        !cors.is_empty(),
        "cors-reflect-origin 应在真实 PHP 样本上至少命中 1 次；实际 violations={:?}",
        report
            .violations
            .iter()
            .map(|v| &v.rule_id)
            .collect::<Vec<_>>()
    );
    // The reflected assignment (rhs contains "origin") must be the one reported.
    assert!(
        cors
            .iter()
            .any(|v| v.node_name.to_ascii_lowercase().contains("origin")),
        "应命中反射型赋值（rhs 含 origin）；violations={:?}",
        cors.iter().map(|v| &v.node_name).collect::<Vec<_>>()
    );
    // The fixed allowlist value (rhs contains "trusted", not "origin") must NOT be reported.
    assert!(
        cors
            .iter()
            .all(|v| !v.node_name.to_ascii_lowercase().contains("trusted")),
        "固定值允许源不应被误报；violations={:?}",
        cors.iter().map(|v| &v.node_name).collect::<Vec<_>>()
    );
}
