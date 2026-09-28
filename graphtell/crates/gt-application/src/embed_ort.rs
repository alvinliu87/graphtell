//! 真实神经网络嵌入适配器（**特性门控，默认不编译**）。
//!
//! 用纯 Rust 推理引擎 `tract` 加载导出的 `bge-m3` ONNX 权重，做真正的跨语言语义向量。
//! 选 tract 是因其零系统依赖（不需要 onnxruntime 的 openssl），离线构建 / 测试照常 green。
//!
//! # 为什么 tract 只负责「查询」，批量编码仍走 candle
//!
//! 实测（本机 16 核 AMD，release）：
//!
//! | 路径 | candle | tract |
//! |---|---|---|
//! | 单条前向（查询，seq≈24） | ~770 ms | **~161 ms（快 ~5×）** |
//! | 批量编码（节点，batch=256） | ~35 ms/条 | 307–632 ms/条（**慢 ~9×**） |
//!
//! tract 的单条路径远快于 candle，但批量路径反而慢一个量级（candle 的分批 GEMM 更划算）。
//! 因此 [`HybridBgeEmbedder`] 让**查询走 tract、节点批量编码走 candle**，两侧都取
//! **0-based position_ids** 以保证落在同一向量空间（故切换无需重编码）。
//!
//! # position_ids 的口径（重要）
//!
//! bge-m3 主干是 XLM-RoBERTa（`padding_idx=1`），HF 参考实现用 **2-based**
//! （`arange(2, seq+2)`），实测 `cos(2-based, HF) == 1.00000`；而 candle 的
//! `BertEmbeddings` 写死 **0-based**，实测 `cos(0-based, HF) ≈ 0.96`。
//! 即现役全部向量相对参考偏移 2 位（查询与节点一致，故检索仍可用）。
//! 这里**刻意沿用 0-based 以与 candle 存量向量保持一致**；若要修正为 2-based，
//! 见 [`Self::POSITION_BASE`] —— 但那会让全部存量向量失效，必须整库重编码。
//!
//! # 启用
//!
//! ```bash
//! cargo build --release -p gt-app --features model-candle,model-ort
//! export GT_BGE_ONNX=models/bge-m3-onnx/model.onnx   # 缺省即此路径
//! ```
//!
//! 权重导出见 `tools/export_bge_onnx.py`（ONNX 需带 `position_ids` 显式输入，
//! 避免图内生成 `Range` 节点 —— tract 0.21 对其 int64 推断会失败，0.23 已修复）。

#![cfg(feature = "model-ort")]

use std::sync::Arc;

use tokenizers::Tokenizer;
use tract_onnx::prelude::*;

use crate::embedding::Embedder;

/// bge 检索指令前缀：查询侧加，文档 / 代码侧不加。
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// 序列上限：超长截断（与 candle 侧 `MAX_TOKENS` 一致，保证两侧口径相同）。
const MAX_TOKENS: usize = 256;

/// tract 的 runnable 模型具体类型。批维固定为 1、序列维保持符号，
/// 故同一份 plan 可接受任意长度（不必补齐到定长，省掉大量无用算力）。
type BgeModel = tract_onnx::prelude::RunnableModel<
    tract_core::model::TypedFact,
    Box<dyn tract_core::ops::TypedOp>,
>;

/// 基于 `tract`（纯 Rust ONNX 推理，零系统依赖）的 bge-m3 编码器。
///
/// 只用于**单条**编码（查询 / 单文档）。批量编码请用 candle（见模块文档）。
pub struct OrtBgeEmbedder {
    model: Arc<BgeModel>,
    tokenizer: Tokenizer,
    dim: usize,
}

impl OrtBgeEmbedder {
    /// position_ids 起始值：**0 = 与 candle 存量节点向量同一空间（自洽检索、免重编码）**。
    ///
    /// 背景：bge-m3 主干 XLM-RoBERTa（`padding_idx=1`）的 HF 参考实现用 **2-based**
    /// （`arange(2, seq+2)`，`cos(2-based, HF) == 1.000000`）；而 candle 的 `BertEmbeddings`
    /// 写死 **0-based**（`0..seq`，`cos(0-based, HF) ≈ 0.96`）。现役全部节点向量是 candle
    /// 0-based 编码落盘的，因此这里**刻意沿用 0-based**，使「查询(tract)」与「文档(candle
    /// 批量)」落在同一空间、检索自洽（当前 @5=37/48），且**无需重编码**。
    /// 实测 `cos(candle 0-based, tract 0-based) == 1.000000`。
    ///
    /// 若要升级到 HF 参考的 2-based 空间（对质量有约 +1/@5 的边际提升），需要给 candle 侧
    /// 做「position_embeddings 权重前移 2 行」并整库重编码 —— 见项目记录，不在本路径内。
    const POSITION_BASE: i64 = 0;

    /// 从本地 ONNX 权重 + tokenizer.json 加载。
    pub fn load(
        model_onnx: &str,
        tokenizer_json: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let m0 = tract_onnx::onnx().model_for_path(model_onnx)?;
        // 批维定为 1（否则 `Flatten` 会算出符号平方而无法定型），序列维保持符号。
        let sym = m0.sym("seq");
        let shape: TVec<TDim> = tvec![TDim::Val(1), TDim::Sym(sym)];
        let mk = || InferenceFact::dt_shape(i64::datum_type(), shape.clone());
        let typed = m0
            .with_input_fact(0, mk())?
            .with_input_fact(1, mk())?
            .with_input_fact(2, mk())?
            .with_input_fact(3, mk())?
            .into_optimized()?;
        // 输出维度直接取输出 fact（[1, seq, hidden]），**不要**跑 dummy 前向：
        // 极短序列会让注意力里的 `Where` 算子形状退化（实测 seq=2 时
        // `expected 1,1,seq,seq got 1,1,1,2` 直接 panic，debug 构建必崩）。
        let dims = typed.output_fact(0)?.shape.dims().to_vec();
        let dim = dims
            .get(2)
            .and_then(|d| d.to_i64().ok())
            .map(|d| d as usize)
            .ok_or("无法从 ONNX 输出 fact 推断 hidden 维度")?;
        // tract 0.23：`into_runnable()` 直接返回 `Arc<SimplePlan<…>>`。
        let model: Arc<BgeModel> = typed.into_runnable()?;
        let tokenizer = Tokenizer::from_file(tokenizer_json)?;
        Ok(Self {
            model,
            tokenizer,
            dim,
        })
    }

    /// 编码一段文本：`is_query=true` 加 bge 检索前缀，否则按文档侧处理（不加）。
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
        // 序列维是符号的，无需补齐 —— 直接按真实长度前向。
        let pos: Vec<i64> = (0..ids.len() as i64).map(|i| i + Self::POSITION_BASE).collect();
        run_session(&self.model, &ids, &attn, &pos)
    }
}

/// 跑一次前向（长度即 `ids.len()`，动态），返回 [CLS] 的 L2 归一化向量。
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

    /// 文档 / 代码侧：**不加**检索前缀。
    fn embed(&self, text: &str) -> Vec<f32> {
        self.encode(text, false)
    }

    /// 查询侧：加 bge 检索前缀。
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.encode(text, true)
    }

    // `embed_batch` 不覆写：默认逐条走 `embed`（文档侧、无前缀），语义正确，
    // 但 tract 批量很慢 —— 生产应由 [`HybridBgeEmbedder`] 把批量交给 candle。
}

/// 混合编码器：**查询走 tract、节点批量编码走 candle**。
///
/// 取两家之长：查询单次前向 ~140 ms（candle ~813 ms），批量编码 ~35 ms/条
/// （tract 307–632 ms/条）。两侧**共用 0-based position_ids**，与 candle 存量节点向量
/// 同一空间、无需重编码；实测 `cos(candle 0-based, tract 0-based) == 1.000000`，检索自洽。
#[cfg(feature = "model-candle")]
pub struct HybridBgeEmbedder {
    candle: crate::embed_model::CandleBgeEmbedder,
    tract: OrtBgeEmbedder,
}

#[cfg(feature = "model-candle")]
impl HybridBgeEmbedder {
    /// `safetensors_dir` 供 candle（含 config.json / tokenizer.json），
    /// `model_onnx` 供 tract。
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

    /// 文档侧走 candle（批量快）。
    fn embed(&self, text: &str) -> Vec<f32> {
        self.candle.embed(text)
    }

    /// 查询侧走 tract（单条快）。
    ///
    /// 带降级：tract 在个别构建下可能因图优化差异 panic（实测 debug 构建的注意力
    /// `Where` 算子会形状退化而崩，release 正常）。这里捕获并降级到 candle ——
    /// 两者向量空间一致（cos==1.0），结果等价，只是慢一些，但请求不会被打挂。
    fn embed_query(&self, text: &str) -> Vec<f32> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.tract.embed_query(text)
        })) {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!("tract 查询编码失败，本次降级到 candle 编码（结果等价，仅变慢）");
                self.candle.embed_query(text)
            }
        }
    }

    /// 批量走 candle：tract 的批量路径比 candle 慢约 9 倍。
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.candle.embed_batch(texts)
    }
}
