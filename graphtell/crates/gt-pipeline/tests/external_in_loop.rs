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

/// Build the graph and assert that exactly the in-loop external call was annotated.
fn assert_one_in_loop_call_flagged(tag: &str, root: &std::path::Path) {
    let Some(b) = common::graph_with_root(root, ProjectConfig::default()) else {
        panic!("the synthetic {tag} project graph build should succeed");
    };
    let kinds = annotation_kinds(&b);
    let n = count_ext_in_loop(&b);
    assert_eq!(
        n, 1,
        "{tag}：循环内的外部调用应恰好被标注 1 次（循环外的那次不应被标注），实际 {n} 次，全部注解：{kinds:?}"
    );
    let _ = std::fs::remove_dir_all(root);
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
