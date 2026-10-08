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

    // Correct, constant-time verification: `hash_equals` is listed in `hash_calls` (so it counts as a signature
    // computation) but is deliberately NOT in `weak_algos`, and it is a function call, not a `==`/`!=` — so it
    // must produce NO sign annotation at all. The original sample never exercised this, so a regression that
    // moved `hash_equals` into `weak_algos` (or treated any signature computation as loose) would have passed.
    public function safeVerify()
    {
        $sign = $_POST['sign'];
        $calc = hash_hmac('sha256', 'body', 'secret');

        if (hash_equals($sign, $calc)) {
            return 'ok';
        }

        return 'fail';
    }

    // Legacy `sha1` (also a weak algo) with a loose `==` comparison — must be flagged just like `md5`. The
    // sample only used md5 before, so a regression dropping sha1 from `weak_algos` would not have been caught.
    public function legacy()
    {
        $sign = $_POST['sign'];
        $calc = sha1($sign);

        if ($calc == $sign) {
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

/// Total number of annotations of a given kind across the whole graph.
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

/// Whether any `weak_sign_hash` annotation cites `algo` in its evidence. Used to pin that a *specific* weak
/// algorithm (and only that one) is judged — e.g. `md5`/`sha1` must be cited, `hash_equals` must not.
fn has_weak_hash_algo(b: &common::Built, algo: &str) -> bool {
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
    for n in &nodes {
        if let Ok(anns) = b.store.annotations_of(n.id) {
            for a in anns {
                if a.kind == "weak_sign_hash"
                    && a.evidence.get("algo").and_then(|v| v.as_str()) == Some(algo)
                {
                    return true;
                }
            }
        }
    }
    false
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

    // Both weak algorithms declared in fkb/php/common.yaml must be judged by name. The sample carries `md5`
    // (notify) and `sha1` (legacy); a regression that dropped either from `weak_algos` would otherwise pass —
    // the original test only ever used md5.
    assert!(
        has_weak_hash_algo(&b, "md5"),
        "md5 must be cited as a weak signature algorithm"
    );
    assert!(
        has_weak_hash_algo(&b, "sha1"),
        "sha1 must be cited as a weak signature algorithm (the second weak algo in the vocabulary)"
    );
    // Exactly two weak-hash findings: one md5, one sha1. Counting rules out a quiet over-flag.
    assert_eq!(
        annotation_count(&b, "weak_sign_hash"),
        2,
        "exactly the md5 and sha1 calls must be flagged as weak-hash, got: {kinds:?}"
    );
    // Exactly two loose comparisons: notify's `$sign == $calc` and legacy's `$calc == $sign`.
    assert_eq!(
        annotation_count(&b, "weak_sign_compare"),
        2,
        "exactly the two loose `==` comparisons must be flagged, got: {kinds:?}"
    );

    // The correct, constant-time form (`hash_equals`) must produce NO weak-hash finding: it is in `hash_calls`
    // but not in `weak_algos`. This pins the FKB comment's "hash_equals is the correct form, not a weak one" —
    // the property the original sample never exercised end-to-end.
    assert!(
        !has_weak_hash_algo(&b, "hash_equals"),
        "hash_equals is the correct form and must NOT be cited as a weak signature algorithm"
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
