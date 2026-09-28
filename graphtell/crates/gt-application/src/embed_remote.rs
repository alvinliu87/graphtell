//! 远程 embedding 后端：让用户自带 embedding 服务（OpenAI 兼容 / TEI 原生）。
//!
//! 通过环境变量配置，由 [`crate::embedding::resolve_recall_embedder`] 在
//! `GT_EMBEDDING_BACKEND=url` 时加载：
//! - `GT_EMBEDDING_URL`      服务基址，如 `http://localhost:8080` 或
//!   `https://api.openai.com/v1`（必填）
//! - `GT_EMBEDDING_FORMAT`   `openai`（默认）或 `tei`
//! - `GT_EMBEDDING_API_KEY`  可选，注入 `Authorization: Bearer`
//! - `GT_EMBEDDING_MODEL`    openai 格式用的模型名（默认 `text-embedding-3-small`）
//! - `GT_EMBEDDING_DIM`      可选，未给则首次请求自动探测
//!
//! 这样「内置 bge-m3」与「用户自有 embedding」可自由切换：发布包无需捆绑任何权重，
//! 用户要么 `graphtell model fetch` 拉 bge-m3，要么指向自己的 GPU 集群 / 云端服务。
use std::sync::OnceLock;

use serde::Deserialize;

use crate::embedding::Embedder;

/// 远程服务响应格式。
#[derive(Clone, Copy, Debug)]
pub enum EmbedFormat {
    /// OpenAI 兼容：`POST {base}/embeddings`，body `{model, input:[..]}`，
    /// 响应 `{data:[{embedding:[..]}]}`。覆盖 vLLM / LiteLLM / ollama / Azure / OpenAI。
    OpenAi,
    /// TEI 原生：`POST {base}/embed`，body `{inputs:[..]}`，响应 `[[..],[..]]`。
    Tei,
}

/// 通过 HTTP 调用外部 embedding 服务的编码器。
pub struct RemoteHttpEmbedder {
    base: String,
    api_key: Option<String>,
    model: String,
    format: EmbedFormat,
    dim: OnceLock<usize>,
}

#[derive(Deserialize)]
struct OpenAiResp {
    data: Vec<OpenAiItem>,
}

#[derive(Deserialize)]
struct OpenAiItem {
    embedding: Vec<f32>,
}

impl RemoteHttpEmbedder {
    /// 从环境变量构造；缺 `GT_EMBEDDING_URL` 或首次探测失败则返回错误。
    pub fn load() -> Result<Self, String> {
        let base = std::env::var("GT_EMBEDDING_URL")
            .map_err(|_| "GT_EMBEDDING_BACKEND=url 需要设置 GT_EMBEDDING_URL".to_string())?;
        let api_key = std::env::var("GT_EMBEDDING_API_KEY").ok();
        let format = match std::env::var("GT_EMBEDDING_FORMAT").as_deref() {
            Ok("tei") => EmbedFormat::Tei,
            _ => EmbedFormat::OpenAi,
        };
        let model = std::env::var("GT_EMBEDDING_MODEL").unwrap_or_else(|_| match format {
            EmbedFormat::OpenAi => "text-embedding-3-small".to_string(),
            EmbedFormat::Tei => String::new(),
        });
        let dim = match std::env::var("GT_EMBEDDING_DIM") {
            Ok(d) => d
                .parse::<usize>()
                .map_err(|_| "GT_EMBEDDING_DIM 必须是正整数".to_string())?,
            Err(_) => {
                // 未显式给维度：发一次探测请求拿向量长度。
                let probe = Self::request(
                    &base,
                    api_key.as_deref(),
                    &model,
                    format,
                    &["__dim_probe__".to_string()],
                )?;
                if probe.is_empty() || probe[0].is_empty() {
                    return Err("远程 embedding 探测失败：返回空向量".to_string());
                }
                probe[0].len()
            }
        };
        let e = RemoteHttpEmbedder {
            base,
            api_key,
            model,
            format,
            dim: OnceLock::new(),
        };
        e.dim.get_or_init(|| dim);
        Ok(e)
    }

    fn request(
        base: &str,
        api_key: Option<&str>,
        model: &str,
        format: EmbedFormat,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, String> {
        let base = base.trim_end_matches('/');
        let (url, body) = match format {
            EmbedFormat::OpenAi => (
                format!("{base}/embeddings"),
                serde_json::json!({ "model": model, "input": texts }),
            ),
            EmbedFormat::Tei => (
                format!("{base}/embed"),
                serde_json::json!({ "inputs": texts }),
            ),
        };
        let mut req = ureq::post(&url).set("Content-Type", "application/json");
        if let Some(k) = api_key {
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
        let resp = req
            .send_json(body)
            .map_err(|e| format!("远程 embedding 请求失败: {e}"))?;
        if resp.status() != 200 {
            return Err(format!("远程 embedding HTTP {}", resp.status()));
        }
        match format {
            EmbedFormat::OpenAi => {
                let r: OpenAiResp = resp
                    .into_json()
                    .map_err(|e| format!("解析 OpenAI 响应失败: {e}"))?;
                Ok(r.data.into_iter().map(|i| i.embedding).collect())
            }
            EmbedFormat::Tei => {
                let r: Vec<Vec<f32>> = resp
                    .into_json()
                    .map_err(|e| format!("解析 TEI 响应失败: {e}"))?;
                Ok(r)
            }
        }
    }
}

impl Embedder for RemoteHttpEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_batch(&[text.to_string()])
            .pop()
            .unwrap_or_default()
    }

    fn dim(&self) -> usize {
        *self.dim.get().unwrap_or(&0)
    }

    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if texts.is_empty() {
            return Vec::new();
        }
        match Self::request(
            &self.base,
            self.api_key.as_deref(),
            &self.model,
            self.format,
            texts,
        ) {
            Ok(v) => {
                if v.len() != texts.len() {
                    tracing::warn!(
                        "远程 embedding 返回 {} 个向量，但请求了 {} 个",
                        v.len(),
                        texts.len()
                    );
                }
                v
            }
            Err(e) => {
                tracing::error!("远程 embedding 失败，返回零向量兜底: {e}");
                vec![vec![0.0; self.dim()]; texts.len()]
            }
        }
    }
}
