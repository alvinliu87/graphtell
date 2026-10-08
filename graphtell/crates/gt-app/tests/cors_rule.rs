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
        "cors-reflect-origin must hit at least once on the real PHP sample; violations={:?}",
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
        "the reflected assignment must be hit (rhs contains origin); violations={:?}",
        cors.iter().map(|v| &v.node_name).collect::<Vec<_>>()
    );
    // The fixed allowlist value (rhs contains "trusted", not "origin") must NOT be reported.
    assert!(
        cors
            .iter()
            .all(|v| !v.node_name.to_ascii_lowercase().contains("trusted")),
        "a fixed allow-origin value must not be a false positive; violations={:?}",
        cors.iter().map(|v| &v.node_name).collect::<Vec<_>>()
    );
}

/// Exercises the rule's **second** `any_of` branch (Form two): a `header("Access-Control-Allow-Origin: " . $origin)`
/// *call* (not the `$header[...] =` array assignment of Form one). The reflected value travels as a string
/// concatenation into the single `header()` argument — a different graph shape (`CallSite`, not `HeaderAssignment`)
/// that the first test never reaches. A regression that drops Form two leaves every concatenated-header CORS
/// reflection undetected.
#[test]
fn cors_reflect_origin_fires_on_header_call_form() {
    let d = std::env::temp_dir().join(format!(
        "graphtell-cors-call-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("app")).expect("mkdir app");
    std::fs::write(
        d.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("composer");
    // Only Form two here: a single header() call carrying the CORS header concatenated with a request variable.
    // There is no `$header['Access-Control-Allow-Origin'] = …` assignment, so any hit must come from the call form.
    std::fs::write(
        d.join("app/CorsMiddleware.php"),
        r#"<?php
namespace App;

class CorsMiddleware
{
    public function handle($request, $next)
    {
        // reflected origin via a header() call (Form two must fire)
        header("Access-Control-Allow-Origin: " . $request->header('origin'));
        return $next($request);
    }
}
"#,
    )
    .expect("php");

    let Built { container, project_id } = build(d);

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
        "cors-reflect-origin (Form two: header() call with a concatenated variable) must fire; violations={:?}",
        report
            .violations
            .iter()
            .map(|v| &v.rule_id)
            .collect::<Vec<_>>()
    );
    // The matched node must be the header() call site, not an array-assignment form.
    assert!(
        cors
            .iter()
            .any(|v| v.node_name.to_ascii_lowercase().contains("header")),
        "the reported node should be the header() call form; violations={:?}",
        cors.iter().map(|v| &v.node_name).collect::<Vec<_>>()
    );
}

/// Negative for Form two's `$`-variable requirement: `header("Access-Control-Allow-Origin: *")` is a static literal
/// with no request variable, so the `$` predicate must reject it (a wildcard is a *different* misconfiguration the
/// rule deliberately does not judge). Pins the branch boundary so a loosened predicate cannot start flagging literals.
#[test]
fn cors_reflect_origin_static_wildcard_header_does_not_fire() {
    let d = std::env::temp_dir().join(format!(
        "graphtell-cors-star-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("app")).expect("mkdir app");
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
        // Static wildcard — no request variable, Form two must NOT fire.
        header("Access-Control-Allow-Origin: *");
        return $next($request);
    }
}
"#,
    )
    .expect("php");

    let Built { container, project_id } = build(d);

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
        cors.is_empty(),
        "a static `Access-Control-Allow-Origin: *` must not trigger cors-reflect-origin; violations={:?}",
        cors.iter().map(|v| &v.node_name).collect::<Vec<_>>()
    );
}
