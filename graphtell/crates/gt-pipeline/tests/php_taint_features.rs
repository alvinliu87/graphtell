//! End-to-end self-check of **P9 Taint** (SQL injection at the call site).
//!
//! P9 feeds `rules/php/security.yaml` (`has_annotation: tainted_raw` / `tainted_where`), so a broken phase
//! silently removes real vulnerabilities from the compliance report — and nothing else in the suite covers
//! it. Two things are pinned here:
//! * the vocabulary (which ORM methods run raw SQL, which expressions read user input) comes from **FKB**
//!   (`taint` in `fkb/php/common.yaml`), not from a `language == php` gate in the kernel;
//! * with PHP's declaration in place, a request value interpolated into raw SQL is really annotated.

use gt_domain::model::ProjectConfig;
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

/// A synthetic ThinkPHP-style callback: `Db::query("...{$id}...")` where `$id` comes from the request.
fn synthetic_php_taint_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-taint-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/controller")).expect("mkdir");

    std::fs::write(
        dir.join("composer.json"),
        r#"{ "name": "demo/shop", "require": { "php": ">=7.4" } }"#,
    )
    .expect("write composer.json");

    std::fs::write(
        dir.join("app/controller/Goods.php"),
        r#"<?php
namespace app\controller;

use think\facade\Db;

class Goods
{
    public function detail()
    {
        $id = request()->param('id');
        return Db::query("SELECT * FROM goods WHERE id = {$id}");
    }
}
"#,
    )
    .expect("write Goods.php");

    dir
}

fn annotation_kinds(b: &common::Built) -> Vec<String> {
    let nodes = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: None,
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("query nodes");
    let mut out = Vec::new();
    for n in &nodes {
        if let Ok(anns) = b.store.annotations_of(n.id) {
            for a in anns {
                out.push(a.kind.clone());
            }
        }
    }
    out
}

#[test]
fn request_value_interpolated_into_raw_sql_is_annotated() {
    let root = synthetic_php_taint_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP project graph build should succeed");
    };

    let kinds = annotation_kinds(&b);
    assert!(
        kinds.iter().any(|k| k == "tainted_raw" || k == "tainted_raw_unknown"),
        "请求值拼进 Db::query 应被 P9 标注: {kinds:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The vocabulary reaches the phase through the **unconditional language layer** (`apply_without_detection`),
/// which is never in the detected-framework list — exactly the merge arm that is easy to forget.
#[test]
fn taint_vocabulary_reaches_the_phase_without_framework_detection() {
    let root = synthetic_php_taint_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP project graph build should succeed");
    };
    // The synthetic project declares no framework (only `require: php`), so P9 can only have run from
    // `fkb/php/common.yaml`'s `taint`.
    assert!(
        !annotation_kinds(&b).is_empty(),
        "未检测到框架时，语言通用层声明的污点词汇仍须生效"
    );
    let _ = std::fs::remove_dir_all(&root);
}
