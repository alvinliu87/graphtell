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
    // Confirmed (critical): a superglobal request source is interpolated straight into a raw SQL call site,
    // so P9 must land the *confirmed* `tainted_raw` (not the unknown warning).
    public function detail()
    {
        $id = $_GET['id'];
        return Db::query("SELECT * FROM goods WHERE id = {$id}");
    }

    // Confirmed where-condition interpolation: `->where("...{$kw}...")` embeds a request variable in the
    // condition string. This is the where-family sink (tainted_where), which the original sample never exercised.
    public function search()
    {
        $kw = $_GET['kw'];
        return Db::table('goods')->where("CONCAT(',',roles,',') LIKE '%,{$kw},%'")->select();
    }

    // A variable whose source cannot be traced (here a plain function parameter, with no assignment-chain
    // link to a request source) must stay the *unknown* warning (`tainted_raw_unknown`), never be elevated to
    // the confirmed critical. This pins P9's documented precision: it does not false-positive a whole-variable
    // pass-in as confirmed — better a false negative (exposed by `rules_silent`) than an unexplainable critical.
    // (Note: `request()->param` *is* a recognised source in fkb/php/common.yaml, so the unrecognised case must
    // be a parameter / config read, not a framework request helper.)
    public function vague($x)
    {
        return Db::query("SELECT * FROM t WHERE c = {$x}");
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

/// Total number of annotations of a given kind across the whole graph. Used to pin that each sink fires
/// exactly once (and that the unrecognised-source case is demoted, never quietly promoted).
fn annotation_count(b: &common::Built, kind: &str) -> usize {
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
    let mut c = 0usize;
    for n in &nodes {
        if let Ok(anns) = b.store.annotations_of(n.id) {
            c += anns.iter().filter(|a| a.kind == kind).count();
        }
    }
    c
}

#[test]
fn request_value_interpolated_into_raw_sql_is_annotated() {
    let root = synthetic_php_taint_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP project graph build should succeed");
    };

    let kinds = annotation_kinds(&b);
    // The original test only required "tainted_raw OR tainted_raw_unknown", which the unproven path alone
    // satisfied — so the *confirmed* request-source trace (the whole point of P9's backward tracing) was never
    // pinned. Now each case is counted precisely:
    assert!(
        kinds.iter().any(|k| k == "tainted_raw" || k == "tainted_raw_unknown"),
        "a request value concatenated into Db::query must be annotated by P9: {kinds:?}"
    );
    // (1) detail(): a superglobal source interpolated into Db::query -> the *confirmed* critical. Exactly one,
    // so a regression that dropped the source trace (and demoted everything to _unknown) cannot pass.
    assert_eq!(
        annotation_count(&b, "tainted_raw"),
        1,
        "the superglobal-sourced raw SQL must be the confirmed tainted_raw, got: {kinds:?}"
    );
    // (2) search(): an embedded variable in ->where(...) -> tainted_where. This sink was never exercised before.
    assert_eq!(
        annotation_count(&b, "tainted_where"),
        1,
        "the where-condition interpolation must be annotated tainted_where, got: {kinds:?}"
    );
    // (3) vague(): request()->param is NOT a recognised source, so it stays the *unknown* warning — and,
    // crucially, must NOT be counted among the confirmed tainted_raw (the count above would be 2 otherwise).
    assert_eq!(
        annotation_count(&b, "tainted_raw_unknown"),
        1,
        "request()->param must be demoted to tainted_raw_unknown, not elevated to tainted_raw, got: {kinds:?}"
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
    let kinds = annotation_kinds(&b);
    assert!(
        !kinds.is_empty(),
        "with no framework detected, the taint vocabulary declared by the language-generic layer must still apply"
    );
    assert!(
        kinds.iter().any(|k| k.starts_with("tainted_")),
        "the applied vocabulary must actually be the SQL-injection taint kinds, got: {kinds:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
