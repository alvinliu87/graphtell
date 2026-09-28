//! 真实神经网络嵌入适配器（**特性门控，默认不编译**）。
//!
//! 用 `candle`（纯 Rust）直接加载本地 `bge-m3` 的 safetensors 权重，做真正的跨语言语义向量。
//! 纯离线、CPU 可跑、零系统依赖（不需要 onnxruntime 的 openssl）。
//! **本文件只在 `model-candle` feature 开启时参与编译**，因此任何没有模型权重的机器上，
//! `cargo build` / `cargo test` 都照常 green。
//!
//! # 在能联网的机器上启用
//!
//! 1)（一次性）拉权重并转 safetensors：
//!
//! ```bash
//! python3 -m pip install torch sentence-transformers modelscope safetensors
//! export HF_ENDPOINT=https://hf-mirror.com
//! python3 tools/bge_demo.py                 # 经 modelscope 拉 bge-m3 到 models/bge-m3-ms/
//! python3 tools/convert_bge_safetensors.py # 转出 models/bge-m3-safetensors/{model.safetensors,config.json,tokenizer.json}
//! ```
//!
//! 2) 在 `gt-application/Cargo.toml` 启用 feature `model-candle`（已内置）。
//!
//! 3) 组装根注入（召回流程零改动）：
//!
//! ```rust,ignore
//! let embedder = Arc::new(CandleBgeEmbedder::load("models/bge-m3-safetensors")?);
//! let svc = RecallService::with_embedder(store, fs, scanner, embedder);
//! ```
//!
//! 已验证：同份权重在 onnxruntime 下与 sentence-transformers 输出 cosine==1.0；
//! 对 `下单改优惠` 的语义召回命中 `applyDiscount / placeOrder` 等英文节点（见 tools/bge_demo.py）。

#![cfg(feature = "model-candle")]

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::embedding::Embedder;

/// 检索指令前缀：bge 系列要求对「待检索文本」加这个前缀以激活 retrieval 表征。
const RETRIEVE_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// 池化方式：bge 用 [CLS]，e5 用「去 Padding 均值」。
#[derive(Clone, Copy)]
enum Pooling {
    Cls,
    Mean,
}

/// 模型目录可选的元数据（`embed_meta.json`），覆盖默认（bge 风格）的池化与前缀。
/// 缺省即保持 bge-m3 行为，向后兼容。
#[derive(Deserialize)]
struct EmbedMeta {
    #[serde(default)]
    pooling: String,
    #[serde(default)]
    query_prefix: String,
    #[serde(default)]
    doc_prefix: String,
}

/// 基于 `candle` 的本地语义编码器（CPU 可跑，纯离线）。bge / e5 共用同一套 BERT 主干
/// （xlm-roberta 与 bert 权重命名一致，`BertModel` 均可加载），仅池化与前缀不同，
/// 由 `embed_meta.json` 区分。
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
    /// 从本地目录加载权重（`model.safetensors` + `config.json` + `tokenizer.json`）。
    /// 可选的 `embed_meta.json` 覆盖池化与前缀：`pooling: "mean"` + `query_prefix`/`doc_prefix`
    /// 用于 e5 系列；缺省保持 bge 风格（[CLS] + `RETRIEVE_PREFIX`）。
    pub fn load(model_dir: &str) -> candle_core::Result<Self> {
        let device = Device::Cpu;

        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(format!("{model_dir}/config.json"))
                .map_err(|e| candle_core::Error::Msg(e.to_string()))?,
        )
        .map_err(|e| candle_core::Error::Msg(e.to_string()))?;

        let safetensors_path = format!("{model_dir}/model.safetensors");
        // mmap 读取权重文件：文件只读、加载期间不改动即安全。
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[safetensors_path], DType::F32, &device)?
        };
        let model = BertModel::load(vb, &config)?;
        let tokenizer = Tokenizer::from_file(format!("{model_dir}/tokenizer.json"))
            .map_err(|e| candle_core::Error::Msg(e.to_string()))?;

        // 默认 bge 行为；有 `embed_meta.json` 则按模型类型覆盖（e5 用 mean + query:/passage:）。
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

/// 单条文本参与编码的最大 token 数。
///
/// 节点侧文本（名 / fqn / 关系摘要）与查询都很短，几百 token 足够表达语义。
/// 不加这个上限时，batch 会按「本批最长那条」补齐，一条超长文本就能把整批注意力成本
/// 抬到 O(batch × seq²) —— 预热因此慢到不可用（实测 144 个节点 4 分钟跑不完）。
const MAX_TOKENS: usize = 256;

impl Embedder for CandleBgeEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    /// 编码「文档」侧文本（节点名 / 代码片段）：**不加** bge 检索前缀，取 [CLS] + L2。
    fn embed(&self, text: &str) -> Vec<f32> {
        self.encode(text, false)
    }

    /// 编码「查询」侧文本（用户提问）：加 bge 检索前缀，使其与文档落入同一向量空间。
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.encode(text, true)
    }

    /// 批量编码（文档侧）：把一批文本拼成一个大批次做一次前向，按 `pooling` 取向量并 L2
    /// 归一化。比逐条 `embed` 快一个数量级，是大图召回冷启动的关键优化。文档侧统一加
    /// `doc_prefix`（e5 需要；bge 文档侧无前缀）。
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if texts.is_empty() {
            return Vec::new();
        }
        // 逐条 tokenize，取最大长度用于右补齐（pad 置于序列尾部，[CLS] 始终在首位不受影响）。
        let mut all_ids: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
        let mut max_len = 1usize;
        for t in texts {
            let enc = self
                .tokenizer
                .encode(format!("{}{}", self.doc_prefix, t), true)
                .expect("tokenize 失败");
            let mut ids = enc.get_ids().to_vec();
            // 必须截断：补齐长度取本批最大值，一条超长文本会拖垮整批。
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
        // 按 `pooling` 取 [batch, hidden]
        let pooled = self.pool(&hidden, &attn, max_len, batch).expect("pool");
        // L2 归一化（按行）
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
    /// 统一编码：查询侧 `is_query=true` 时拼 `query_prefix`，文档侧拼 `doc_prefix`
    /// （e5 需要；bge 文档侧无前缀）。按 `pooling` 取向量并 L2 归一化。
    fn encode(&self, text: &str, is_query: bool) -> Vec<f32> {
        let prefix = if is_query {
            &self.query_prefix
        } else {
            &self.doc_prefix
        };
        let t = format!("{prefix}{text}");
        let encoding = self.tokenizer.encode(t, true).expect("tokenize 失败");
        let mut ids: Vec<u32> = encoding.get_ids().to_vec();
        // 同 [`Self::embed_batch`]：限制序列长度，避免超长文本拖垮单次前向。
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

    /// 池化：`Cls` 取序列第 0 位；`Mean` 对真实 token（attn=1）做掩码均值。
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
                // hidden[b,s,h] * attn[b,s,1] → 对 s 求和 → / 求和(attn)[b,1]
                // attn 是 U32 的 mask，需转 F32 才能与 hidden 做乘法。
                let a = attn.to_dtype(DType::F32)?.reshape((batch, seq_len, 1))?;
                let weighted = hidden.broadcast_mul(&a)?;
                let sum = weighted.sum(1)?; // [b, h]
                let denom = a.sum(1)?.clamp(1.0, f64::MAX)?; // [b, 1] 防除零
                sum.broadcast_div(&denom)
            }
        }
    }
}
