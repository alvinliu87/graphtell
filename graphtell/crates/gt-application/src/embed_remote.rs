//! Remote embedding backend: let users bring their own embedding service (OpenAI-compatible / TEI native).
//!
//! Configured through environment variables and loaded by [`crate::embedding::resolve_recall_embedder`] when
//! `GT_EMBEDDING_BACKEND=url`:
//! - `GT_EMBEDDING_URL`      service base URL, e.g. `http://localhost:8080` or
//!   `https://api.openai.com/v1` (required)
//! - `GT_EMBEDDING_FORMAT`   `openai` (default) or `tei`
//! - `GT_EMBEDDING_API_KEY`  optional, injected as `Authorization: Bearer`
//! - `GT_EMBEDDING_MODEL`    model name for the openai format (default `text-embedding-3-small`)
//! - `GT_EMBEDDING_DIM`      optional; when absent it is probed automatically on the first request
//!
//! That way "the built-in bge-m3" and "the user's own embedding" are freely interchangeable: the release bundle
//! ships no weights, and the user either runs `graphtell model fetch` for bge-m3 or points at their own GPU
//! cluster / cloud service.
use std::sync::OnceLock;

use serde::Deserialize;

use crate::embedding::Embedder;

/// Response format of the remote service.
#[derive(Clone, Copy, Debug)]
pub enum EmbedFormat {
    /// OpenAI-compatible: `POST {base}/embeddings`, body `{model, input:[..]}`,
    /// response `{data:[{embedding:[..]}]}`. Covers vLLM / LiteLLM / ollama / Azure / OpenAI.
    OpenAi,
    /// TEI native: `POST {base}/embed`, body `{inputs:[..]}`, response `[[..],[..]]`.
    Tei,
}

/// An encoder that calls an external embedding service over HTTP.
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
    /// Built from environment variables; returns an error when `GT_EMBEDDING_URL` is missing or the first probe fails.
    pub fn load() -> Result<Self, String> {
        let base = std::env::var("GT_EMBEDDING_URL")
            .map_err(|_| "GT_EMBEDDING_BACKEND=url requires GT_EMBEDDING_URL to be set".to_string())?;
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
                .map_err(|_| "GT_EMBEDDING_DIM must be a positive integer".to_string())?,
            Err(_) => {
                // No explicit dimension: send one probe request to learn the vector length.
                let probe = Self::request(
                    &base,
                    api_key.as_deref(),
                    &model,
                    format,
                    &["__dim_probe__".to_string()],
                )?;
                if probe.is_empty() || probe[0].is_empty() {
                    return Err("remote embedding probe failed: an empty vector was returned".to_string());
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
            .map_err(|e| format!("remote embedding request failed: {e}"))?;
        if resp.status() != 200 {
            return Err(format!("remote embedding HTTP {}", resp.status()));
        }
        match format {
            EmbedFormat::OpenAi => {
                let r: OpenAiResp = resp
                    .into_json()
                    .map_err(|e| format!("failed to parse the OpenAI response: {e}"))?;
                Ok(r.data.into_iter().map(|i| i.embedding).collect())
            }
            EmbedFormat::Tei => {
                let r: Vec<Vec<f32>> = resp
                    .into_json()
                    .map_err(|e| format!("failed to parse the TEI response: {e}"))?;
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
                        "remote embedding returned {} vectors, but {} were requested",
                        v.len(),
                        texts.len()
                    );
                }
                v
            }
            Err(e) => {
                tracing::error!("remote embedding failed, falling back to a zero vector: {e}");
                vec![vec![0.0; self.dim()]; texts.len()]
            }
        }
    }
}
