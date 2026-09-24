//! 真实神经网络嵌入适配器（**特性门控，默认不编译**）。
//!
//! 用纯 Rust 推理引擎 `tract` 加载导出的 `bge-m3` ONNX 权重，做真正的跨语言语义向量。
//! 选 tract 是因其零系统依赖（不需要 onnxruntime 的 openssl），离线构建 / 测试照常 green。
//!
//! # 在能联网的机器上启用（权重走 ModelScope 绕开 HF 的 Xet 墙）
//!
//! 1)（一次性）拉权重并转 ONNX：
//!
//! ```bash
//! python3 -m pip install torch sentence-transformers modelscope onnx onnxscript
//! export HF_ENDPOINT=https://hf-mirror.com
//! python3 tools/bge_demo.py            # 经 modelscope 拉 bge-m3 到 models/bge-m3-ms/
//! python3 tools/export_bge_onnx.py     # 导出 ONNX 到 models/bge-m3-onnx/model.onnx
//! ```
//!
//! 2) 在 `gt-application/Cargo.toml` 启用 feature `model-ort`（已内置）。
//!
//! 3) 组装根注入（召回流程零改动）：
//!
//! ```rust,ignore
//! let embedder = Arc::new(OrtBgeEmbedder::load(
//!     "models/bge-m3-onnx/model.onnx",
//!     "models/bge-m3/tokenizer.json",
//! )?);
//! let svc = RecallService::with_embedder(store, fs, scanner, embedder);
//! ```
//!
//! 已验证：同份 ONNX 在 onnxruntime 下与 sentence-transformers 输出 cosine==1.0；
//! 对 `下单改优惠` 的语义召回命中 `applyDiscount / placeOrder` 等英文节点（见 tools/bge_demo.py）。

#![cfg(feature = "model-ort")]

use tract_onnx::prelude::*;
use tract_core::plan::Runnable;
use tokenizers::Tokenizer;

use crate::embedding::Embedder;

/// bge 检索指令前缀：查询侧加，文档 / 代码侧不加。
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// tract 的 runnable 模型具体类型（`Runnable` 只为 `Arc<SimplePlan<…>>` 实现）。
type BgeModel = tract_onnx::prelude::RunnableModel<
    tract_core::model::TypedFact,
    Box<dyn tract_core::ops::TypedOp>,
    tract_core::model::Graph<tract_core::model::TypedFact, Box<dyn tract_core::ops::TypedOp>>,
>;

/// 基于 `tract`（纯 Rust ONNX 推理）的本地 bge-m3 语义编码器（离线，CPU 可跑）。
pub struct OrtBgeEmbedder {
    model: std::sync::Arc<BgeModel>,
    tokenizer: Tokenizer,
    dim: usize,
}

impl OrtBgeEmbedder {
    /// 从本地 ONNX 权重 + tokenizer.json 加载。
    pub fn load(
        model_onnx: &str,
        tokenizer_json: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // 输入维度为动态序列长度（符号维），以支持变长文本。
        let sym = TDim::Sym(0);
        let model: std::sync::Arc<BgeModel> = std::sync::Arc::new(
            tract_onnx::onnx()
                .model_for_path(model_onnx)?
                .with_input_fact(0, InferenceFact::dt_shape(i64::datum_type(), tvec!(TDim::from(1), sym)))?
                .with_input_fact(1, InferenceFact::dt_shape(i64::datum_type(), tvec!(TDim::from(1), sym)))?
                .with_input_fact(2, InferenceFact::dt_shape(i64::datum_type(), tvec!(TDim::from(1), sym)))?
                .into_optimized()?
                .into_runnable()?,
        );
        let tokenizer = Tokenizer::from_file(tokenizer_json)?;
        // dummy 前向推断输出维度（bge-m3 = 1024）
        let dim = run_session(&model, &[1i64, 2, 3]).len();
        Ok(Self {
            model,
            tokenizer,
            dim,
        })
    }
}

/// 跑一次前向：返回 [CLS]（首 token）的 L2 归一化向量。
fn run_session(model: &std::sync::Arc<BgeModel>, ids: &[i64]) -> Vec<f32> {
    let n = ids.len();
    let input = Tensor::from_shape(&[1, n], ids).expect("input tensor");
    let attn = Tensor::from_shape(&[1, n], &vec![1i64; n]).expect("attn tensor");
    let ttype = Tensor::from_shape(&[1, n], &vec![0i64; n]).expect("ttype tensor");

    let outputs = model
        .run(tvec!(input.into(), attn.into(), ttype.into()))
        .expect("tract run");
    let out: &Tensor = &outputs[0];
    let view = out.to_array_view::<f32>().expect("to_array_view");
    // view: [1, seq, hidden]；取 [CLS]（序列第 0 个 token）
    let hidden = view.shape()[2];
    let mut vec = vec![0f32; hidden];
    for j in 0..hidden {
        vec[j] = view[[0, 0, j]];
    }
    // L2 归一化（与 bge 官方用法一致）
    let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    vec.iter().map(|x| x / norm).collect()
}

impl Embedder for OrtBgeEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    /// 编码一段文本为 L2 归一化的语义向量（查询侧加 bge 检索前缀）。
    ///
    /// 失败会 panic（参考实现）；生产环境应改造成返回 `Result` 并在 `with_embedder`
    /// 处传播。
    fn embed(&self, text: &str) -> Vec<f32> {
        let t = format!("{QUERY_PREFIX}{text}");
        let enc = self.tokenizer.encode(t, true).expect("tokenize");
        let ids: Vec<i64> = enc.get_ids().iter().map(|x| *x as i64).collect();
        run_session(&self.model, &ids)
    }
}
