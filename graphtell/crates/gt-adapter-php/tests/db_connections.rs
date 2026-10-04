//! PHP `config/database.php` → `db_connections` integration test (public surface only).
//!
//! Guards the *enumeration* path: a project may declare several connections, and the connection **name**
//! is user-chosen — not always `mysql` (read/write splitting, several databases). The old rule read the
//! single pointer `connections.mysql.prefix`, which silently produced no `db_prefix` at all for any other
//! name, and with it lost every column-level fact (measured on CRMEB: 0 columns, 0 PII annotations).

use std::path::PathBuf;

use gt_adapter_fs::StdFileSystem;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_php::PhpTechStackAdapter;
use gt_domain::model::{EntryField, EntryFieldFrom, Language, ProjectId, SubProject, SubProjectId};
use gt_domain::port::{ManifestEntry, TechStackAdapter};

fn sub(root: PathBuf) -> SubProject {
    SubProject {
        id: SubProjectId::new(1),
        project_id: ProjectId::new(1),
        name: "app".into(),
        root_path: root,
        language: Language::new(Language::PHP),
        role: "backend".into(),
        detected_by: "composer.json".into(),
        frameworks: vec!["thinkphp".into()],
        facts: serde_json::Value::Null,
    }
}

/// ThinkPHP spells the driver `type`; Laravel spells it `driver` — same call, different field.
fn fields(driver_key: &str) -> Vec<EntryField> {
    vec![
        EntryField { name: "name".into(), pointer: None, from: Some(EntryFieldFrom::Key) },
        EntryField { name: "driver".into(), pointer: Some(format!("{{key}}.{driver_key}")), from: None },
        EntryField { name: "prefix".into(), pointer: Some("{key}.prefix".into()), from: None },
    ]
}

fn write_config(root: &PathBuf, body: &str) {
    let dir = root.join("config");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("database.php"), body).unwrap();
}

fn read(root: &PathBuf, driver_key: &str) -> Option<gt_domain::port::ManifestEntries> {
    let adapter = PhpTechStackAdapter::new();
    adapter.read_manifest_entries(
        &sub(root.clone()),
        root,
        "config/database.php",
        "connections",
        &fields(driver_key),
        Some("default"),
        &StdFileSystem::new(),
        &DefaultParserRegistry::new(),
    )
}

fn find<'a>(entries: &'a [ManifestEntry], name: &str) -> &'a ManifestEntry {
    entries
        .iter()
        .find(|e| e.key == name)
        .unwrap_or_else(|| panic!("connection `{name}` missing, got {:?}", entries.iter().map(|e| &e.key).collect::<Vec<_>>()))
}

fn field_of<'a>(entry: &'a ManifestEntry, name: &str) -> &'a str {
    entry
        .fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("field `{name}` missing on `{}`: {:?}", entry.key, entry.fields))
}

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("phpad_conn_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// The classic single-connection case must keep working exactly as before (CRMEB: `eb_`).
#[test]
fn single_mysql_connection_resolves_driver_and_prefix() {
    let root = tmp("single");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'mysql',\n    'connections' => [\n        'mysql' => [\n            'type' => 'mysql',\n            'prefix' => 'eb_',\n        ],\n    ],\n];",
    );

    let out = read(&root, "type").expect("one mysql connection must resolve");
    assert_eq!(out.default.as_deref(), Some("mysql"));
    assert_eq!(out.entries.len(), 1);
    let e = find(&out.entries, "mysql");
    assert_eq!(field_of(e, "name"), "mysql");
    assert_eq!(field_of(e, "driver"), "mysql");
    assert_eq!(field_of(e, "prefix"), "eb_");
    let _ = std::fs::remove_dir_all(&root);
}

/// Read/write splitting + a second database: every connection keeps **its own** prefix, and none is
/// cross-attributed (the `env()` fallback is deliberately disabled once there is more than one entry).
#[test]
fn multiple_connections_keep_their_own_prefix() {
    let root = tmp("multi");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'mysql',\n    'connections' => [\n        'mysql' => ['type' => 'mysql', 'prefix' => 'eb_'],\n        'mysql_read' => ['type' => 'mysql', 'prefix' => 'eb_'],\n        'order' => ['type' => 'mysql', 'prefix' => 'or_'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("three connections must resolve");
    assert_eq!(out.entries.len(), 3);
    assert_eq!(out.default.as_deref(), Some("mysql"));
    assert_eq!(field_of(find(&out.entries, "mysql"), "prefix"), "eb_");
    assert_eq!(field_of(find(&out.entries, "mysql_read"), "prefix"), "eb_");
    assert_eq!(field_of(find(&out.entries, "order"), "prefix"), "or_");
    assert_eq!(field_of(find(&out.entries, "order"), "driver"), "mysql");
    let _ = std::fs::remove_dir_all(&root);
}

/// The regression the old `connections.mysql.prefix` pointer could never cover: a project whose only
/// connection is named after its driver, with no `mysql` key anywhere.
#[test]
fn connection_not_named_mysql_is_still_resolved() {
    let root = tmp("pgsql");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'pgsql',\n    'connections' => [\n        'pgsql' => ['type' => 'pgsql', 'prefix' => 'pg_'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("pgsql-only project must resolve");
    assert_eq!(out.default.as_deref(), Some("pgsql"));
    assert_eq!(field_of(find(&out.entries, "pgsql"), "prefix"), "pg_");
    assert_eq!(field_of(find(&out.entries, "pgsql"), "driver"), "pgsql");
    let _ = std::fs::remove_dir_all(&root);
}

/// Laravel spells the driver `driver`; the FKB declares which key to read — the adapter knows neither.
#[test]
fn laravel_driver_spelling_resolves() {
    let root = tmp("laravel");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'pgsql',\n    'connections' => [\n        'pgsql' => ['driver' => 'pgsql', 'prefix' => ''],\n        'mysql' => ['driver' => 'mysql', 'prefix' => 'lz_'],\n    ],\n];",
    );

    let out = read(&root, "driver").expect("laravel connections must resolve");
    assert_eq!(field_of(find(&out.entries, "mysql"), "driver"), "mysql");
    assert_eq!(field_of(find(&out.entries, "pgsql"), "driver"), "pgsql");
    // An empty prefix is kept as-is: the kernel drops empties when merging into `table_prefixes`.
    assert_eq!(field_of(find(&out.entries, "pgsql"), "prefix"), "");
    let _ = std::fs::remove_dir_all(&root);
}

/// `env('DB_PREFIX', 'eb_')` cannot be evaluated statically; the literal default is recovered, and the
/// default connection name comes from `env('DB_CONNECTION', 'mysql')` the same way.
#[test]
fn env_defaults_are_recovered_for_a_single_connection() {
    let root = tmp("env");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => env('DB_CONNECTION', 'mysql'),\n    'connections' => [\n        'mysql' => ['type' => 'mysql', 'prefix' => env('DB_PREFIX', 'eb_')],\n    ],\n];",
    );

    let out = read(&root, "type").expect("env defaults must still resolve");
    assert_eq!(out.default.as_deref(), Some("mysql"));
    assert_eq!(field_of(find(&out.entries, "mysql"), "prefix"), "eb_");
    let _ = std::fs::remove_dir_all(&root);
}

/// No `connections` key at all → nothing to enumerate (and no guess).
#[test]
fn missing_connections_key_resolves_to_nothing() {
    let root = tmp("none");
    write_config(&root, "<?php\nreturn ['default' => 'mysql'];");

    assert!(read(&root, "type").is_none());
    let _ = std::fs::remove_dir_all(&root);
}
