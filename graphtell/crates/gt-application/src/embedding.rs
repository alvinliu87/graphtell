//! 离线文本编码器（向量召回的「编码」一侧）。
//!
//! 当前默认实现 [`LocalHashingEmbedder`] 是**零依赖、纯离线**的：用特征哈希
//! （hashing trick）把文本投到固定维度再 L2 归一化。它**不是**神经网络语义向量，
//! 但能在本仓库离线跑起来，提供「软匹配」（跨字段、部分重叠即得余弦分），足以
//! 演示「向量种子 + 图扩展」的完整管线。
//!
//! 真正的语义模型（`bge-m3` / `unixcoder`，本地 `candle` / `ort` 推理）只要实现
//! [`Embedder`] trait 即可无缝替换，召回流程无需改动。

use std::sync::Arc;

/// 文本 → 稠密向量的编码器。可替换、可离线。
pub trait Embedder: Send + Sync {
    /// 编码一段文本为向量（已 L2 归一化）。
    fn embed(&self, text: &str) -> Vec<f32>;
    /// 向量维度。
    fn dim(&self) -> usize;
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
