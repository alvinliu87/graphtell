//! PHP migration → schema loader integration test (public surface only).
//!
//! Mirrors the gt-pipeline/tests dual-rail style: a synthetic temp project, no external sample. It guards the
//! `migration_schema` loader that turns Laravel / ThinkPHP `.php` migrations into `Schema` facts — the
//! *only* source of `Column` facts for PHP (ORM models declare no fields). A regression here means the schema
//! symbol table is empty and no column is ever materialised (measured on CRMEB: 0 columns, 0 PII annotations).

use std::path::PathBuf;

use gt_adapter_fs::StdFileSystem;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_php::PhpTechStackAdapter;
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
        "表前缀 eb_ 应被剥除: {tables:?}"
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
    assert!(facts.is_empty(), "未知 loader 应返回空向量而非报错");
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
        "栈前缀 id `php_migration_schema` 不应再被识别（已彻底移除别名）: {facts:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
