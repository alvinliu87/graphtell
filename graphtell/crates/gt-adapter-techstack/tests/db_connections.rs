//! PHP `config/database.php` → `db_connections` integration test (public surface only).
//!
//! Guards the *enumeration* path: a project may declare several connections, and the connection **name**
//! is user-chosen — not always `mysql` (read/write splitting, several databases). The old rule read the
//! single pointer `connections.mysql.prefix`, which silently produced no `db_prefix` at all for any other
//! name, and with it lost every column-level fact (measured on sample_project: 0 columns, 0 PII annotations).

use std::path::PathBuf;

use gt_adapter_fs::StdFileSystem;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_techstack::PhpTechStackAdapter;
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

/// Like [`field_of`], but returns `None` instead of panicking — for the negative cases where the whole point
/// is that a field must **not** be produced.
fn field_opt<'a>(entry: &'a ManifestEntry, name: &str) -> Option<&'a str> {
    entry
        .fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("phpad_conn_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// The classic single-connection case must keep working exactly as before (sample_project: `eb_`).
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

/// The `default` connection name is read through `default_from` ("default"), which uses the same `env()`-default
/// fallback as the per-connection fields. When the project declares `env('DB_CONNECTION', 'pgsql')` the kernel must
/// still learn that `pgsql` is the default — otherwise every "which connection owns this table" lookup degrades.
/// This branch (an `env()`-valued `default`) is exercised by no other test, in `src` or here.
#[test]
fn default_connection_resolves_via_env_fallback() {
    let root = tmp("default_env");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => env('DB_CONNECTION', 'pgsql'),\n    'connections' => [\n        'pgsql' => ['type' => 'pgsql', 'prefix' => 'pg_'],\n        'mysql' => ['type' => 'mysql', 'prefix' => 'lz_'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("connections must resolve");
    assert_eq!(
        out.default.as_deref(),
        Some("pgsql"),
        "the env-valued default must fall back to its literal default (pgsql)"
    );
    // Both connections still enumerate independently of which one is default.
    assert_eq!(field_of(find(&out.entries, "pgsql"), "prefix"), "pg_");
    assert_eq!(field_of(find(&out.entries, "mysql"), "prefix"), "lz_");
    let _ = std::fs::remove_dir_all(&root);
}

/// `default` is not assumed to be the first entry (or literally `mysql`) — it must resolve to whatever name is
/// declared. A project that routes writes through `order` must report `order` as default so the kernel attributes
/// tables to the right connection. No other test sets `default` to a non-first, non-mysql name.
#[test]
fn default_connection_can_point_to_any_named_connection() {
    let root = tmp("default_other");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'order',\n    'connections' => [\n        'mysql' => ['type' => 'mysql', 'prefix' => 'eb_'],\n        'order' => ['type' => 'mysql', 'prefix' => 'or_'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("connections must resolve");
    assert_eq!(
        out.default.as_deref(),
        Some("order"),
        "the default must resolve to the named connection, not the first entry"
    );
    assert_eq!(field_of(find(&out.entries, "order"), "prefix"), "or_");
    assert_eq!(field_of(find(&out.entries, "mysql"), "prefix"), "eb_");
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: the `env()`-default regex fallback may only run for a **single** entry — with several connections
/// it scans raw text for the first `'<leaf>' => …` and therefore cannot be attributed, so it must stay absent
/// rather than silently copying one connection's value onto all of them (src/php.rs:258). `mysql` declares
/// `env('DB_PREFIX', 'eb_')`; `order` declares a literal `or_`.
#[test]
fn env_fallback_is_disabled_for_multiple_connections() {
    let root = tmp("env_multi");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'mysql',\n    'connections' => [\n        'mysql' => ['type' => 'mysql', 'prefix' => env('DB_PREFIX', 'eb_')],\n        'order' => ['type' => 'mysql', 'prefix' => 'or_'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("connections must resolve");
    let mysql = find(&out.entries, "mysql");
    assert_eq!(
        field_opt(mysql, "prefix"),
        None,
        "an unattributable env() default must NOT be fabricated for a multi-connection project: {:?}",
        mysql.fields
    );
    // The resolvable fields of that same connection are still read.
    assert_eq!(field_of(mysql, "driver"), "mysql");
    // ...and no connection inherits another's prefix.
    assert_eq!(field_of(find(&out.entries, "order"), "prefix"), "or_");
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: a connection that does not declare the requested key must simply **omit** that field — no empty
/// string, no value borrowed from a sibling connection (src/php.rs:327 only pushes a pair when a value exists).
/// Fabricating a prefix would silently attribute tables to the wrong connection.
#[test]
fn missing_field_is_not_fabricated() {
    let root = tmp("missing_field");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'mysql',\n    'connections' => [\n        'mysql' => ['type' => 'mysql', 'prefix' => 'eb_'],\n        'order' => ['type' => 'mysql'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("connections must resolve");
    assert_eq!(out.entries.len(), 2);
    let order = find(&out.entries, "order");
    assert_eq!(
        field_opt(order, "prefix"),
        None,
        "a connection without a prefix must not gain one: {:?}",
        order.fields
    );
    // The fields it does declare are unaffected, and the sibling keeps its own prefix.
    assert_eq!(field_of(order, "driver"), "mysql");
    assert_eq!(field_of(find(&out.entries, "mysql"), "prefix"), "eb_");
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: when the config declares no `default`, `default` must be `None` — not guessed as the first entry
/// and not assumed to be `mysql` (src/php.rs:334). A guessed default would make the kernel attribute every
/// table to the wrong connection.
#[test]
fn absent_default_is_not_guessed() {
    let root = tmp("no_default");
    write_config(
        &root,
        "<?php\nreturn [\n    'connections' => [\n        'mysql' => ['type' => 'mysql', 'prefix' => 'eb_'],\n        'order' => ['type' => 'mysql', 'prefix' => 'or_'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("connections must still enumerate");
    assert_eq!(out.entries.len(), 2);
    assert_eq!(
        out.default, None,
        "an undeclared default must not be invented: {:?}",
        out.default
    );
    assert_eq!(field_of(find(&out.entries, "mysql"), "prefix"), "eb_");
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: every child **key** of a connection (`type`, `prefix`, `host`, `database`) must not become an entry
/// of its own — the enumeration collapses `connections.<name>.…` to the distinct `<name>` (src/php.rs:299),
/// so a richly configured connection is still one entry with several fields.
#[test]
fn sub_keys_do_not_become_extra_entries() {
    let root = tmp("subkeys");
    write_config(
        &root,
        "<?php\nreturn [\n    'default' => 'mysql',\n    'connections' => [\n        'mysql' => ['type' => 'mysql', 'prefix' => 'eb_', 'host' => '127.0.0.1', 'database' => 'app', 'port' => '3306'],\n    ],\n];",
    );

    let out = read(&root, "type").expect("connections must resolve");
    assert_eq!(
        out.entries.len(),
        1,
        "one connection with five settings must yield one entry, got {:?}",
        out.entries.iter().map(|e| &e.key).collect::<Vec<_>>()
    );
    let mysql = find(&out.entries, "mysql");
    assert_eq!(field_of(mysql, "prefix"), "eb_");
    // The undeclared fields are not invented either.
    assert_eq!(field_opt(mysql, "host"), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: a manifest that does not exist resolves to nothing rather than panicking or inventing a
/// connection (src/php.rs:273). Complements `missing_connections_key_resolves_to_nothing`, which covers a
/// file that exists but has no `connections` key.
#[test]
fn absent_manifest_file_resolves_to_nothing() {
    let root = tmp("no_file");
    // Deliberately no `write_config`: the file must not exist.

    assert!(
        read(&root, "type").is_none(),
        "a missing config/database.php must resolve to nothing"
    );
    let _ = std::fs::remove_dir_all(&root);
}
