//! Batch scoring of the recall evaluation corpus.
//!
//! Treats `tests/eval/*.jsonl`'s "query -> expected hits -> minimum quality tier" as a regression corpus and runs
//! recall **against the real sample_project sample** to validate it. Purpose:
//!
//! 1. Pin the recall baseline for 6 kinds of real development scenarios (listeners surfacing / business methods
//!    matching / quality not collapsing);
//! 2. Later, when the recall algorithm changes or a bge semantic path is added, batch-compare whether "quality tier
//!    + hit nodes" has regressed.
//!
//! **Depends on a real sample that is too large to commit (not in the repo) + the bge-m3 model weights**:
//! with no sample it skips gracefully (rather than failing); with a sample but no model weights the quality tier
//! will not be met, so `eval_recall_scenarios` stays `#[ignore]` (the reason is written above it).
//! To force a run: `cargo test -p gt-app -- --ignored eval_recall`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, RecallQuality, RecallQuery, RecallService};
use gt_domain::model::NewProject;
use gt_domain::port::{NoopObserver, Persistence, SystemClock};
use gt_sample_support::sample_root;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}



struct Built {
    container: Container,
    project_id: gt_domain::model::ProjectId,
}

/// Run one complete graph build and cache it (runs only once per test binary).
fn built() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let sample = sample_root()?;
            let data_dir =
                std::env::temp_dir().join(format!("graphtell-evaltest-{}", std::process::id()));
            std::fs::create_dir_all(&data_dir).ok()?;

            let config = AppConfig {
                data_dir,
                fkb_dir: Some(workspace_root().join("fkb")),
                views_dir: Some(workspace_root().join("views")),
                rules_dir: Some(workspace_root().join("rules")),
                bind: "127.0.0.1".into(),
                port: 0,
                ui_dir: None,
                };
            let container = Container::new(config).expect("container assembly should not fail");

            let projects =
                ProjectService::new(container.store.clone() as Arc<dyn Persistence>, Arc::new(SystemClock));
            let pipeline = PipelineService::new(
                container.store.clone() as Arc<dyn Persistence>,
                Arc::clone(&container.deps),
                Arc::clone(&container.rules) as Arc<dyn gt_domain::port::RuleProvider>,
            );

            let project = projects
                .create(NewProject {
                    name: "sample_project".into(),
                    root_path: sample,
                    description: None,
                    config: None,
                })
                .expect("creating the project should not fail");

            pipeline
                .run(project.id, &NoopObserver)
                .expect("graph build on the sample_project sample should not fail");

            Some(Arc::new(Built {
                container,
                project_id: project.id,
            }))
        })
        .clone()
}


/// Parse a quality-tier string into a comparable ordinal.
fn quality_ord(q: &RecallQuality) -> u8 {
    match q {
        RecallQuality::Low => 0,
        RecallQuality::Medium => 1,
        RecallQuality::High => 2,
    }
}

fn threshold_ord(s: &str) -> u8 {
    match s {
        "low" => 0,
        "high" => 2,
        _ => 1, // default medium
    }
}

/// Whether the production semantic embedder (bge-m3 weights) is available. Under the hash fallback the
/// recall *logic* is identical, only the relevance ranking is weaker, so structural / output-shape
/// assertions run everywhere, while quality-tier and a couple of encoder-dependent hit expectations
/// are only enforced when real weights are present.
fn real_embedder() -> bool {
    std::env::var("GT_BGE_MODEL").is_ok()
}

/// Run the whole `tests/eval/recall_scenarios.jsonl` corpus, validating "expected hits + minimum quality tier" case by case.
// This is a **recall quality evaluation**: the expected "minimum quality tier + hit nodes" is calibrated against
// bge-m3 semantic vectors. A `--no-default-features` build has no model weights and takes the hash fallback, where
// 2 of the 6 corpus cases measurably fail (e.g. `order_create_notify` at Low quality, `login_log` missing
// loginSaveVisit) — that is a difference in encoder capability, not broken recall logic. So it additionally
// **runs always** as a pipeline smoke + structural check (no `#[ignore]`); the quality tier and the
// encoder-sensitive expected hits are enforced only when `GT_BGE_MODEL` weights are present.
#[test]
fn eval_recall_scenarios() {
    let Some(b) = built() else {
        eprintln!("skip: sample_project corpus not available (set GRAPHTELL_SAMPLE_DIR)");
        return;
    };
    let weighted = real_embedder();

    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/eval/recall_scenarios.jsonl");
    let text = match std::fs::read_to_string(&corpus) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skip: corpus file {corpus:?} read failed: {e}");
            return;
        }
    };

    let svc = RecallService::new(
        b.container.store.clone() as Arc<dyn Persistence>,
        b.container.filesystem(),
        b.container.scanner(),
    );

    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;

    for (i, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("line {} corpus JSON parse failed: {e}", i + 1));
                continue;
            }
        };
        let id = v["id"].as_str().unwrap_or("?");
        let query = match v["query"].as_str() {
            Some(q) => q.to_string(),
            None => {
                failures.push(format!("[{id}] missing the query field"));
                continue;
            }
        };
        let expect: Vec<String> = v["expect_contains"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let min_q = v["expect_quality_at_least"].as_str().unwrap_or("medium");
        let note = v["note"].as_str().unwrap_or("");

        let result = match svc.recall(
            b.project_id,
            &RecallQuery {
                query: query.clone(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        ) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("[{id}] recall failed: {e}"));
                continue;
            }
        };
        ran += 1;

        // Always: the pipeline must return results (a zero-hit recall for a real query is a breakage
        // regardless of encoder).
        assert!(
            !result.hits.is_empty(),
            "[{id}] recall returned zero hits for `{query}`"
        );

        // Expected hit: a substring match on the name or the fqn counts (the fqn contains the namespace and class name, so it matches more reliably).
        let mut missing = Vec::new();
        for exp in &expect {
            let hit = result
                .hits
                .iter()
                .any(|h| h.name.contains(exp) || h.fqn.as_deref().map_or(false, |f| f.contains(exp)));
            if !hit {
                missing.push(exp.clone());
            }
        }

        // Quality tier: the ordinal of result.quality must be >= the expected threshold.
        let quality_ok = quality_ord(&result.quality) >= threshold_ord(min_q);

        // A one-line visual summary (so a weighted run shows each case's quality and what is missing).
        let top: Vec<&str> = result
            .hits
            .iter()
            .take(4)
            .map(|h| h.name.as_str())
            .collect();
        eprintln!(
            "[{id}] quality={:?} top={:?} missing={:?} expect={:?}",
            result.quality, top, missing, expect
        );
        if !note.is_empty() {
            eprintln!("      note: {note}");
        }

        // Only enforce the encoder-sensitive assertions when real weights are present; under the hash
        // fallback they would fail for encoder-capability reasons, not recall-logic bugs.
        if weighted {
            if !missing.is_empty() {
                failures.push(format!(
                    "[{id}] query=`{query}` expected hit missing: {:?} (actual top: {:?})",
                    missing, top
                ));
            }
            if !quality_ok {
                failures.push(format!(
                    "[{id}] query=`{query}` quality tier {:?} below expected `{}`",
                    result.quality, min_q
                ));
            }
        } else if !missing.is_empty() {
            eprintln!(
                "      (hash fallback) skipping encoder-sensitive expected-hit check, missing={:?}",
                missing
            );
        }
    }

    if ran == 0 {
        eprintln!("warn: corpus contains no executable query lines");
    }
    if !failures.is_empty() {
        panic!(
            "eval corpus has {} failures (of {ran} executed):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

/// Pins the `with_snippets` / `include_body` output-shape toggles of `RecallQuery` — branches the corpus loop
/// never touches (it always sends `false`/`false`). A regression that drops snippet population or the
/// `## Full files (include_body)` context-pack section would otherwise ship silently; the IDE / MCP consumer
/// that reads `hits[].snippet` / the markdown body would get empty strings. This is an **output-shape** check
/// (encoder-independent), so it always runs; it only needs the sample corpus to be present.
#[test]
fn recall_snippet_and_body_toggles_are_respected() {
    let Some(b) = built() else {
        eprintln!("skip: sample_project corpus not available (set GRAPHTELL_SAMPLE_DIR)");
        return;
    };
    let svc = RecallService::new(
        b.container.store.clone() as Arc<dyn Persistence>,
        b.container.filesystem(),
        b.container.scanner(),
    );
    let base = RecallQuery {
        query: "支付回调怎么处理".into(),
        limit: 20,
        hops: 2,
        kinds: Vec::new(),
        with_snippets: false,
        include_body: false,
    };

    // OFF: no hit should carry a populated snippet, and the markdown must NOT contain the body section.
    let off = svc.recall(b.project_id, &base).expect("recall off");
    assert!(
        off.hits.iter().all(|h| h.snippet.as_deref().unwrap_or("").is_empty()),
        "with_snippets=false must not populate hit snippets: {:?}",
        off.hits.iter().filter_map(|h| h.snippet.clone()).collect::<Vec<_>>()
    );
    assert!(
        !off.markdown.contains("## Full files (include_body)"),
        "include_body=false must not emit the body section in markdown"
    );

    // ON: at least one hit must carry a non-empty snippet, and the markdown must carry the body section.
    let on = svc
        .recall(
            b.project_id,
            &RecallQuery {
                with_snippets: true,
                include_body: true,
                ..base
            },
        )
        .expect("recall on");
    assert!(
        on.hits
            .iter()
            .any(|h| h.snippet.as_deref().map_or(false, |s| !s.is_empty())),
        "with_snippets=true must populate at least one hit snippet; hits={:?}",
        on.hits.len()
    );
    assert!(
        on.markdown.contains("## Full files (include_body)"),
        "include_body=true must emit the `## Full files (include_body)` section in markdown"
    );
}

/// Pins the `kinds` filter branch of `RecallQuery`: it must narrow recall to the requested kind's neighborhood
/// (surface that kind, and never introduce a kind absent from the unfiltered result) — not broaden it into a
/// wider mix. The corpus loop always sends `kinds: []` (no filter), so this path is otherwise unguarded.
/// Self-adapting: it derives the target kind from a real hit of an unfiltered recall, so it does not hard-code a
/// kind name that may be absent from a given sample. This is an **output-shape** check (encoder-independent), so it
/// always runs; it only needs the sample corpus to be present.
#[test]
fn recall_kinds_filter_restricts_results() {
    let Some(b) = built() else {
        eprintln!("skip: sample_project corpus not available (set GRAPHTELL_SAMPLE_DIR)");
        return;
    };
    let svc = RecallService::new(
        b.container.store.clone() as Arc<dyn Persistence>,
        b.container.filesystem(),
        b.container.scanner(),
    );

    // First, an unfiltered recall to learn a kind that actually appears in the results.
    let unfiltered = svc
        .recall(
            b.project_id,
            &RecallQuery {
                query: "支付回调怎么处理".into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("recall unfiltered");
    let Some(target) = unfiltered.hits.first().map(|h| h.kind.clone()) else {
        eprintln!("skip: unfiltered recall returned no hits on this sample");
        return;
    };

    let filtered = svc
        .recall(
            b.project_id,
            &RecallQuery {
                query: "支付回调怎么处理".into(),
                limit: 20,
                hops: 2,
                kinds: vec![target.clone()],
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("recall filtered");
    assert!(
        !filtered.hits.is_empty(),
        "filtering by an existing kind `{target}` must still return hits"
    );
    // The kinds filter narrows recall: it must (a) surface the requested kind, and (b) never introduce a
    // kind that was not already reachable unfiltered. Expansion along chain edges legitimately keeps
    // structural containers of the requested kind (e.g. the containing `Class`/`Namespace`), so we do
    // NOT assert every hit equals `target` — only that the result is a restrained subset, not a broader mix.
    let unfiltered_kinds: std::collections::HashSet<&str> =
        unfiltered.hits.iter().map(|h| h.kind.as_str()).collect();
    let filtered_kinds: std::collections::HashSet<&str> =
        filtered.hits.iter().map(|h| h.kind.as_str()).collect();
    assert!(
        filtered_kinds.contains(target.as_str()),
        "kinds filter must still surface the requested kind `{target}`"
    );
    assert!(
        filtered_kinds.is_subset(&unfiltered_kinds),
        "kinds filter must not introduce kinds absent from the unfiltered result; new kinds={:?}",
        filtered_kinds.difference(&unfiltered_kinds).collect::<Vec<_>>()
    );
    if unfiltered_kinds.len() > 1 {
        assert!(
            !unfiltered_kinds.is_subset(&filtered_kinds),
            "kinds filter must drop at least one kind present in the unfiltered result"
        );
    }
}
