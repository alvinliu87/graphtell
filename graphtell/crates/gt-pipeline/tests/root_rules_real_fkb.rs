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
        "{framework} must be recognised, got: {:?}",
        sub.frameworks
    );

    let facts: Value = serde_json::from_value(sub.facts.clone()).unwrap_or(Value::Null);

    // `app-root`: from composer `autoload.psr-4`, not the fallback directory.
    let app_root = facts
        .get("app_root")
        .expect("the real FKB must resolve the app_root fact");
    assert_eq!(
        app_root.get("value").and_then(|v| v.as_str()),
        Some("app"),
        "AppRoot must come from autoload.psr-4 in composer.json"
    );
    assert_eq!(
        app_root.get("fallback_used").and_then(|v| v.as_bool()),
        Some(false),
        "once psr-4 hits, the fallback directory must not be used"
    );
    let source = app_root
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        source.contains("autoload.psr-4"),
        "the source of app_root must record autoload.psr-4, got: {source}"
    );

    // `db-connections`: `db_prefix` from the default connection's prefix in `config/database.php`.
    let db_prefix = facts
        .get("db_prefix")
        .expect("the real FKB must resolve the db_prefix fact");
    assert_eq!(
        db_prefix.get("value").and_then(|v| v.as_str()),
        Some("eb_"),
        "db_prefix must come from the prefix of the default connection in config/database.php"
    );
    assert_eq!(
        db_prefix.get("fallback_used").and_then(|v| v.as_bool()),
        Some(false),
        "when the default connection exists it hits directly, with no fallback"
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

// ------------------------------------------------- `directory_exists` root rules
//
// The two shipped FKBs whose `app_root` comes from a **directory probe** rather than a manifest pointer:
// `fkb/python/fastapi.yaml` probes `app`, `fkb/java/spring-boot.yaml` probes `src/main/java`. That path
// goes through `facts::resolve_directory_exists`, whose value is the **parent** of the marker path — so a
// single-module project that hits directly under the root resolves to `"."`.
//
// `fallback_used == false` and the `source` text are load-bearing here: fastapi's fallback is literally
// `app`, so if the probe failed the fallback could hand back a value that looks plausible. Only "not the
// fallback" proves the `directory_exists` source itself resolved (exactly the trap the PHP cases above hit,
// where a broken `pointer` still yielded `app_root == "app"` via the fallback).

fn synthetic_root(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-root-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for (rel, body) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, body).expect("write");
    }
    dir
}

/// Assert that a real FKB's `directory_exists` root rule resolved `app_root`.
fn assert_app_root_probed_from_a_directory(
    root: &std::path::Path,
    language: &str,
    framework: &str,
    expected: &str,
) {
    let Some(b) = common::graph_with_root(root, ProjectConfig::default()) else {
        panic!("the synthetic {framework} project graph build should succeed");
    };
    let subs = b
        .store
        .list_sub_projects(b.project.id)
        .expect("sub-projects readable");
    let sub = subs.iter().find(|s| s.language.as_str() == language).unwrap_or_else(|| {
        panic!(
            "a {language} sub-project must exist, got: {:?}",
            subs.iter().map(|s| s.language.as_str()).collect::<Vec<_>>()
        )
    });

    assert!(
        sub.frameworks.contains(&framework.to_string()),
        "{framework} must be recognised, got: {:?}",
        sub.frameworks
    );

    let facts: Value = serde_json::from_value(sub.facts.clone()).unwrap_or(Value::Null);
    let app_root = facts
        .get("app_root")
        .unwrap_or_else(|| panic!("the real FKB of {framework} must resolve the app_root fact, got facts={facts}"));

    assert_eq!(
        app_root.get("value").and_then(|v| v.as_str()),
        Some(expected),
        "the app_root of {framework} must come from the directory_exists probe"
    );
    assert_eq!(
        app_root.get("fallback_used").and_then(|v| v.as_bool()),
        Some(false),
        "once the probe hits, the fallback directory must not be used (otherwise you cannot tell which one took effect)"
    );
    let source = app_root
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        source.contains("directory exists"),
        "the source of app_root must record directory exists, got: {source}"
    );
}

#[test]
fn real_fastapi_fkb_app_root_comes_from_a_directory_probe() {
    // Detected on `requirements.txt` (a textual containment match), and `app_root` is the probe of `app`.
    let root = synthetic_root(
        "fastapi",
        &[
            ("requirements.txt", "fastapi>=0.100\nuvicorn\n"),
            (
                "app/main.py",
                r#"from fastapi import FastAPI

app = FastAPI()


@app.get("/orders")
def list_orders():
    return []
"#,
            ),
        ],
    );
    assert_app_root_probed_from_a_directory(&root, "python", "fastapi", ".");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn real_spring_boot_fkb_source_root_comes_from_a_directory_probe() {
    // Detected on `pom.xml`; `fkb/java/spring-boot.yaml`'s `source-root` probes `src/main/java`.
    let root = synthetic_root(
        "spring",
        &[
            (
                "pom.xml",
                r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
            ),
            (
                "src/main/java/demo/App.java",
                r#"package demo;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;

@SpringBootApplication
public class App {
    public static void main(String[] args) {
        SpringApplication.run(App.class, args);
    }
}
"#,
            ),
        ],
    );
    assert_app_root_probed_from_a_directory(&root, "java", "spring-boot", ".");
    let _ = std::fs::remove_dir_all(&root);
}
