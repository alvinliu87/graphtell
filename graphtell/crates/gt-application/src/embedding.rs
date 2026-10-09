//! Offline text encoder (the "encoding" side of vector recall).
//!
//! The current default [`LocalHashingEmbedder`] is **zero-dependency, fully offline**: it uses feature hashing
//! (hashing trick) to project text into a fixed dimension then L2-normalizes. It is **not** a neural-network semantic vector,
//! but it runs offline in this repo and provides "soft matching" (cross-field, partial overlap still yields a cosine score), enough to
//! demonstrate the full pipeline of "vector seeds + graph expansion".
//!
//! A real semantic model (`bge-m3` / `unixcoder`, local `candle` / `ort` inference) only needs to implement
//! the [`Embedder`] trait to drop in seamlessly; the recall flow needs no changes.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

/// Text → dense-vector encoder. Replaceable, offline-capable.
pub trait Embedder: Send + Sync {
    /// Encode a piece of text into a vector (already L2-normalized).
    fn embed(&self, text: &str) -> Vec<f32>;
    /// Vector dimension.
    fn dim(&self) -> usize;
    /// Encode "query" text (used on the user-query side during recall).
    ///
    /// Defaults to the same as [`Embedder::embed`]; but some models (e.g. the bge family) require a retrieval prefix on the **query side**
    /// and none on the document side, in which case override this method so query and document land in the same vector space.
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.embed(text)
    }

    /// Batch encode (document side, no prefix).
    ///
    /// Defaults to calling [`Embedder::embed`] item by item; a real model (bge-m3 / candle) should override as a "single forward pass"
    /// to cut cold-start from "N forwards per node" to "a few large-batch forwards" — an order-of-magnitude speedup for large-graph recall
    /// (bge-m3 on CPU is ~hundreds of ms per single forward, batch forward amortizes to a few ms/item).
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// Cosine similarity of two vectors (recomputes each L2 norm to avoid pre-normalization float drift).
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..n {
        let x = a[i] as f64;
        let y = b[i] as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        let c = dot / (na.sqrt() * nb.sqrt());
        if c > 1.0 {
            1.0
        } else if c < -1.0 {
            -1.0
        } else {
            c
        }
    }
}

/// Zero-dependency local encoder: feature hashing (signed hash → ±1 projected to a fixed dimension).
pub struct LocalHashingEmbedder {
    dim: usize,
}

impl LocalHashingEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }
}

impl Embedder for LocalHashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for feat in text_features(text) {
            let h = signed_hash(&feat);
            let idx = (h.unsigned_abs() as usize) % self.dim;
            v[idx] += if h < 0 { -1.0 } else { 1.0 };
        }
        l2_normalize(&mut v);
        v
    }
}

/// Default offline encoder (256-dim, zero external dependencies).
pub fn default_embedder() -> Arc<dyn Embedder> {
    Arc::new(LocalHashingEmbedder::new(256))
}

/// Human-readable description of the currently active embedding backend (for the status endpoint / UI).
static BACKEND_INFO: OnceLock<String> = OnceLock::new();
/// The currently active embedding vector dimension.
static BACKEND_DIM: OnceLock<usize> = OnceLock::new();

fn set_backend_info(name: &str, dim: usize) {
    let _ = BACKEND_INFO.set(name.to_string());
    let _ = BACKEND_DIM.set(dim);
}

/// Human-readable description of the current embedding backend (e.g. `bge-m3-local`, `remote-openai (http://...)`).
pub fn embedding_backend_info() -> String {
    BACKEND_INFO.get().cloned().unwrap_or_else(|| "unknown".to_string())
}

/// The current embedding vector dimension; 0 when unknown.
pub fn embedding_dim() -> usize {
    BACKEND_DIM.get().copied().unwrap_or(0)
}

/// Resolve the encoder used for recall: choose the backend per `GT_EMBEDDING_BACKEND`.
///
/// - `auto` (default): local bge-m3 preferred, safely falls back to offline lexical hashing when weights are missing;
/// - `local`: force local bge-m3, no fallback when missing (errors explicitly, hinting to run `graphtell model fetch` first);
/// - `url` / `remote`: point at the user's own embedding service (OpenAI-compatible / TEI native);
/// - `hash` / `off` / `none`: pure offline lexical hashing (zero-dependency, weaker quality but a usable floor).
///
/// The return value injects directly into [`crate::RecallService`]. Production entry points (CLI / HTTP router) all go through it,
/// so "real semantics if weights exist, fall back to offline otherwise" is uniform behavior, callers need not care.
pub fn resolve_recall_embedder() -> Arc<dyn Embedder> {
    let backend = std::env::var("GT_EMBEDDING_BACKEND").unwrap_or_else(|_| "auto".to_string());
    match backend.as_str() {
        "hash" | "off" | "none" => {
            set_backend_info("hash (offline lexical, 256d)", 256);
            return Arc::new(LocalHashingEmbedder::new(256));
        }
        "url" | "remote" => {
            return match crate::embed_remote::RemoteHttpEmbedder::load() {
                Ok(e) => {
                    let dim = e.dim();
                    let fmt = match std::env::var("GT_EMBEDDING_FORMAT").as_deref() {
                        Ok("tei") => "tei",
                        _ => "openai",
                    };
                    let url = std::env::var("GT_EMBEDDING_URL").unwrap_or_default();
                    set_backend_info(&format!("remote-{fmt} ({url})"), dim);
                    Arc::new(e)
                }
                Err(err) => {
                    tracing::warn!("remote embedding load failed ({err}); falling back to the offline lexical encoder");
                    set_backend_info("hash (remote failed)", 256);
                    Arc::new(LocalHashingEmbedder::new(256))
                }
            };
        }
        "local" => {
            // Force local bge-m3: no fall back to lexical hashing when missing, so the user explicitly notices the missing weights.
            if let Some(e) = try_real_recall_embedder() {
                let dim = e.dim();
                set_backend_info("bge-m3-local", dim);
                return e;
            }
            tracing::error!(
                "GT_EMBEDDING_BACKEND=local but the local bge-m3 weights are missing; run `graphtell model fetch` first"
            );
            set_backend_info("hash (local missing)", 256);
            return Arc::new(LocalHashingEmbedder::new(256));
        }
        "auto" | _ => {}
    }

    // ---- auto: local bge-m3 preferred, otherwise fall back to offline lexical (default behavior) ----
    #[cfg(all(feature = "model-candle", feature = "model-ort"))]
    {
        let dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        let onnx = std::env::var("GT_BGE_ONNX")
            .unwrap_or_else(|_| "models/bge-m3-onnx/model.onnx".to_string());
        if std::path::Path::new(&onnx).exists() {
            let tok = format!("{dir}/tokenizer.json");
            match crate::embed_ort::HybridBgeEmbedder::load(&dir, &onnx, &tok) {
                Ok(embedder) => {
                    tracing::info!("loaded the hybrid bge-m3 encoder (query tract / batch candle, {onnx})");
                    set_backend_info("bge-m3-local (hybrid tract+candle)", embedder.dim());
                    return Arc::new(embedder);
                }
                Err(err) => tracing::warn!("hybrid encoder load failed ({onnx}); falling back to candle: {err}"),
            }
        }
    }
    #[cfg(feature = "model-candle")]
    {
        let dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        match crate::embed_model::CandleBgeEmbedder::load(&dir) {
            Ok(embedder) => {
                tracing::info!("loaded the real bge-m3 semantic encoder ({dir})");
                set_backend_info("bge-m3-local (candle)", embedder.dim());
                return Arc::new(embedder);
            }
            Err(err) => {
                tracing::warn!("bge-m3 model load failed ({dir}); falling back to the local hash encoder: {err}");
            }
        }
    }
    set_backend_info("hash (offline fallback, 256d)", 256);
    default_embedder()
}

/// Return the real bge-m3 encoder only when `model-candle` is compiled and `GT_BGE_MODEL` weights are available,
/// otherwise return `None` (the caller should fall back to lexical / fast-vector path and disable background warmup).
///
/// Differs from [`resolve_recall_embedder`]: the latter **safely falls back** to the default hash encoder when weights are missing;
/// this function explicitly hands the fact "do we have real semantics" back to the caller, so they can decide whether to trigger background warmup.
pub fn try_real_recall_embedder() -> Option<Arc<dyn Embedder>> {
    // Same as [`resolve_recall_embedder`]: prefer the hybrid encoder (query tract / batch candle).
    #[cfg(all(feature = "model-candle", feature = "model-ort"))]
    {
        let dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        let onnx = std::env::var("GT_BGE_ONNX")
            .unwrap_or_else(|_| "models/bge-m3-onnx/model.onnx".to_string());
        if std::path::Path::new(&onnx).exists() {
            let tok = format!("{dir}/tokenizer.json");
            match crate::embed_ort::HybridBgeEmbedder::load(&dir, &onnx, &tok) {
                Ok(embedder) => {
                    tracing::info!("loaded the hybrid bge-m3 encoder (query tract / batch candle, {onnx})");
                    return Some(Arc::new(embedder));
                }
                Err(err) => tracing::warn!("hybrid encoder load failed ({onnx}); falling back to candle: {err}"),
            }
        }
    }
    #[cfg(feature = "model-candle")]
    {
        let dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        match crate::embed_model::CandleBgeEmbedder::load(&dir) {
            Ok(embedder) => {
                tracing::info!("loaded the real bge-m3 semantic encoder ({dir})");
                Some(Arc::new(embedder))
            }
            Err(err) => {
                tracing::warn!("bge-m3 model load failed ({dir}); no semantic encoder: {err}");
                None
            }
        }
    }
    #[cfg(not(feature = "model-candle"))]
    {
        tracing::info!("model-candle not compiled; no semantic encoder (lexical / fast vector path only)");
        None
    }
}

/// Persisted backend-mode selection (written by the HTTP API so a mode switch survives restart).
///
/// `mode` is one of `auto` / `local` / `url` / `hash`; `url` is the remote embedding endpoint for `url` mode.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelBackendConfig {
    pub mode: String,
    pub url: Option<String>,
}

/// Build the semantic encoder for an **explicit** backend mode (used by the HTTP API to hot-swap the
/// encoder without restarting the server). Returns `(semantic_embedder, backend_info, dim)`.
///
/// `semantic_embedder` is `None` whenever the mode resolves to the offline hash encoder (the call sites
/// then fall back to the always-available `fast_embedder`).
pub fn resolve_backend(mode: &str, url: Option<String>) -> (Option<Arc<dyn Embedder>>, String, usize) {
    match mode {
        "hash" | "off" | "none" => {
            set_backend_info("hash (offline lexical, 256d)", 256);
            (None, "hash (offline lexical, 256d)".to_string(), 256)
        }
        "url" | "remote" => match crate::embed_remote::RemoteHttpEmbedder::load_with(url.clone()) {
            Ok(e) => {
                let dim = e.dim();
                let fmt = std::env::var("GT_EMBEDDING_FORMAT").unwrap_or_else(|_| "openai".to_string());
                let u = url.unwrap_or_default();
                set_backend_info(&format!("remote-{fmt} ({u})"), dim);
                (Some(Arc::new(e)), format!("remote-{fmt} ({u})"), dim)
            }
            Err(err) => {
                tracing::warn!("remote embedder failed to load: {err}; falling back to hash");
                set_backend_info("hash (remote failed)", 256);
                (None, "hash (remote failed)".to_string(), 256)
            }
        },
        "local" => match try_real_recall_embedder() {
            Some(e) => {
                let dim = e.dim();
                set_backend_info("bge-m3-local", dim);
                (Some(e), "bge-m3-local".to_string(), dim)
            }
            None => {
                set_backend_info("hash (local missing)", 256);
                (None, "hash (local missing)".to_string(), 256)
            }
        },
        // "auto": mirror `resolve_recall_embedder` but only return the semantic part when real weights exist.
        _ => match try_real_recall_embedder() {
            Some(e) => {
                let dim = e.dim();
                set_backend_info("bge-m3-local (auto)", dim);
                (Some(e), "bge-m3-local (auto)".to_string(), dim)
            }
            None => {
                set_backend_info("hash (offline fallback, 256d)", 256);
                (None, "hash (offline fallback, 256d)".to_string(), 256)
            }
        },
    }
}

/// Extract features for encoding: ASCII tokens (preserve case, split camelCase/snake then lowercase) +
/// CJK chars + CJK bigrams.
fn text_features(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // ASCII / numeric tokens: first split camelCase with original case (OrderService → Order+Service),
    // then lowercase, to avoid losing the case boundary by lowercasing too early.
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let raw = raw.trim_matches('_');
        if raw.chars().count() < 2 {
            continue;
        }
        let low = raw.to_lowercase();
        out.push(low.clone());
        for sub in split_identifier(raw) {
            let sub = sub.to_lowercase();
            if sub.chars().count() >= 2 && !out.iter().any(|x| x == &sub) {
                out.push(sub);
            }
        }
    }
    // CJK chars + bigrams
    let chars: Vec<char> = text.chars().filter(|c| is_cjk(*c)).collect();
    for c in &chars {
        out.push(c.to_string());
    }
    for w in chars.windows(2) {
        out.push(w.iter().collect());
    }
    out
}

/// Split `placeOrder` / `applyDiscount` / `unused_log` into subwords.
fn split_identifier(tok: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in tok.chars() {
        if ch == '_' || ch == '-' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
        prev_lower = ch.is_lowercase();
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

/// FNV-1a 64-bit, take the top bit for sign, return a signed hash.
fn signed_hash(s: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let magnitude = (h & 0x7fff_ffff_ffff_ffff) as i64;
    if (h >> 63) & 1 == 1 {
        -magnitude
    } else {
        magnitude
    }
}

fn l2_normalize(v: &mut [f32]) {
    let n: f64 = v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x = (*x as f64 / n) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_returns_normalized_fixed_dim() {
        let e = LocalHashingEmbedder::new(128);
        let v = e.embed("placeOrder discount coupon");
        assert_eq!(v.len(), 128);
        let norm: f64 = v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "it must be L2-normalised, got norm {norm}");
    }

    #[test]
    fn identical_text_cosine_is_one() {
        let e = LocalHashingEmbedder::new(256);
        let a = e.embed("下单改优惠 order discount");
        let b = e.embed("下单改优惠 order discount");
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn shared_token_beats_disjoint() {
        let e = LocalHashingEmbedder::new(1024);
        // query has order / discount / coupon; related shares all three, unrelated touches none of them.
        let q = e.embed("order discount coupon place 下单改优惠");
        let related = e.embed("class OrderService applyDiscount vipCoupon");
        let unrelated = e.embed("unused_log config cache session");
        assert!(
            cosine(&q, &related) > cosine(&q, &unrelated),
            "nodes sharing order/discount/coupon must be closer than unrelated ones"
        );
    }
}
