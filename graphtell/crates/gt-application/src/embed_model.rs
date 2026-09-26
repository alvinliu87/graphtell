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

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
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
        // `with_pooling = false`：我们自己做 [CLS] 池化 + L2 归一化（bge 用 CLS）。
        let model = BertModel::load(vb, &config)?;
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

    /// 批量编码（文档侧，不加前缀）：把一批文本拼成一个大批次做一次前向，
    /// 取各自 [CLS] 并 L2 归一化。比逐条 `embed` 快一个数量级，是大图召回冷启动的关键优化。
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
                .encode(t.as_str(), true)
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

        // [batch, seq, hidden] → 取 [CLS]（序列第 0 位）→ [batch, hidden]
        let hidden = self
            .model
            .forward(&input_ids, &type_ids, Some(&attn))
            .expect("bert forward");
        let cls = hidden
            .narrow(1, 0, 1)
            .expect("narrow")
            .squeeze(1)
            .expect("squeeze");
        // L2 归一化（按行）
        let norm = cls
            .sqr()
            .expect("sqr")
            .sum(1)
            .expect("sum")
            .unsqueeze(1)
            .expect("unsqueeze")
            .sqrt()
            .expect("norm");
        let normalized = cls.broadcast_div(&norm).expect("normalize");

        normalized.to_vec2::<f32>().expect("to_vec")
    }
}

impl CandleBgeEmbedder {
    /// 统一编码：查询侧 `is_query=true` 时拼上 bge 检索前缀，文档侧不加。
    /// 取 [CLS] token 并 L2 归一化（bge 用 [CLS]，非 mean-pooling）。
    fn encode(&self, text: &str, is_query: bool) -> Vec<f32> {
        let t = if is_query {
            format!("{RETRIEVE_PREFIX}{text}")
        } else {
            text.to_string()
        };
        let encoding = self.tokenizer.encode(t, true).expect("tokenize 失败");
        let mut ids: Vec<u32> = encoding.get_ids().to_vec();
        // 同 [`Self::embed_batch`]：限制序列长度，避免超长文本拖垮单次前向。
        ids.truncate(MAX_TOKENS);
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
            .forward(&input_ids, &type_ids, None)
            .expect("bert forward");
        // 取 [CLS]（序列第 0 个 token）
        let cls = hidden.i((0, 0)).expect("cls index");
        // L2 归一化
        let norm = cls
            .sqr()
            .expect("sqr")
            .sum_all()
            .expect("sum")
            .sqrt()
            .expect("norm");
        let normalized = cls.broadcast_div(&norm).expect("normalize");

        normalized.to_vec1::<f32>().expect("to_vec")
    }
}
