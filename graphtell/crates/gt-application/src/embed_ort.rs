//! Real neural-network embedding adapter (**feature-gated, not compiled by default**).
//!
//! Uses the pure-Rust inference engine `tract` to load the exported `bge-m3` ONNX weights and produce genuinely cross-language semantic vectors.
//! `tract` is chosen because it has zero system dependencies (no onnxruntime openssl), so offline builds / tests stay green.
//!
//! # Why tract only handles "queries" while batch encoding still goes through candle
//!
//! Measured locally (16-core AMD, release):
//!
//! | path | candle | tract |
//! |---|---|---|
//! | single forward (query, seq≈24) | ~770 ms | **~161 ms (~5× faster)** |
//! | batch encoding (nodes, batch=256) | ~35 ms/item | 307–632 ms/item (**~9× slower**) |
//!
//! tract's single-item path is far faster than candle, but its batch path is an order of magnitude slower (candle's batched GEMM pays off more).
//! So [`HybridBgeEmbedder`] routes **queries through tract, node batch encoding through candle**, and both take
//! **0-based position_ids** to land in the same vector space (so switching needs no re-encoding).
//!
//! # position_ids convention (important)
//!
//! bge-m3's backbone is XLM-RoBERTa (`padding_idx=1`); the HF reference uses **2-based**
//! (`arange(2, seq+2)`), measured `cos(2-based, HF) == 1.00000`; while candle's
//! `BertEmbeddings` hard-codes **0-based**, measured `cos(0-based, HF) ≈ 0.96`.
//! That is, all current vectors are offset 2 from the reference (queries and nodes agree, so retrieval still works).
//! Here we **deliberately keep 0-based to stay consistent with candle's stored node vectors**; to switch to 2-based,
//! see [`Self::POSITION_BASE`] — but that would invalidate all stored vectors and require re-encoding the whole DB.
//!
//! # Enabling
//!
//! ```bash
//! cargo build --release -p gt-app --features model-candle,model-ort
//! export GT_BGE_ONNX=models/bge-m3-onnx/model.onnx   # default is this path
//! ```
//!
//! Weight export: see `tools/export_bge_onnx.py` (the ONNX needs an explicit `position_ids` input
//! to avoid generating a `Range` node in the graph — tract 0.21 fails its int64 inference, fixed in 0.23).

#![cfg(feature = "model-ort")]

use std::sync::Arc;

use tokenizers::Tokenizer;
use tract_onnx::prelude::*;

use crate::embedding::Embedder;

/// bge retrieval instruction prefix: added on the query side, not on the document / code side.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// Sequence cap: truncate when too long (consistent with candle's `MAX_TOKENS`, keeping both sides aligned).
const MAX_TOKENS: usize = 256;

/// tract's concrete runnable model type. Batch dim fixed at 1, sequence dim kept symbolic,
/// so the same plan accepts any length (no need to pad to a fixed length, saving lots of wasted compute).
type BgeModel = tract_onnx::prelude::RunnableModel<
    tract_core::model::TypedFact,
    Box<dyn tract_core::ops::TypedOp>,
>;

/// bge-m3 encoder based on `tract` (pure-Rust ONNX inference, zero system dependencies).
///
/// Used only for **single-item** encoding (query / single document). For batch encoding use candle (see module docs).
pub struct OrtBgeEmbedder {
    model: Arc<BgeModel>,
    tokenizer: Tokenizer,
    dim: usize,
}

impl OrtBgeEmbedder {
    /// position_ids start value: **0 = same space as candle's stored node vectors (self-consistent retrieval, no re-encoding)**.
    ///
    /// Background: bge-m3's backbone XLM-RoBERTa (`padding_idx=1`) has an HF reference using **2-based**
    /// (`arange(2, seq+2)`, `cos(2-based, HF) == 1.000000`); while candle's `BertEmbeddings`
    /// hard-codes **0-based** (`0..seq`, `cos(0-based, HF) ≈ 0.96`). All current node vectors are candle
    /// 0-based encoded and persisted, so here we **deliberately keep 0-based**, making "query (tract)" and "document (candle
    /// batch)" land in the same space with self-consistent retrieval (currently @5=37/48), and **no re-encoding**.
    /// Measured `cos(candle 0-based, tract 0-based) == 1.000000`.
    ///
    /// To upgrade to the HF reference 2-based space (~+1/@5 marginal quality gain) requires shifting candle's
    /// `position_embeddings` weights up by 2 rows and re-encoding the whole DB — see project notes, out of scope here.
    const POSITION_BASE: i64 = 0;

    /// Load from local ONNX weights + tokenizer.json.
    pub fn load(
        model_onnx: &str,
        tokenizer_json: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let m0 = tract_onnx::onnx().model_for_path(model_onnx)?;
        // Batch dim set to 1 (otherwise `Flatten` would compute a symbolic square and fail to fix the shape); sequence dim kept symbolic.
        let sym = m0.sym("seq");
        let shape: TVec<TDim> = tvec![TDim::Val(1), TDim::Sym(sym)];
        let mk = || InferenceFact::dt_shape(i64::datum_type(), shape.clone());
        let typed = m0
            .with_input_fact(0, mk())?
            .with_input_fact(1, mk())?
            .with_input_fact(2, mk())?
            .with_input_fact(3, mk())?
            .into_optimized()?;
        let dims = typed.output_fact(0)?.shape.dims().to_vec();
        let dim = dims
            .get(2)
            .and_then(|d| d.to_i64().ok())
            .map(|d| d as usize)
            .ok_or("无法从 ONNX 输出 fact 推断 hidden 维度")?;
        // tract 0.23: `into_runnable()` returns `Arc<SimplePlan<…>>` directly.
        let model: Arc<BgeModel> = typed.into_runnable()?;
        let tokenizer = Tokenizer::from_file(tokenizer_json)?;
        Ok(Self {
            model,
            tokenizer,
            dim,
        })
    }

    /// Encode a piece of text: `is_query=true` adds the bge retrieval prefix, otherwise treat as the document side (no prefix).
    fn encode(&self, text: &str, is_query: bool) -> Vec<f32> {
        let t = if is_query {
            format!("{QUERY_PREFIX}{text}")
        } else {
            text.to_string()
        };
        let enc = self.tokenizer.encode(t, true).expect("tokenize 失败");
        let mut ids: Vec<i64> = enc.get_ids().iter().map(|x| *x as i64).collect();
        let mut attn = vec![1i64; ids.len()];
        if ids.len() > MAX_TOKENS {
            ids.truncate(MAX_TOKENS);
            attn.truncate(MAX_TOKENS);
        }
        // The sequence dim is symbolic, no padding needed — forward with the real length directly.
        let pos: Vec<i64> = (0..ids.len() as i64).map(|i| i + Self::POSITION_BASE).collect();
        run_session(&self.model, &ids, &attn, &pos)
    }
}

/// Run one forward pass (length = `ids.len()`, dynamic), return the L2-normalized [CLS] vector.
fn run_session(model: &Arc<BgeModel>, ids: &[i64], attn: &[i64], pos_ids: &[i64]) -> Vec<f32> {
    let n = ids.len();
    let input = Tensor::from_shape(&[1, n], ids).expect("input tensor");
    let attn_t = Tensor::from_shape(&[1, n], attn).expect("attn tensor");
    let ttype = Tensor::from_shape(&[1, n], &vec![0i64; n]).expect("ttype tensor");
    let pos = Tensor::from_shape(&[1, n], pos_ids).expect("pos tensor");

    let outputs = model
        .run(tvec!(input.into(), attn_t.into(), ttype.into(), pos.into()))
        .expect("tract run");
    let out: &Tensor = &outputs[0];
    let view = out.to_plain_array_view::<f32>().expect("to_plain_array_view");
    // view: [1, seq, hidden]; take [CLS] (0th token in the sequence)
    let hidden = view.shape()[2];
    let mut vec = vec![0f32; hidden];
    for j in 0..hidden {
        vec[j] = view[[0, 0, j]];
    }
    // L2 normalize (consistent with bge official usage)
    let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    vec.iter().map(|x| x / norm).collect()
}

impl Embedder for OrtBgeEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    /// Document / code side: **no** retrieval prefix.
    fn embed(&self, text: &str) -> Vec<f32> {
        self.encode(text, false)
    }

    /// Query side: add the bge retrieval prefix.
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.encode(text, true)
    }

    // `embed_batch` doesn't override: by default it goes item-by-item through `embed` (document side, no prefix), semantically correct,
    // but tract batch is very slow — in production [`HybridBgeEmbedder`] should hand batching to candle.
}

/// Hybrid encoder: **queries through tract, node batch encoding through candle**.
///
/// Takes the best of both: a single query forward is ~140 ms (candle ~813 ms), batch encoding ~35 ms/item
/// (tract 307–632 ms/item). Both sides **share 0-based position_ids**, landing in the same space as candle's stored node vectors
/// with no re-encoding; measured `cos(candle 0-based, tract 0-based) == 1.000000`, retrieval self-consistent.

#[cfg(feature = "model-candle")]
pub struct HybridBgeEmbedder {
    candle: crate::embed_model::CandleBgeEmbedder,
    tract: OrtBgeEmbedder,
}

#[cfg(feature = "model-candle")]
impl HybridBgeEmbedder {
    /// `safetensors_dir` for candle (config.json / tokenizer.json),
    /// `model_onnx` for tract.
    pub fn load(
        safetensors_dir: &str,
        model_onnx: &str,
        tokenizer_json: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            candle: crate::embed_model::CandleBgeEmbedder::load(safetensors_dir)
                .map_err(|e| e.to_string())?,
            tract: OrtBgeEmbedder::load(model_onnx, tokenizer_json)?,
        })
    }
}

#[cfg(feature = "model-candle")]
impl Embedder for HybridBgeEmbedder {
    fn dim(&self) -> usize {
        self.candle.dim()
    }

    /// Document side goes through candle (fast batch).
    fn embed(&self, text: &str) -> Vec<f32> {
        self.candle.embed(text)
    }

    /// Query side goes through tract (fast single item).
    ///
    /// With fallback: in some builds tract may panic due to graph-optimization differences (measured: a debug build's attention
    /// `Where` op degrades shape and crashes, release is fine). Here we catch and fall back to candle —
    /// their vector spaces agree (cos==1.0), results are equivalent, just slower, but the request won't be killed.
    fn embed_query(&self, text: &str) -> Vec<f32> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.tract.embed_query(text)
        })) {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!("tract query encoding failed; falling back to candle encoding for this call (equivalent result, only slower)");
                self.candle.embed_query(text)
            }
        }
    }

    /// Batch goes through candle: tract's batch path is ~9× slower than candle's.
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.candle.embed_batch(texts)
    }
}
