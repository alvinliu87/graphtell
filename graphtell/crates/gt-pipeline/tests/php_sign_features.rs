//! End-to-end self-check of **P11 Sign** (signature-verification quality).
//!
//! Two things are pinned:
//! * the vocabulary — which calls compute a signature and which algorithms are weak — comes from **FKB**
//!   (`sign_check` in `fkb/php/common.yaml`), not from a `language == php` gate in the kernel. A stack that
//!   declares nothing is simply not judged, instead of inheriting PHP's vocabulary;
//! * with PHP's declaration in place, a loose `==` comparison of a signature and a weak `md5()` are both
//!   annotated — this is what proves the FKB → `ctx.sign_check` → P11 wiring really runs. Without an
//!   end-to-end test here, a broken wiring shows up only as "no annotations", which nothing else catches.

use gt_domain::model::ProjectConfig;
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

/// A synthetic PHP payment callback: `md5(...)` computed into `$calc`, then compared loosely with `$sign`.
fn synthetic_php_pay_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-sign-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/api/controller")).expect("mkdir");

    std::fs::write(
        dir.join("composer.json"),
        r#"{ "name": "demo/pay", "require": { "php": ">=7.4" } }"#,
    )
    .expect("write composer.json");

    std::fs::write(
        dir.join("app/api/controller/Pay.php"),
        r#"<?php
namespace app\api\controller;

class Pay
{
    public function notify()
    {
        $sign = $_POST['sign'];
        $calc = md5($this->buildSign());

        if ($sign == $calc) {
            return 'ok';
        }

        return 'fail';
    }

    private function buildSign()
    {
        return hash_hmac('sha256', 'body', 'secret');
    }
}
"#,
    )
    .expect("write Pay.php");

    dir
}

/// Every annotation kind present in the built graph.
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
fn php_signature_checks_are_annotated_from_fkb_vocabulary() {
    let root = synthetic_php_pay_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP project graph build should succeed");
    };

    let kinds = annotation_kinds(&b);
    assert!(
        kinds.iter().any(|k| k == "weak_sign_hash"),
        "a weak-hash signature must be annotated (md5 + an argument containing sign): {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "weak_sign_compare"),
        "a loose comparison of the signature must be annotated ($sign == $calc): {kinds:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A PHP sub-project gets PHP's vocabulary through the **unconditional language layer**
/// (`apply_without_detection`), which is never in the detected-framework list. If the merge skipped that
/// layer, P11 would silently judge nothing — hence this end-to-end guard.
#[test]
fn vocabulary_reaches_the_phase_without_framework_detection() {
    let root = synthetic_php_pay_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP project graph build should succeed");
    };
    // The synthetic project declares no framework at all (only `require: php`), so the annotations above can
    // only have come from `fkb/php/common.yaml`'s `sign_check`.
    assert!(
        !annotation_kinds(&b).is_empty(),
        "with no framework detected, the signature vocabulary declared by the language-generic layer must still apply"
    );
    let _ = std::fs::remove_dir_all(&root);
}
