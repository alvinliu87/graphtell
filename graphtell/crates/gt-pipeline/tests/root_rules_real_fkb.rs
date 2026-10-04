//! Always-on self-check that the **real** FKB's `root_rules` resolve against a synthetic project tree —
//! no `@samples`, no skip-trap.
//!
//! Why this is needed (the same gap `exclude.rs`'s real-FKB tests close for `exclude_rules`):
//! * `prepare.rs`'s `#[cfg(test)]` module builds hand-crafted `StaticKb` / inline `FrameworkKnowledge`
//!   objects — it proves the *engine* (selector matching, fact expansion, `{app_root}` substitution)
//!   works, but it never loads a real FKB, so a typo in a shipped rule's `pointer` / `source` slips past.
//! * the CRMEB integration test (`crmeb_pipeline.rs::prepare_resolves_app_root_from_composer_psr4`) *does*
//!   load the real FKB, but it is **sample-gated** — when `GRAPHTELL_SAMPLE_DIR` is unset it skips
//!   silently (false green), so CI without samples gets zero coverage of PHP root rules.
//!
//! This pins, on the shipped `fkb/php/{thinkphp,laravel}.yaml`, that:
//! * `app-root` resolves from `composer.json`'s `autoload.psr-4` (value `"app"`, not the fallback dir),
//!   and records `autoload.psr-4` as its source;
//! * `db-connections` resolves from `config/database.php`'s default connection prefix (`db_prefix == "eb_"`).
//!
//! The JS/Python stacks already have always-on synthetic integration tests (`node_real_samples.rs`,
//! `python_django_features.rs`) that run the real FKB, so this file concentrates on the PHP gap.

use gt_domain::model::ProjectConfig;
use gt_domain::port::ProjectReader;
use serde_json::Value;

mod common;

/// Build a minimal PHP project tree that (a) is detected as the given framework and (b) yields a PHP
/// sub-project so `root_rules` actually run. The single `app/Model/User.php` is what makes the scanner
/// create a PHP sub-project — without a `.php` source file there is no sub-project and root rules never fire.
fn synthetic_php_root(
    framework_dep: &str,
    vendor_ns: &str,
    app_dir: &str,
    db_driver_key: &str,
) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-root-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    // `create_dir_all` also creates the intermediate `app/` directory.
    std::fs::create_dir_all(dir.join(app_dir).join("Model")).expect("mkdir app source dir");
    std::fs::create_dir_all(dir.join("config")).expect("mkdir config");

    // `composer.json`: the detector's dependency *and* the `autoload.psr-4` that drives `app-root`.
    let composer = format!(
        r#"{{"require":{{"{dep}":"^6"}},"autoload":{{"psr-4":{{"{ns}\\":"{app}/"}}}}}}"#,
        dep = framework_dep,
        ns = vendor_ns,
        app = app_dir,
    );
    std::fs::write(dir.join("composer.json"), composer).expect("write composer.json");

    // `config/database.php`: a default connection carrying a table prefix -> `db_prefix`.
    // ThinkPHP spells the driver `type`; Laravel spells it `driver` (see the two FKBs).
    std::fs::write(
        dir.join("config").join("database.php"),
        format!(
            r#"<?php
return [
    'default' => 'mysql',
    'connections' => [
        'mysql' => [
            '{driver_key}' => 'mysql',
            'prefix' => 'eb_',
        ],
    ],
];"#,
            driver_key = db_driver_key,
        ),
    )
    .expect("write config/database.php");

    // A minimal PHP class so the scanner groups a PHP sub-project (root rules run per sub-project).
    std::fs::write(
        dir.join(app_dir).join("Model").join("User.php"),
        format!("<?php\nnamespace {ns};\nclass User {{}}\n", ns = vendor_ns),
    )
    .expect("write php source");

    dir
}

/// Assert the resolved `app_root` and `db_prefix` facts on the PHP sub-project.
fn assert_php_root_rules(root: &std::path::Path, framework: &str) {
    let Some(b) = common::graph_with_root(root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic PHP project should succeed");
    };
    let subs = b
        .store
        .list_sub_projects(b.project.id)
        .expect("sub-projects readable");
    let sub = subs
        .iter()
        .find(|s| s.language.as_str() == "php")
        .expect("a PHP sub-project must exist");

    assert!(
        sub.frameworks.contains(&framework.to_string()),
        "应识别出 {framework}，实际：{:?}",
        sub.frameworks
    );

    let facts: Value = serde_json::from_value(sub.facts.clone()).unwrap_or(Value::Null);

    // `app-root`: from composer `autoload.psr-4`, not the fallback directory.
    let app_root = facts
        .get("app_root")
        .expect("真实 FKB 应解析出 app_root 事实");
    assert_eq!(
        app_root.get("value").and_then(|v| v.as_str()),
        Some("app"),
        "AppRoot 应来自 composer.json 的 autoload.psr-4"
    );
    assert_eq!(
        app_root.get("fallback_used").and_then(|v| v.as_bool()),
        Some(false),
        "psr-4 命中后不应走兜底目录"
    );
    let source = app_root
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        source.contains("autoload.psr-4"),
        "app_root 的 source 必须记录 autoload.psr-4，实际：{source}"
    );

    // `db-connections`: `db_prefix` from the default connection's prefix in `config/database.php`.
    let db_prefix = facts
        .get("db_prefix")
        .expect("真实 FKB 应解析出 db_prefix 事实");
    assert_eq!(
        db_prefix.get("value").and_then(|v| v.as_str()),
        Some("eb_"),
        "db_prefix 应来自 config/database.php 默认连接的 prefix"
    );
    assert_eq!(
        db_prefix.get("fallback_used").and_then(|v| v.as_bool()),
        Some(false),
        "默认连接存在时应直接命中，不走兜底"
    );
}

#[test]
fn real_thinkphp_fkb_root_rules_resolve_without_fallback() {
    let root = synthetic_php_root("topthink/framework", "app", "app", "type");
    assert_php_root_rules(&root, "thinkphp");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn real_laravel_fkb_root_rules_resolve_without_fallback() {
    let root = synthetic_php_root("laravel/framework", "App", "app", "driver");
    assert_php_root_rules(&root, "laravel");
    let _ = std::fs::remove_dir_all(&root);
}
