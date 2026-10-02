//! A real neural-network embedding adapter (**feature-gated, not compiled by default**).
//!
//! Uses `candle` (pure Rust) to load the local `bge-m3` safetensors weights directly and produce genuine cross-language semantic vectors.
//! Fully offline, runs on CPU, zero system dependencies (no openssl from onnxruntime).
//! **This file only compiles when the `model-candle` feature is on**, so `cargo build` / `cargo test` stay green on any machine without model weights.
//!
//! # Enable on a networked machine
//!
//! 1) (one-off) fetch weights and convert to safetensors:
//!
//! ```bash
//! python3 -m pip install torch sentence-transformers modelscope safetensors
//! export HF_ENDPOINT=https://hf-mirror.com
//! python3 tools/bge_demo.py                 # pull bge-m3 via modelscope into models/bge-m3-ms/
//! python3 tools/convert_bge_safetensors.py # export models/bge-m3-safetensors/{model.safetensors,config.json,tokenizer.json}
//! ```
//!
//! 2) Enable the `model-candle` feature in `gt-application/Cargo.toml` (already built-in).
//!
//! 3) Wire it into the assembly root (zero change to the recall flow):
//!
//! ```rust,ignore
//! let embedder = Arc::new(CandleBgeEmbedder::load("models/bge-m3-safetensors")?);
//! let svc = RecallService::with_embedder(store, fs, scanner, embedder);
//! ```
//!
//! Verified: the same weights give cosine == 1.0 against sentence-transformers under onnxruntime;
//! semantic recall of `apply discount` hits English nodes like `applyDiscount` / `placeOrder` (see tools/bge_demo.py).

#![cfg(feature = "model-candle")]

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::embedding::Embedder;

/// The retrieval instruction prefix: the bge family requires this prefix on the "text to retrieve" to activate the retrieval representation.
const RETRIEVE_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// The pooling method: bge uses [CLS], e5 uses "mean without padding".
#[derive(Clone, Copy)]
enum Pooling {
    Cls,
    Mean,
}

/// Optional metadata (`embed_meta.json`) in the model directory, overriding the default (bge-style) pooling and prefix.
/// Default keeps bge-m3 behaviour for backward compatibility.
#[derive(Deserialize)]
struct EmbedMeta {
    #[serde(default)]
    pooling: String,
    #[serde(default)]
    query_prefix: String,
    #[serde(default)]
    doc_prefix: String,
}

/// A local semantic encoder based on `candle` (runs on CPU, fully offline). bge / e5 share one BERT backbone
/// (xlm-roberta and bert weight names agree, both loadable by `BertModel`); only pooling and prefix differ,
/// distinguished by `embed_meta.json`.
pub struct CandleBgeEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    dim: usize,
    device: Device,
    pooling: Pooling,
    query_prefix: String,
    doc_prefix: String,
}

impl CandleBgeEmbedder {
    /// Load weights from a local directory (`model.safetensors` + `config.json` + `tokenizer.json`).
    /// An optional `embed_meta.json` overrides pooling and prefix: `pooling: "mean"` + `query_prefix`/`doc_prefix`
    /// for the e5 family; default keeps bge style ([CLS] + `RETRIEVE_PREFIX`).
    pub fn load(model_dir: &str) -> candle_core::Result<Self> {
        let device = Device::Cpu;

        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(format!("{model_dir}/config.json"))
                .map_err(|e| candle_core::Error::Msg(e.to_string()))?,
        )
        .map_err(|e| candle_core::Error::Msg(e.to_string()))?;

        let safetensors_path = format!("{model_dir}/model.safetensors");
        // mmap-read the weights file: the file is read-only and not modified during loading, so it is safe.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[safetensors_path], DType::F32, &device)?
        };
        let model = BertModel::load(vb, &config)?;
        let tokenizer = Tokenizer::from_file(format!("{model_dir}/tokenizer.json"))
            .map_err(|e| candle_core::Error::Msg(e.to_string()))?;

        // Default bge behaviour; with `embed_meta.json` the model type overrides (e5 uses mean + query:/passage:).
        let mut pooling = Pooling::Cls;
        let mut query_prefix = RETRIEVE_PREFIX.to_string();
        let mut doc_prefix = String::new();
        if let Ok(s) = std::fs::read_to_string(format!("{model_dir}/embed_meta.json")) {
            if let Ok(m) = serde_json::from_str::<EmbedMeta>(&s) {
                pooling = if m.pooling.trim() == "mean" {
                    Pooling::Mean
                } else {
                    Pooling::Cls
                };
                if !m.query_prefix.is_empty() {
                    query_prefix = m.query_prefix;
                }
                doc_prefix = m.doc_prefix;
            }
        }

        Ok(Self {
            model,
            tokenizer,
            dim: config.hidden_size,
            device,
            pooling,
            query_prefix,
            doc_prefix,
        })
    }
}

/// The maximum token count a single text participates in encoding.
///
/// Node-side text (name / fqn / relation summary) and queries are both short, a few hundred tokens suffice for semantics.
/// Without this cap, the batch pads to "the longest item in the batch", and one over-long text lifts the whole batch's attention cost to O(batch x seq^2) —
/// warm-up thus slows to unusable (measured: 144 nodes did not finish in 4 minutes).
const MAX_TOKENS: usize = 256;

impl Embedder for CandleBgeEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    /// Encode the "document" side text (node name / code snippet): **do not** add the bge retrieval prefix, take [CLS] + L2.
    fn embed(&self, text: &str) -> Vec<f32> {
        self.encode(text, false)
    }

    /// Encode the "query" side text (user question): add the bge retrieval prefix so it falls into the same vector space as documents.
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.encode(text, true)
    }

    /// Batch-encode (document side): pack a batch of texts into one big batch and do one forward pass, take the vector by `pooling` and L2-normalise.
/// An order of magnitude faster than per-item `embed`, the key optimisation for large-graph recall cold-start. The document side uniformly adds
/// `doc_prefix` (e5 needs it; bge document side has no prefix).
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if texts.is_empty() {
            return Vec::new();
        }
        // Tokenize item by item, take the max length for right-padding (pad at sequence tail, [CLS] always at head unaffected).
        let mut all_ids: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
        let mut max_len = 1usize;
        for t in texts {
            let enc = self
                .tokenizer
                .encode(format!("{}{}", self.doc_prefix, t), true)
                .expect("tokenize failed");
            let mut ids = enc.get_ids().to_vec();
            // Must truncate: the pad length takes the batch max, one over-long text drags the whole batch down.
            ids.truncate(MAX_TOKENS);
            if !ids.is_empty() {
                max_len = max_len.max(ids.len());
            }
            all_ids.push(ids);
        }
        let batch = all_ids.len();
        let mut flat = vec![0u32; batch * max_len];
        let mut mask = vec![0u32; batch * max_len];
        for (i, ids) in all_ids.iter().enumerate() {
            for (j, &id) in ids.iter().enumerate() {
                flat[i * max_len + j] = id;
                mask[i * max_len + j] = 1;
            }
        }
        let input_ids = Tensor::new(flat, &self.device)
            .expect("input_ids")
            .reshape((batch, max_len))
            .expect("reshape");
        let type_ids = Tensor::new(vec![0u32; batch * max_len], &self.device)
            .expect("type_ids")
            .reshape((batch, max_len))
            .expect("reshape");
        let attn = Tensor::new(mask, &self.device)
            .expect("attn")
            .reshape((batch, max_len))
            .expect("reshape");

        let hidden = self
            .model
            .forward(&input_ids, &type_ids, Some(&attn))
            .expect("bert forward");
        // Take [batch, hidden] by `pooling`
        let pooled = self.pool(&hidden, &attn, max_len, batch).expect("pool");
        // L2 normalise (per row)
        let norm = pooled
            .sqr()
            .expect("sqr")
            .sum(1)
            .expect("sum")
            .unsqueeze(1)
            .expect("unsqueeze")
            .sqrt()
            .expect("norm");
        let normalized = pooled.broadcast_div(&norm).expect("normalize");

        normalized.to_vec2::<f32>().expect("to_vec")
    }
}

impl CandleBgeEmbedder {
    /// Uniform encode: when `is_query=true`, prepend `query_prefix`; the document side prepends `doc_prefix`
    /// (e5 needs it; bge document side has no prefix). Take the vector by `pooling` and L2-normalise.
    fn encode(&self, text: &str, is_query: bool) -> Vec<f32> {
        let prefix = if is_query {
            &self.query_prefix
        } else {
            &self.doc_prefix
        };
        let t = format!("{prefix}{text}");
        let encoding = self.tokenizer.encode(t, true).expect("tokenize failed");
        let mut ids: Vec<u32> = encoding.get_ids().to_vec();
        // Same as [`Self::embed_batch`]: bound the sequence length to stop one over-long text from dragging the single forward pass down.
        ids.truncate(MAX_TOKENS);
        let seq_len = ids.len();
        if seq_len == 0 {
            return vec![0.0; self.dim];
        }

        let input_ids = Tensor::new(ids, &self.device)
            .expect("input_ids")
            .unsqueeze(0)
            .expect("unsqueeze");
        let type_ids = Tensor::new(vec![0u32; seq_len], &self.device)
            .expect("type_ids")
            .unsqueeze(0)
            .expect("unsqueeze");
        let attn = Tensor::new(vec![1u32; seq_len], &self.device)
            .expect("attn")
            .unsqueeze(0)
            .expect("unsqueeze");

        let hidden = self
            .model
            .forward(&input_ids, &type_ids, Some(&attn))
            .expect("bert forward");
        let pooled = self.pool(&hidden, &attn, seq_len, 1).expect("pool");
        let norm = pooled
            .sqr()
            .expect("sqr")
            .sum_all()
            .expect("sum")
            .sqrt()
            .expect("norm");
        let normalized = pooled.broadcast_div(&norm).expect("normalize");

        let mut v = normalized.to_vec2::<f32>().expect("to_vec");
        v.pop().unwrap()
    }

    /// Pooling: `Cls` takes sequence position 0; `Mean` masked-averages the real tokens (attn=1).
    fn pool(
        &self,
        hidden: &Tensor,
        attn: &Tensor,
        seq_len: usize,
        batch: usize,
    ) -> candle_core::Result<Tensor> {
        match self.pooling {
            Pooling::Cls => hidden.narrow(1, 0, 1)?.squeeze(1),
            Pooling::Mean => {
                // hidden[b,s,h] * attn[b,s,1] -> sum over s -> / sum(attn)[b,1]
                // attn is a U32 mask, must cast to F32 to multiply with hidden.
                let a = attn.to_dtype(DType::F32)?.reshape((batch, seq_len, 1))?;
                let weighted = hidden.broadcast_mul(&a)?;
                let sum = weighted.sum(1)?; // [b, h]
                let denom = a.sum(1)?.clamp(1.0, f64::MAX)?; // [b, 1] guard against divide-by-zero
                sum.broadcast_div(&denom)
            }
        }
    }
}
