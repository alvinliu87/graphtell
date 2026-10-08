//! PHP migration → schema loader integration test (public surface only).
//!
//! Mirrors the gt-pipeline/tests dual-rail style: a synthetic temp project, no external sample. It guards the
//! `migration_schema` loader that turns Laravel / ThinkPHP `.php` migrations into `Schema` facts — the
//! *only* source of `Column` facts for PHP (ORM models declare no fields). A regression here means the schema
//! symbol table is empty and no column is ever materialised (measured on sample_project: 0 columns, 0 PII annotations).

use std::path::PathBuf;

use gt_adapter_fs::StdFileSystem;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_techstack::PhpTechStackAdapter;
use gt_domain::model::{Language, ProjectId, SubProject, SubProjectId};
use gt_domain::port::{AdapterFact, TechStackAdapter};

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

#[test]
fn migration_loader_emits_stripped_schema_facts() {
    let root = std::env::temp_dir().join(format!("phpad_mig_{}", std::process::id()));
    let mig = root.join("database/migrations");
    std::fs::create_dir_all(&mig).unwrap();
    std::fs::write(
        mig.join("2019_01_01_create_users.php"),
        "<?php\nSchema::create('eb_users', function ($table) {\n    $table->id();\n    $table->string('email');\n});",
    )
    .unwrap();
    std::fs::write(
        mig.join("2019_01_02_create_orders.php"),
        "<?php\nSchema::create('eb_orders', function ($table) {\n    $table->id();\n    $table->unsignedBigInteger('user_id');\n});",
    )
    .unwrap();

    let adapter = PhpTechStackAdapter::new();
    let facts = adapter
        .load(
            "migration_schema",
            &serde_json::json!({ "prefixes": ["eb_"] }),
            &sub(root.clone()),
            &root,
            &StdFileSystem::new(),
            &DefaultParserRegistry::new(),
            &[],
        )
        .expect("load ok");

    let mut tables: Vec<String> = facts
        .iter()
        .filter_map(|f| match f {
            AdapterFact::Schema { table, .. } => Some(table.clone()),
            _ => None,
        })
        .collect();
    tables.sort();
    assert_eq!(
        tables,
        vec!["orders".to_string(), "users".to_string()],
        "the eb_ table prefix must be stripped: {tables:?}"
    );

    let users = facts
        .iter()
        .find_map(|f| match f {
            AdapterFact::Schema { table, columns, .. } if table == "users" => Some(columns.clone()),
            _ => None,
        })
        .expect("users schema");
    assert_eq!(users, vec!["id".to_string(), "email".to_string()]);

    let _ = std::fs::remove_dir_all(&root);
}

/// Unknown loader ids must fall through to the generic built-ins, not error.
#[test]
fn unknown_loader_id_is_empty() {
    let root = std::env::temp_dir().join(format!("phpad_mig_unknown_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let adapter = PhpTechStackAdapter::new();
    let facts = adapter
        .load(
            "no_such_loader",
            &serde_json::Value::Null,
            &sub(root.clone()),
            &root,
            &StdFileSystem::new(),
            &DefaultParserRegistry::new(),
            &[],
        )
        .expect("load ok (empty)");
    assert!(facts.is_empty(), "an unknown loader must return an empty vector rather than error");
    let _ = std::fs::remove_dir_all(&root);
}

/// A stack-prefixed spelling must NOT be recognised: the id is language-agnostic, so there is exactly one
/// name for this loader. Accepting an alias is how a rename silently survives in old knowledge files.
#[test]
fn stack_prefixed_loader_id_is_not_recognised() {
    let root = std::env::temp_dir().join(format!("phpad_mig_alias_{}", std::process::id()));
    let mig = root.join("database/migrations");
    std::fs::create_dir_all(&mig).unwrap();
    std::fs::write(
        mig.join("2019_01_01_create_users.php"),
        "<?php\nSchema::create('eb_users', function ($table) {\n    $table->id();\n});",
    )
    .unwrap();

    let adapter = PhpTechStackAdapter::new();
    let facts = adapter
        .load(
            "php_migration_schema",
            &serde_json::json!({ "prefixes": ["eb_"] }),
            &sub(root.clone()),
            &root,
            &StdFileSystem::new(),
            &DefaultParserRegistry::new(),
            &[],
        )
        .expect("load ok");
    assert!(
        facts.is_empty(),
        "the stack-prefixed id `php_migration_schema` must no longer be recognised (the alias is gone for good): {facts:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `Schema::table(...)` (an alter, adding columns to an existing table) must be parsed exactly like
/// `Schema::create(...)` — the parser's `create|table` alternation is the only source of columns for *evolved*
/// tables. No test (here or in `src`) exercises `Schema::table`, so a broken alternation would ship silently and
/// every add-a-column migration would yield 0 columns.
#[test]
fn schema_table_alter_emits_columns() {
    let root = std::env::temp_dir().join(format!("phpad_mig_alter_{}", std::process::id()));
    let mig = root.join("database/migrations");
    std::fs::create_dir_all(&mig).unwrap();
    std::fs::write(
        mig.join("2020_01_01_add_tracking_to_orders.php"),
        "<?php\nSchema::table('eb_orders', function ($table) {\n    $table->string('tracking_no');\n    $table->boolean('shipped');\n});",
    )
    .unwrap();

    let adapter = PhpTechStackAdapter::new();
    let facts = adapter
        .load(
            "migration_schema",
            &serde_json::json!({ "prefixes": ["eb_"] }),
            &sub(root.clone()),
            &root,
            &StdFileSystem::new(),
            &DefaultParserRegistry::new(),
            &[],
        )
        .expect("load ok");

    let cols = facts
        .iter()
        .find_map(|f| match f {
            AdapterFact::Schema { table, columns, .. } if table == "orders" => Some(columns.clone()),
            _ => None,
        })
        .expect("orders schema from Schema::table");
    assert_eq!(
        cols,
        vec!["tracking_no".to_string(), "shipped".to_string()],
        "Schema::table must emit the added columns, not be skipped: {cols:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The migration directory is FKB-declared via `params.paths`, not hard-coded: projects that keep migrations under a
/// non-default fragment (e.g. `db/migrations`) must still load. Only pinned in `src/php.rs`; this guards it on the
/// public surface — a dropped `paths` param silently yields 0 columns for those projects.
#[test]
fn custom_migration_paths_param_is_honored() {
    let root = std::env::temp_dir().join(format!("phpad_mig_paths_{}", std::process::id()));
    let mig = root.join("db/migrations"); // non-default fragment
    std::fs::create_dir_all(&mig).unwrap();
    std::fs::write(
        mig.join("2021_01_01_create_posts.php"),
        "<?php\nSchema::create('eb_posts', function ($table) {\n    $table->id();\n    $table->string('title');\n});",
    )
    .unwrap();

    let adapter = PhpTechStackAdapter::new();
    let facts = adapter
        .load(
            "migration_schema",
            &serde_json::json!({ "prefixes": ["eb_"], "paths": ["db/migrations"] }),
            &sub(root.clone()),
            &root,
            &StdFileSystem::new(),
            &DefaultParserRegistry::new(),
            &[],
        )
        .expect("load ok");

    let mut tables: Vec<String> = facts
        .iter()
        .filter_map(|f| match f {
            AdapterFact::Schema { table, .. } => Some(table.clone()),
            _ => None,
        })
        .collect();
    tables.sort();
    assert_eq!(
        tables,
        vec!["posts".to_string()],
        "migrations under a custom `paths` fragment must be discovered: {tables:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Only Blueprint **column-declaration** methods count as columns. Modifiers that also take a string arg —
/// `$table->comment('邮箱')`, `$table->default('x')`, `$table->after('id')` — must NOT be mistaken for columns,
/// or the symbol table fills with garbage and PII/owner annotations land on the wrong fields. Only pinned in
/// `src/php.rs`; this guards it on the public surface the symbol table is built from.
#[test]
fn modifier_string_args_are_not_mistaken_for_columns() {
    let root = std::env::temp_dir().join(format!("phpad_mig_mod_{}", std::process::id()));
    let mig = root.join("database/migrations");
    std::fs::create_dir_all(&mig).unwrap();
    std::fs::write(
        mig.join("2022_01_01_create_users.php"),
        "<?php\nSchema::create('eb_users', function ($table) {\n    $table->id();\n    $table->string('email');\n    $table->comment('邮箱');\n    $table->default('x');\n    $table->after('id');\n    $table->unsignedBigInteger('user_id');\n});",
    )
    .unwrap();

    let adapter = PhpTechStackAdapter::new();
    let facts = adapter
        .load(
            "migration_schema",
            &serde_json::json!({ "prefixes": ["eb_"] }),
            &sub(root.clone()),
            &root,
            &StdFileSystem::new(),
            &DefaultParserRegistry::new(),
            &[],
        )
        .expect("load ok");

    let cols = facts
        .iter()
        .find_map(|f| match f {
            AdapterFact::Schema { table, columns, .. } if table == "users" => Some(columns.clone()),
            _ => None,
        })
        .expect("users schema");
    assert_eq!(
        cols,
        vec!["id".to_string(), "email".to_string(), "user_id".to_string()],
        "only column-declaration methods count; comment/default/after must be excluded: {cols:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
