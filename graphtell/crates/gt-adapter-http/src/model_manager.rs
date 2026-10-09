//! Model download + backend-mode management for the HTTP API.
//!
//! Two concerns:
//! 1. **One-click download**: the backend downloads a **pre-converted** `bge-m3-safetensors.zip`
//!    bundle directly in Rust (no Python on the host) and unzips it into `models/bge-m3-safetensors/`.
//!    The maintainer publishes the bundle once to GitHub Releases; the release binary ships no weights
//!    and the host needs nothing but network access. Override the URL with `GT_BGE_DOWNLOAD_URL`.
//! 2. **Backend mode**: `local` / `url` / `hash` (plus `auto`), persisted to `data/model_config.json`
//!    so a choice survives restart, and applied at runtime via [`gt_application::RecallService::set_semantic_embedder`]
//!    (no server restart required).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use gt_application::embedding::{self, ModelBackendConfig};
use gt_application::RecallService;

use crate::dto::ApiResponse;

/// Default download URL for the pre-converted bge-m3 safetensors bundle.
///
/// The maintainer publishes `bge-m3-safetensors.zip` to GitHub Releases (see `tools/make_bge_release.py`).
/// The bundle must contain `model.safetensors`, `config.json`, `tokenizer.json` (plus optional
/// `tokenizer_config.json`, `special_tokens_map.json`, `sentencepiece.bpe.model`) — a single
/// top-level directory (e.g. `bge-m3-safetensors/`) is automatically stripped on extraction.
/// fp16 weights are fine (candle upcasts to f32 on load), so the default release uses fp16 to
/// halve the ~2.2GB f32 file down to ~1.1GB. Override with the `GT_BGE_DOWNLOAD_URL` env var.
pub const DEFAULT_BGE_DOWNLOAD_URL: &str =
    "https://github.com/graphtell/graphtell/releases/download/bge-m3/bge-m3-safetensors.zip";

/// The download URL, honouring `GT_BGE_DOWNLOAD_URL`.
fn bge_download_url() -> String {
    std::env::var("GT_BGE_DOWNLOAD_URL").unwrap_or_else(|_| DEFAULT_BGE_DOWNLOAD_URL.to_string())
}

/// Live state of a (possibly running) model download, surfaced to the UI for a progress view.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DownloadState {
    /// Whether a download is currently in progress.
    pub active: bool,
    /// Phase: `downloading` | `unzipping` | `done` | `error`.
    pub phase: String,
    /// 0..1 progress; `-1.0` means indeterminate (stage-based only).
    pub progress: f32,
    /// Accumulated log lines from the download script.
    pub log: String,
    /// Set when the download failed.
    pub error: Option<String>,
}

/// Where the model-config file lives (relative to the server's working directory).
pub const MODEL_CONFIG_PATH: &str = "data/model_config.json";

/// Read the persisted model backend config, if any.
pub fn load_model_config() -> ModelBackendConfig {
    match std::fs::read_to_string(MODEL_CONFIG_PATH) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => ModelBackendConfig::default(),
    }
}

/// Persist the model backend config.
pub fn save_model_config(mode: &str, url: Option<String>) {
    let cfg = ModelBackendConfig {
        mode: mode.to_string(),
        url,
    };
    if let Some(parent) = Path::new(MODEL_CONFIG_PATH).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(&cfg) {
        let _ = std::fs::write(MODEL_CONFIG_PATH, s);
    }
}

/// Whether local bge-m3 weights already exist on disk (so the UI can show "ready").
pub fn local_weights_present() -> bool {
    let dir = std::env::var("GT_BGE_MODEL")
        .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
    Path::new(&dir).join("model.safetensors").exists()
}

/// Request body for switching backend mode.
#[derive(Deserialize)]
pub struct SetBackendRequest {
    /// `local` | `url` | `hash` | `auto`.
    pub mode: String,
    /// Required only for `url` mode.
    #[serde(default)]
    pub url: Option<String>,
}

/// Apply a backend mode now: build the encoder for that mode and hot-swap it into the recall service,
/// then persist the choice. Returns the resulting status payload.
pub fn apply_backend_mode(
    recall: &Arc<RecallService>,
    req: &SetBackendRequest,
) -> ApiResponse<BackendStatus> {
    let mode = req.mode.trim();
    if !matches!(mode, "local" | "url" | "hash" | "auto") {
        return ApiResponse::failure(format!("unknown backend mode: {mode}"));
    }
    if (mode == "url" || mode == "remote")
        && req.url.as_ref().map_or(true, |u| u.trim().is_empty())
    {
        return ApiResponse::failure("url mode requires a non-empty `url`");
    }
    let url = if mode == "url" || mode == "remote" {
        req.url.clone()
    } else {
        None
    };
    let (emb, _info, _dim) = embedding::resolve_backend(mode, url.clone());
    recall.set_semantic_embedder(emb);
    save_model_config(mode, url);
    ApiResponse::success(BackendStatus {
        local_weights_present: local_weights_present(),
        backend_mode: mode.to_string(),
        backend_url: req.url.clone().filter(|_| mode == "url" || mode == "remote"),
        embedding_backend: embedding::embedding_backend_info(),
        embedding_dim: embedding::embedding_dim(),
    })
}

/// Payload returned by the model status + backend endpoints.
#[derive(Clone, Debug, Serialize)]
pub struct BackendStatus {
    pub local_weights_present: bool,
    pub backend_mode: String,
    pub backend_url: Option<String>,
    pub embedding_backend: String,
    pub embedding_dim: usize,
}

/// GET handler: current model + backend status.
pub fn model_status(_recall: &Arc<RecallService>, download: &Arc<Mutex<DownloadState>>) -> ApiResponse<ModelStatusDto> {
    let cfg = load_model_config();
    let dl = download.lock().unwrap().clone();
    ApiResponse::success(ModelStatusDto {
        local_weights_present: local_weights_present(),
        backend_mode: cfg.mode.clone(),
        backend_url: if cfg.mode == "url" || cfg.mode == "remote" {
            cfg.url.clone()
        } else {
            None
        },
        embedding_backend: embedding::embedding_backend_info(),
        embedding_dim: embedding::embedding_dim(),
        download: dl,
    })
}

/// Response DTO for `/api/models/status`.
#[derive(Clone, Debug, Serialize)]
pub struct ModelStatusDto {
    pub local_weights_present: bool,
    pub backend_mode: String,
    pub backend_url: Option<String>,
    pub embedding_backend: String,
    pub embedding_dim: usize,
    pub download: DownloadState,
}

/// POST handler: start a one-click download (no-op if already running). Returns immediately; the UI
/// polls `/api/models/status` for progress.
///
/// The bundle is fetched directly in Rust (no host Python) from [`bge_download_url`] and unzipped into
/// the `GT_BGE_MODEL` directory, after which the semantic encoder is hot-swapped in.
pub fn start_model_download(
    recall: &Arc<RecallService>,
    download: &Arc<Mutex<DownloadState>>,
) -> ApiResponse<ModelStatusDto> {
    {
        let mut s = download.lock().unwrap();
        if s.active {
            return ApiResponse::failure("a model download is already in progress");
        }
        s.active = true;
        s.phase = "downloading".to_string();
        s.progress = -1.0;
        s.log.clear();
        s.error = None;
    }
    let recall_clone = Arc::clone(recall);
    let download_clone = Arc::clone(download);

    thread::spawn(move || {
        let url = bge_download_url();
        // Resolve the destination directory (mirrors `local_weights_present`): GT_BGE_MODEL or default.
        let models_dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        let dst_dir = PathBuf::from(&models_dir);
        if let Some(parent) = dst_dir.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return finish_error(&download_clone, &format!("cannot create {parent:?}: {e}"));
            }
        }

        // Stage 1: download the zip to a temp file with progress.
        let tmp_zip = PathBuf::from("data").join(".bge-m3.zip");
        if let Some(parent) = tmp_zip.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        {
            let mut s = download_clone.lock().unwrap();
            s.phase = "downloading".to_string();
            s.progress = -1.0;
            s.log.push_str(&format!("GET {url}\n"));
        }
        if let Err(e) = download_bge_bundle(&url, &tmp_zip, &download_clone) {
            return finish_error(&download_clone, &e);
        }

        // Stage 2: unzip into the destination directory.
        {
            let mut s = download_clone.lock().unwrap();
            s.phase = "unzipping".to_string();
            s.progress = -1.0;
            s.log.push_str("unzipping…\n");
        }
        if let Err(e) = extract_zip_into(&tmp_zip, &dst_dir) {
            let _ = std::fs::remove_file(&tmp_zip);
            return finish_error(&download_clone, &format!("failed to unzip bundle: {e}"));
        }
        let _ = std::fs::remove_file(&tmp_zip);

        // Stage 3: point the loader at the weights and hot-swap the encoder.
        std::env::set_var("GT_BGE_MODEL", &models_dir);
        let (emb, info, _dim) = embedding::resolve_backend("local", None);
        recall_clone.set_semantic_embedder(emb);
        save_model_config("local", None);

        let mut s = download_clone.lock().unwrap();
        s.phase = "done".to_string();
        s.progress = 1.0;
        s.active = false;
        s.log.push_str(&format!("\n[ok] semantic backend is now: {info}\n"));
    });

    model_status(recall, download)
}

/// Download `url` to `dest`, streaming with progress written into `state`.
fn download_bge_bundle(
    url: &str,
    dest: &Path,
    state: &Arc<Mutex<DownloadState>>,
) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(3600))
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("download request failed: {e}"))?;
    let total: Option<u64> = resp.header("Content-Length").and_then(|s| s.parse().ok());
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest)
        .map_err(|e| format!("cannot create {dest:?}: {e}"))?;
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("download read error: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("write error: {e}"))?;
        downloaded += n as u64;
        if let Some(t) = total {
            if t > 0 {
                let mut s = state.lock().unwrap();
                s.progress = (downloaded as f32 / t as f32).clamp(0.0, 1.0);
            }
        }
    }
    Ok(())
}

/// Extract a zip archive into `dest`, stripping a single common top-level directory if every
/// file entry lives under exactly one such directory (so both `bge-m3-safetensors/model.safetensors`
/// and bare `model.safetensors` layouts unzip to the same place).
fn extract_zip_into(zip_path: &Path, dest: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // Detect a single common top-level directory to strip.
    let mut top: Option<String> = None;
    let mut all_under_one = true;
    for i in 0..archive.len() {
        let name = match archive.by_index(i) {
            Ok(e) => e.name().to_string(),
            Err(_) => {
                all_under_one = false;
                break;
            }
        };
        if name.ends_with('/') {
            continue;
        }
        let first = match name.split('/').next() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                all_under_one = false;
                break;
            }
        };
        match &top {
            Some(t) if *t == first => {}
            Some(_) => {
                all_under_one = false;
                break;
            }
            None => top = Some(first),
        }
    }
    let strip = if all_under_one { top } else { None };

    std::fs::create_dir_all(dest)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        let rel = match &strip {
            Some(p) => name.strip_prefix(p).and_then(|s| s.strip_prefix('/')).unwrap_or(&name),
            None => &name,
        };
        if rel.is_empty() {
            continue;
        }
        let out_path = dest.join(rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Mark the download as failed and record the message.
fn finish_error(state: &Arc<Mutex<DownloadState>>, msg: &str) {
    let mut s = state.lock().unwrap();
    s.phase = "error".to_string();
    s.active = false;
    s.error = Some(msg.to_string());
    s.log.push_str(&format!("\n[error] {msg}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// Build an uncompressed zip in memory from `(name, bytes)` entries.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'static, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, data) in entries {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(data).unwrap();
            }
            zw.finish().unwrap();
        }
        buf
    }

    /// Write `zip_bytes` to a unique temp dir and extract into `<dir>/out`; returns the `out` path.
    fn extract_to_tmp(zip_bytes: &[u8]) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("gt_zip_test_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zip_path = base.join("bundle.zip");
        std::fs::write(&zip_path, zip_bytes).unwrap();
        let dest = base.join("out");
        extract_zip_into(&zip_path, &dest).unwrap();
        dest
    }

    #[test]
    fn extract_strips_single_top_level_dir() {
        let zip = make_zip(&[
            ("bge-m3-safetensors/model.safetensors", b"WEIGHTS"),
            ("bge-m3-safetensors/config.json", b"{}"),
        ]);
        let dest = extract_to_tmp(&zip);
        assert!(dest.join("model.safetensors").exists());
        assert!(dest.join("config.json").exists());
        // The single common top-level directory must be stripped, not preserved.
        assert!(!dest.join("bge-m3-safetensors").exists());
        assert_eq!(std::fs::read(dest.join("model.safetensors")).unwrap(), b"WEIGHTS");
    }

    #[test]
    fn extract_bare_files_are_not_stripped() {
        let zip = make_zip(&[
            ("model.safetensors", b"W"),
            ("config.json", b"{}"),
        ]);
        let dest = extract_to_tmp(&zip);
        assert!(dest.join("model.safetensors").exists());
        assert!(dest.join("config.json").exists());
    }

    #[test]
    fn extract_preserves_multiple_top_level_dirs() {
        // Two distinct top-level dirs → no stripping, structure preserved.
        let zip = make_zip(&[("a/x", b"1"), ("b/y", b"2")]);
        let dest = extract_to_tmp(&zip);
        assert!(dest.join("a").join("x").exists());
        assert!(dest.join("b").join("y").exists());
    }

    #[test]
    fn extract_keeps_nested_directories() {
        let zip = make_zip(&[("bge-m3-safetensors/sub/mod.safetensors", b"W")]);
        let dest = extract_to_tmp(&zip);
        assert!(dest.join("sub").join("mod.safetensors").exists());
        assert!(!dest.join("bge-m3-safetensors").exists());
    }

    #[test]
    fn bge_download_url_honours_env_override() {
        std::env::set_var("GT_BGE_DOWNLOAD_URL", "https://example.com/x.zip");
        assert_eq!(bge_download_url(), "https://example.com/x.zip");
        std::env::remove_var("GT_BGE_DOWNLOAD_URL");
        assert_eq!(bge_download_url(), DEFAULT_BGE_DOWNLOAD_URL);
    }

    #[test]
    fn local_weights_present_after_extraction() {
        // Ties the extraction target to the same path `local_weights_present` reads.
        let zip = make_zip(&[("bge-m3-safetensors/model.safetensors", b"W")]);
        let dest = extract_to_tmp(&zip);
        std::env::set_var("GT_BGE_MODEL", &dest);
        assert!(local_weights_present());
        std::env::remove_var("GT_BGE_MODEL");
    }

    /// Spin up a one-shot localhost HTTP server that answers a single GET with `body`.
    fn serve_once(body: &[u8], status_line: &'static str) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_vec();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let resp = format!("{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (addr, handle)
    }

    #[test]
    fn download_bge_bundle_writes_file_with_progress() {
        let zip = make_zip(&[("model.safetensors", b"WEIGHTS")]);
        let (addr, server) = serve_once(&zip, "HTTP/1.1 200 OK");
        let tmp = std::env::temp_dir().join(format!("gt_dl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let out = tmp.join("got.zip");
        let state = Arc::new(Mutex::new(DownloadState::default()));

        download_bge_bundle(&format!("http://{addr}/bundle.zip"), &out, &state)
            .expect("download should succeed");

        assert_eq!(std::fs::read(&out).unwrap(), zip);
        assert_eq!(state.lock().unwrap().progress, 1.0);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn download_bge_bundle_reports_http_error() {
        let (addr, server) = serve_once(&[], "HTTP/1.1 404 Not Found");
        let tmp = std::env::temp_dir().join(format!("gt_dl_test_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let out = tmp.join("got.zip");
        let state = Arc::new(Mutex::new(DownloadState::default()));

        let res = download_bge_bundle(&format!("http://{addr}/nope"), &out, &state);
        assert!(res.is_err(), "a non-2xx response must surface as an error");
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
