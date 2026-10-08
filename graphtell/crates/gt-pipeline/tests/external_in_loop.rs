//! End-to-end self-check of **P12 External** ("a remote call happens inside a loop").
//!
//! `external.rs`'s unit tests inject hand-written `CallRecord`s and a hand-written `external_calls` list,
//! so they prove the predicate but can neither see a real FKB nor see whether `in_loop` is still produced
//! by the real pipeline. What is pinned here instead:
//!
//! * the verb list really comes from the shipped FKB — `fkb/php/common.yaml` (`curl_exec`),
//!   `fkb/js/common.yaml` (`fetch`), `fkb/python/common.yaml` (`requests.get`), all three
//!   `apply_without_detection`, so no framework needs to be detected for them to take effect;
//! * the `in_loop` gate really holds end-to-end: each synthetic file contains **one external call inside a
//!   loop and one outside any loop**, and exactly one annotation must appear. A loop analysis that stopped
//!   setting `in_loop` (or started marking every call) shows up here, not as a silent "no annotations".
//!
//! No `@samples` needed: the phase is language-agnostic and the vocabulary is entirely FKB-declared, so a
//! synthetic tree reproduces the behaviour exactly (and runs on every `cargo test`, unlike a sample-gated test).

use gt_domain::model::ProjectConfig;
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

/// The annotation kind P12 writes (`external.rs::EXT_IN_LOOP`).
const EXT_IN_LOOP: &str = "ext-call-in-loop";

/// Every annotation kind present in the built graph (with duplicates: one entry per annotation).
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

fn count_ext_in_loop(b: &common::Built) -> usize {
    annotation_kinds(b)
        .iter()
        .filter(|k| *k == EXT_IN_LOOP)
        .count()
}

/// A synthetic project root: `files` are written relative to it (parent directories created).
fn synthetic_root(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-ext-{}-{}-{}",
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
        std::fs::write(&path, body).expect("write source");
    }
    dir
}

/// Build the graph and assert that exactly `expected` `ext-call-in-loop` annotations were produced.
fn assert_ext_in_loop_count(tag: &str, root: &std::path::Path, expected: usize) {
    let Some(b) = common::graph_with_root(root, ProjectConfig::default()) else {
        panic!("the synthetic {tag} project graph build should succeed");
    };
    let kinds = annotation_kinds(&b);
    let n = count_ext_in_loop(&b);
    assert_eq!(
        n, expected,
        "{tag}: expected {expected} `ext-call-in-loop` annotations, got {n}, all annotations: {kinds:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Build the graph and assert that exactly the in-loop external call was annotated (the canonical case proved by
/// the three language tests below: one external call inside a loop, one outside).
fn assert_one_in_loop_call_flagged(tag: &str, root: &std::path::Path) {
    assert_ext_in_loop_count(tag, root, 1);
}

#[test]
fn php_curl_exec_inside_foreach_is_flagged() {
    let root = synthetic_root(
        "php",
        &[
            ("composer.json", r#"{"require":{"php":">=7.4"}}"#),
            (
                "app/Sender.php",
                r#"<?php
namespace app;

class Sender
{
    public function sendAll($urls)
    {
        foreach ($urls as $url) {
            curl_exec($url);
        }
        curl_exec($urls[0]);
    }
}
"#,
            ),
        ],
    );
    assert_one_in_loop_call_flagged("php", &root);
}

#[test]
fn js_fetch_inside_for_of_is_flagged() {
    // `.js`, not `.ts`: `fkb/js/common.yaml` declares `language: javascript`.
    let root = synthetic_root(
        "js",
        &[
            ("package.json", r#"{"name":"demo","dependencies":{}}"#),
            (
                "src/api.js",
                r#"async function loadAll(urls) {
  for (const url of urls) {
    await fetch(url);
  }
  await fetch(urls[0]);
}

module.exports = { loadAll };
"#,
            ),
        ],
    );
    assert_one_in_loop_call_flagged("js", &root);
}

#[test]
fn python_requests_get_inside_for_is_flagged() {
    let root = synthetic_root(
        "python",
        &[
            ("requirements.txt", "requests>=2.0\n"),
            (
                "app.py",
                r#"import requests


def load_all(urls):
    for url in urls:
        requests.get(url)
    requests.get(urls[0])
"#,
            ),
        ],
    );
    assert_one_in_loop_call_flagged("python", &root);
}

/// The verb list really gates the annotation: a call whose name is **not** an FKB-declared external verb must not
/// be flagged even though it sits inside a loop. Every other test uses the *same* FKB verb for both the in-loop and
/// the outside call, so it can only prove the loop gate — a verb list that became (wrongly) over-broad would still
/// pass `count == 1`. `console.log` is a plain builtin, never a remote call, so this pins the other half of the
/// predicate (`is_external_call`) end-to-end: zero annotations.
#[test]
fn js_non_external_call_in_loop_not_flagged() {
    let root = synthetic_root(
        "js-nonverb",
        &[
            ("package.json", r#"{"name":"demo","dependencies":{}}"#),
            (
                "src/api.js",
                r#"function logAll(urls) {
  for (const url of urls) {
    console.log(url);
  }
}
"#,
            ),
        ],
    );
    assert_ext_in_loop_count("js-nonverb", &root, 0);
}

/// Two external calls inside the same loop must **both** be flagged — `count == 2`, not just the first. The
/// `count == 1` canonical tests would pass even if the loop analysis only annotated the first matching call, so
/// this pins that every in-loop external call site is annotated independently.
#[test]
fn php_multiple_external_calls_in_loop_all_flagged() {
    let root = synthetic_root(
        "php-multi",
        &[
            ("composer.json", r#"{"require":{"php":">=7.4"}}"#),
            (
                "app/Sender.php",
                r#"<?php
namespace app;

class Sender
{
    public function sendAll($urls)
    {
        foreach ($urls as $url) {
            curl_exec($url);
            curl_exec($url . "&x=1");
        }
        curl_exec($urls[0]);
    }
}
"#,
            ),
        ],
    );
    assert_ext_in_loop_count("php-multi", &root, 2);
}

/// An external call inside a loop that is itself inside another loop must still be flagged: `in_loop` must hold for
/// nested loops, not only for a call directly under a single loop header. The canonical tests only exercise one
/// loop level, so a regression that set `in_loop` only for immediate loop children would slip through.
#[test]
fn python_nested_loop_external_call_flagged() {
    let root = synthetic_root(
        "python-nested",
        &[
            ("requirements.txt", "requests>=2.0\n"),
            (
                "app.py",
                r#"import requests


def load_all(groups):
    for group in groups:
        for url in group:
            requests.get(url)
    requests.get(group[0][0])
"#,
            ),
        ],
    );
    assert_ext_in_loop_count("python-nested", &root, 1);
}
