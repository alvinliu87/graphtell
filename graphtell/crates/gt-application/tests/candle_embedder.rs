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
        assert_eq!(v.len(), dim, "输出维度必须等于 dim()");
        assert!((l2(&v) - 1.0).abs() < 1e-4, "向量必须 L2 归一化为单位长度，得到 {}", l2(&v));
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
        "单条 batch 必须等于单条 embed（doc 侧同前缀、同归一）"
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
        "query 侧带检索前缀，与 doc 侧编码必须不同"
    );
    // Both still unit-length and in the same space: they must correlate above the noise floor.
    let sim = cosine(&q, &d);
    assert!(sim > 0.0, "query/doc 同源文本应正相关，得到 {sim}");
}

/// An empty batch is a no-op, not a shape panic (the forward pass cannot take a `batch x 0` tensor).
#[test]
fn embed_batch_empty_is_empty() {
    let Some(emb) = embedder_or_skip() else { return };
    assert!(emb.embed_batch(&[]).is_empty(), "空 batch 必须返回空 vec");
}
