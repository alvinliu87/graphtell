//! Feature-gated integration test for the real `candle` bge-m3 encoder (`CandleBgeEmbedder`).
//!
//! Compiled only with `--features model-candle`. When the safetensors weights are absent it **skips**
//! (prints a note and returns) instead of failing, so an offline `cargo test --features model-candle`
//! still goes green — you only get real coverage after pulling weights (see `embed_model.rs`).
//!
//! This file deliberately covers the *structural* invariants the doc comments promise (output dimension,
//! L2 normalisation, `embed_batch` == single `embed`, query/doc differ by prefix, empty batch); the
//! semantic-recall correctness (Chinese intent -> English business nodes) lives in `recall_service.rs`.

#![cfg(feature = "model-candle")]

use std::path::{Path, PathBuf};

use gt_application::embed_model::CandleBgeEmbedder;
use gt_application::embedding::cosine;
use gt_application::Embedder;

/// Resolve the weights dir (`GT_BGE_MODEL` or `models/bge-m3-safetensors` next to the workspace) and load.
/// Returns `None` (and prints a skip note) when the weights are not present on this machine.
fn embedder_or_skip() -> Option<CandleBgeEmbedder> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
    let model_dir = std::env::var("GT_BGE_MODEL")
        .unwrap_or_else(|_| root.join("bge-m3-safetensors").to_string_lossy().into());
    if !Path::new(&model_dir).join("model.safetensors").exists() {
        eprintln!(
            "skip candle_embedder test: {model_dir}/model.safetensors not found \
             (run tools/convert_bge_safetensors.py on a networked machine first)"
        );
        return None;
    }
    Some(CandleBgeEmbedder::load(&model_dir).expect("load bge-m3 safetensors"))
}

fn all_close(a: &[f32], b: &[f32], tol: f32) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

fn l2(v: &[f32]) -> f64 {
    v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt()
}

/// Every encoder output (`embed` / `embed_query` / `embed_batch` rows) has length `dim()` and is L2-normalised
/// to unit length — the recall layer assumes unit vectors so it can compare via raw dot / cosine cheaply.
#[test]
fn outputs_are_unit_length_and_match_dim() {
    let Some(emb) = embedder_or_skip() else { return };
    let dim = emb.dim();

    for v in [
        emb.embed("placeOrder"),
        emb.embed("apply discount to cart"),
        emb.embed_query("apply discount"),
    ] {
        assert_eq!(v.len(), dim, "the output dimension must equal dim()");
        assert!((l2(&v) - 1.0).abs() < 1e-4, "the vector must be L2-normalised to unit length, got {}", l2(&v));
    }

    let batch = emb.embed_batch(&[
        "placeOrder".to_string(),
        "apply discount to cart".to_string(),
    ]);
    assert_eq!(batch.len(), 2);
    for v in &batch {
        assert_eq!(v.len(), dim);
        assert!((l2(v) - 1.0).abs() < 1e-4);
    }
}

/// `embed_batch([t])` must equal `embed(t)`: the batch path is just the single path with right-padding, so a
/// one-item batch cannot drift (the doc claims batch is an order of magnitude faster than per-item `embed`,
/// which is only safe if the result is identical). This pins that contract against padding/shape bugs.
#[test]
fn embed_batch_single_item_matches_embed() {
    let Some(emb) = embedder_or_skip() else { return };
    let text = "applyDiscount".to_string();

    let single = emb.embed(&text);
    let batched = emb.embed_batch(&[text.clone()]);
    assert_eq!(batched.len(), 1);
    assert!(
        all_close(&single, &batched[0], 1e-3),
        "a batch of one must equal a single embed (same prefix and normalisation on the doc side)"
    );
}

/// The query side prepends the bge retrieval prefix while the document side does not, so the *same* text encodes
/// to two different vectors that nonetheless live in the same space. This is the whole point of the prefix: a
/// bare `embed_query`/`embed` equality would mean the prefix was dropped (silently breaking semantic recall).
#[test]
fn query_and_doc_encodings_differ_for_same_text() {
    let Some(emb) = embedder_or_skip() else { return };
    let text = "apply discount";

    let q = emb.embed_query(text);
    let d = emb.embed(text);
    assert_ne!(q.len(), 0);
    assert!(
        !all_close(&q, &d, 1e-2),
        "the query side carries a retrieval prefix, so its encoding must differ from the doc side"
    );
    // Both still unit-length and in the same space: they must correlate above the noise floor.
    let sim = cosine(&q, &d);
    assert!(sim > 0.0, "query/doc of the same source text must correlate positively, got {sim}");
}

/// An empty batch is a no-op, not a shape panic (the forward pass cannot take a `batch x 0` tensor).
#[test]
fn embed_batch_empty_is_empty() {
    let Some(emb) = embedder_or_skip() else { return };
    assert!(emb.embed_batch(&[]).is_empty(), "an empty batch must return an empty vec");
}

/// `embed_batch` for N>1 must preserve input order, and every row must equal the per-item `embed` of the *same*
/// input. The doc claims batch is ~10x faster than per-item `embed` "only safe if the result is identical"; the
/// existing `embed_batch_single_item_matches_embed` only pins N=1. A transpose/padding bug in the multi-row path
/// would reorder or mutate rows and silently corrupt recall (each result is associated back to its input by index).
#[test]
fn embed_batch_preserves_order_and_matches_individuals() {
    let Some(emb) = embedder_or_skip() else { return };
    let inputs = [
        "placeOrder".to_string(),
        "apply discount to cart".to_string(),
        "shipping method selection".to_string(),
        "refund the order".to_string(),
    ];
    let batch = emb.embed_batch(&inputs);
    assert_eq!(batch.len(), inputs.len(), "batch length must equal input length");
    for (i, t) in inputs.iter().enumerate() {
        assert!(
            all_close(&batch[i], &emb.embed(t), 1e-3),
            "batch row {i} must equal the per-item embed of the same input (order/padding bug): \
             batch[{i}] vs embed({t:?})"
        );
    }
}

/// Encoding is deterministic: the same text encoded twice yields the same vector. A nondeterministic embedder
/// (dropout left on, unseeded op) would make recall scores jitter between cold-start and refresh, breaking the
/// "candidate set is stable" assumption the snapshot/recall tests rely on.
#[test]
fn embed_is_deterministic() {
    let Some(emb) = embedder_or_skip() else { return };
    let a = emb.embed("apply discount to cart");
    let b = emb.embed("apply discount to cart");
    assert!(
        all_close(&a, &b, 1e-4),
        "encoding the same text twice must be stable (no dropout / nondeterministic op)"
    );
    let q1 = emb.embed_query("refund the order");
    let q2 = emb.embed_query("refund the order");
    assert!(all_close(&q1, &q2, 1e-4), "embed_query must also be deterministic");
}

/// `dim()` must report bge-m3's known embedding dimension (1024). This pins the *model identity*: if the wrong
/// safetensors is loaded, or the pooling/head is mis-wired, the dimension changes and every stored recall vector
/// becomes incompatible — a silent, whole-layer regression for recall.
#[test]
fn dim_is_bge_m3_1024() {
    let Some(emb) = embedder_or_skip() else { return };
    assert_eq!(emb.dim(), 1024, "bge-m3 must produce 1024-dim embeddings");
}
