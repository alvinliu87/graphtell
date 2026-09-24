//! 真实神经网络嵌入适配器（**特性门控，默认不编译**）。
//!
//! 这是"强版"离线向量召回的落点：用本地 `candle` 跑 `bge-m3`，把中文意图与英文
//! 代码符号映射到同一语义空间。**本文件只在 `model` feature 开启时参与编译**，
//! 因此任何没有网络 / 没有模型权重的机器上，`cargo build` / `cargo test` 都照常 green。
//!
//! # 在能联网的机器上启用
//!
//! 1) 在 `crates/gt-application/Cargo.toml` 增加（candle 版本请对齐你拉取的权重）：
//!
//! ```toml
//! [features]
//! model = ["dep:candle-core", "dep:candle-nn", "dep:candle-transformers", "dep:tokenizers", "dep:safetensors"]
//!
//! [dependencies]
//! candle-core = { version = "0.6", optional = true }
//! candle-nn  = { version = "0.6", optional = true }
//! candle-transformers = { version = "0.6", optional = true }
//! tokenizers = { version = "0.20", optional = true }
//! safetensors = { version = "0.4", optional = true }
//! ```
//!
//! 2) 下载 `bge-m3` 权重到某个目录 `MODEL_DIR`，需含：
//!    - `model.safetensors`（dense 权重）
//!    - `config.json`
//!    - `tokenizer.json`（bge-m3 自带，含检索指令模板）
//!
//! 3) 在组装根（container / router）里注入：
//!
//! ```rust,ignore
//! let embedder = Arc::new(CandleBgeEmbedder::load(MODEL_DIR)?);
//! let svc = RecallService::with_embedder(store, fs, scanner, embedder);
//! ```
//!
//! 召回流程（`recall`、`score_node`、图扩展、`Embedder` trait）**零改动**——
//! 只是把 `default_embedder()` 换成了真模型。

#![cfg(feature = "model")]

use std::sync::Arc;

use candle_core::{DType, Device, Tensor};
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::Tokenizer;

use crate::embedding::Embedder;

/// 检索指令前缀：bge 系列要求对「待检索文本」加这个前缀以激活 retrieval 表征。
const RETRIEVE_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// 基于 `candle` + `bge-m3` 的本地语义编码器（CPU 可跑，纯离线）。
pub struct CandleBgeEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    dim: usize,
    device: Device,
}

impl CandleBgeEmbedder {
    /// 从本地目录加载权重（`model.safetensors` + `config.json` + `tokenizer.json`）。
    pub fn load(model_dir: &str) -> candle_core::Result<Self> {
        let device = Device::Cpu;

        let config: Config =
            serde_json::from_str(&std::fs::read_to_string(format!("{model_dir}/config.json"))?)
                .map_err(|e| candle_core::Error::Msg(e.to_string()))?;
        let weights = std::fs::read(format!("{model_dir}/model.safetensors"))
            .map_err(|e| candle_core::Error::Msg(e.to_string()))?;
        let mmap = unsafe { candle_core::safetensors::MmapedSafetensors::new(weights)? };
        let vb = candle_core::VarBuilder::from_safetensors(vec![mmap], DType::F32, &device)?;
        // `with_pooling = false`：我们自己做 mean-pooling + L2 归一化。
        let model = BertModel::load(vb, &config, false)?;
        let tokenizer = Tokenizer::from_file(format!("{model_dir}/tokenizer.json"))
            .map_err(|e| candle_core::Error::Msg(e.to_string()))?;

        Ok(Self {
            model,
            tokenizer,
            dim: config.hidden_size,
            device,
        })
    }
}

impl Embedder for CandleBgeEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    /// 编码一段文本为 L2 归一化的语义向量。
    ///
    /// 失败会 panic（参考实现）；生产环境应改造成返回 `Result` 并在 `with_embedder`
    /// 处传播。
    fn embed(&self, text: &str) -> Vec<f32> {
        let encoding = self
            .tokenizer
            .encode(format!("{RETRIEVE_PREFIX}{text}"), true)
            .expect("tokenize 失败");
        let ids: Vec<u32> = encoding.get_ids().to_vec();
        let seq_len = ids.len();

        let input_ids = Tensor::new(ids, &self.device)
            .expect("input_ids")
            .unsqueeze(0)
            .expect("unsqueeze");
        let type_ids = Tensor::new(vec![0u32; seq_len], &self.device)
            .expect("type_ids")
            .unsqueeze(0)
            .expect("unsqueeze");

        // [1, seq, hidden]
        let hidden = self
            .model
            .forward(&input_ids, &type_ids)
            .expect("bert forward");
        // 去掉 batch 维 → [seq, hidden]
        let hidden = hidden.squeeze(0).expect("squeeze");
        // mean-pooling（忽略 padding 由 seq_len 近似，bge 输入已无额外 pad）
        let summed = hidden.sum(0).expect("sum");
        let mean = summed
            .broadcast_div(&Tensor::new(seq_len as f32, &self.device).expect("scalar"))
            .expect("mean");
        // L2 归一化
        let norm = mean.sqr().expect("sqr").sum_all().expect("sum").sqrt().expect("norm");
        let normalized = mean.broadcast_div(&norm).expect("normalize");

        normalized.to_vec1::<f32>().expect("to_vec")
    }
}
