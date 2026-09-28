//! 离线文本编码器（向量召回的「编码」一侧）。
//!
//! 当前默认实现 [`LocalHashingEmbedder`] 是**零依赖、纯离线**的：用特征哈希
//! （hashing trick）把文本投到固定维度再 L2 归一化。它**不是**神经网络语义向量，
//! 但能在本仓库离线跑起来，提供「软匹配」（跨字段、部分重叠即得余弦分），足以
//! 演示「向量种子 + 图扩展」的完整管线。
//!
//! 真正的语义模型（`bge-m3` / `unixcoder`，本地 `candle` / `ort` 推理）只要实现
//! [`Embedder`] trait 即可无缝替换，召回流程无需改动。

use std::sync::{Arc, OnceLock};

/// 文本 → 稠密向量的编码器。可替换、可离线。
pub trait Embedder: Send + Sync {
    /// 编码一段文本为向量（已 L2 归一化）。
    fn embed(&self, text: &str) -> Vec<f32>;
    /// 向量维度。
    fn dim(&self) -> usize;
    /// 编码「查询」文本（召回时用户查询侧使用）。
    ///
    /// 默认与 [`Embedder::embed`] 相同；但部分模型（如 bge 系列）要求**查询侧**
    /// 加检索前缀、文档侧不加，此时应覆写本方法，使查询与文档落入同一向量空间。
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.embed(text)
    }

    /// 批量编码（文档侧，不加前缀）。
    ///
    /// 默认逐条调用 [`Embedder::embed`]；真实模型（bge-m3 / candle）应覆写为「单次前向」
    /// 把冷启动从「逐节点 N 次前向」降为「少量大批次前向」——对大图召回是数量级提速
    /// （bge-m3 在 CPU 上单条前向 ~百毫秒，批量前向可 amortize 到几毫秒 / 条）。
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// 两段向量的余弦相似度（重算各自 L2 范数，避免预归一化的浮点漂移）。
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

/// 零依赖的本地编码器：特征哈希（signed hash → ±1 投到固定维度）。
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

/// 默认离线编码器（256 维，零外部依赖）。
pub fn default_embedder() -> Arc<dyn Embedder> {
    Arc::new(LocalHashingEmbedder::new(256))
}

/// 当前生效的 embedding 后端描述（供 status 端点 / UI 展示）。
static BACKEND_INFO: OnceLock<String> = OnceLock::new();
/// 当前生效的 embedding 向量维度。
static BACKEND_DIM: OnceLock<usize> = OnceLock::new();

fn set_backend_info(name: &str, dim: usize) {
    let _ = BACKEND_INFO.set(name.to_string());
    let _ = BACKEND_DIM.set(dim);
}

/// 当前 embedding 后端的人类可读描述（如 `bge-m3-local`、`remote-openai (http://...)`）。
pub fn embedding_backend_info() -> String {
    BACKEND_INFO.get().cloned().unwrap_or_else(|| "unknown".to_string())
}

/// 当前 embedding 向量维度；未知时为 0。
pub fn embedding_dim() -> usize {
    BACKEND_DIM.get().copied().unwrap_or(0)
}

/// 解析召回用的编码器：依据 `GT_EMBEDDING_BACKEND` 选择后端。
///
/// - `auto`（默认）：本地 bge-m3 优先，缺权重则安全退回离线词面哈希；
/// - `local`：强制本地 bge-m3，缺失也不退回（明确报错，提示先 `graphtell model fetch`）；
/// - `url` / `remote`：指向用户自带的 embedding 服务（OpenAI 兼容 / TEI 原生）；
/// - `hash` / `off` / `none`：纯离线词面哈希（零依赖，质量弱但保底可用）。
///
/// 返回值可直接注入 [`crate::RecallService`]。生产入口（CLI / HTTP router）都走它，
/// 因此「有权重就走真实语义、没有就退回离线」是统一行为，无需调用方关心。
pub fn resolve_recall_embedder() -> Arc<dyn Embedder> {
    let backend = std::env::var("GT_EMBEDDING_BACKEND").unwrap_or_else(|_| "auto".to_string());
    match backend.as_str() {
        "hash" | "off" | "none" => {
            set_backend_info("hash (离线词面, 256d)", 256);
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
                    tracing::warn!("远程 embedding 加载失败（{err}），退回离线词面编码器");
                    set_backend_info("hash (remote failed)", 256);
                    Arc::new(LocalHashingEmbedder::new(256))
                }
            };
        }
        "local" => {
            // 强制本地 bge-m3：缺失也不退回词面，让用户明确感知权重缺失。
            if let Some(e) = try_real_recall_embedder() {
                let dim = e.dim();
                set_backend_info("bge-m3-local", dim);
                return e;
            }
            tracing::error!(
                "GT_EMBEDDING_BACKEND=local 但本地 bge-m3 权重缺失，请先 `graphtell model fetch`"
            );
            set_backend_info("hash (local missing)", 256);
            return Arc::new(LocalHashingEmbedder::new(256));
        }
        "auto" | _ => {}
    }

    // ---- auto：本地 bge-m3 优先，否则退回离线词面（默认行为） ----
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
                    tracing::info!("已加载混合 bge-m3 编码器（查询 tract / 批量 candle，{onnx}）");
                    set_backend_info("bge-m3-local (hybrid tract+candle)", embedder.dim());
                    return Arc::new(embedder);
                }
                Err(err) => tracing::warn!("混合编码器加载失败（{onnx}），回退 candle：{err}"),
            }
        }
    }
    #[cfg(feature = "model-candle")]
    {
        let dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        match crate::embed_model::CandleBgeEmbedder::load(&dir) {
            Ok(embedder) => {
                tracing::info!("已加载真实 bge-m3 语义编码器（{dir}）");
                set_backend_info("bge-m3-local (candle)", embedder.dim());
                return Arc::new(embedder);
            }
            Err(err) => {
                tracing::warn!("bge-m3 模型加载失败（{dir}），退回本地哈希编码器：{err}");
            }
        }
    }
    set_backend_info("hash (offline fallback, 256d)", 256);
    default_embedder()
}

/// 仅当编译了 `model-candle` 且 `GT_BGE_MODEL` 权重可用时返回真实 bge-m3 编码器，
/// 否则返回 `None`（调用方应退回词面 / 快速向量路，且关闭后台预热）。
///
/// 与 [`resolve_recall_embedder`] 的区别：后者在无权重时**安全退回**默认哈希编码器；
/// 本函数把「是否具备真实语义」这一事实显式交回调用方，便于决定是否触发后台预热。
pub fn try_real_recall_embedder() -> Option<Arc<dyn Embedder>> {
    // 同 [`resolve_recall_embedder`]：优先混合编码器（查询 tract / 批量 candle）。
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
                    tracing::info!("已加载混合 bge-m3 编码器（查询 tract / 批量 candle，{onnx}）");
                    return Some(Arc::new(embedder));
                }
                Err(err) => tracing::warn!("混合编码器加载失败（{onnx}），回退 candle：{err}"),
            }
        }
    }
    #[cfg(feature = "model-candle")]
    {
        let dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        match crate::embed_model::CandleBgeEmbedder::load(&dir) {
            Ok(embedder) => {
                tracing::info!("已加载真实 bge-m3 语义编码器（{dir}）");
                Some(Arc::new(embedder))
            }
            Err(err) => {
                tracing::warn!("bge-m3 模型加载失败（{dir}），无语义编码器：{err}");
                None
            }
        }
    }
    #[cfg(not(feature = "model-candle"))]
    {
        tracing::info!("未编译 model-candle，无语义编码器（仅词面 / 快速向量路）");
        None
    }
}

/// 抽取用于编码的特征：ASCII token（保留大小写拆 camelCase/snake 后再转小写）
/// + CJK 字符 + CJK 二元组。
fn text_features(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // ASCII / 数字 token：先用原始大小写拆 camelCase（OrderService → Order+Service），
    // 再转小写，避免提前 lowercase 抹掉大小写边界。
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
    // CJK 字符 + 二元组
    let chars: Vec<char> = text.chars().filter(|c| is_cjk(*c)).collect();
    for c in &chars {
        out.push(c.to_string());
    }
    for w in chars.windows(2) {
        out.push(w.iter().collect());
    }
    out
}

/// 把 `placeOrder` / `applyDiscount` / `unused_log` 拆成子词。
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

/// FNV-1a 64 位，取最高位定符号，返回有符号哈希。
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
        assert!((norm - 1.0).abs() < 1e-6, "应 L2 归一化，实际范数 {norm}");
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
        // 查询带 order / discount / coupon；related 共享这三个，unrelated 完全不沾。
        let q = e.embed("order discount coupon place 下单改优惠");
        let related = e.embed("class OrderService applyDiscount vipCoupon");
        let unrelated = e.embed("unused_log config cache session");
        assert!(
            cosine(&q, &related) > cosine(&q, &unrelated),
            "共享 order/discount/coupon 的节点应比无关节点更近"
        );
    }
}
