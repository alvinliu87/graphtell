//! P0 Ingest: determine sub-projects and the files to analyse.
//!
//! Excludes dependency directories like `vendor/` / `node_modules/`, static assets and build artifacts
//! (including Rust's `target/` and JS's `dist/` / `node_modules/`).

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    Language, NewSourceFile, NewSubProject, Project, ProjectId, SourceFile, SubProject,
};
use gt_domain::port::{FileScanner, Marker, MarkerProvider, ParserRegistry, ScanRequest};
use tracing::info;

/// The product of the Ingest phase.
pub struct IngestResult {
    pub sub_projects: Vec<NewSubProject>,
    pub files: Vec<NewSourceFile>,
}

/// Compute the file fingerprint.
///
/// # Why it must be a **content** hash
///
/// Early on this was `hash("path:file-size")` — it never read the file content.
/// So "changed one line but the byte count is unchanged" (`a = 1` -> `b = 1`, swapping statement order,
/// changing a string whose length happens to match) **went undetected**.
/// That makes any incremental update based on this fingerprint silently miss changes — more dangerous than no
/// incrementality, because the user would believe the graph is up to date.
///
/// Files that cannot be read (deleted / no permission / binary) degrade to a `path:size` fingerprint,
/// so Ingest never aborts because of a few individual files.
pub fn fingerprint(
    fs: &dyn gt_domain::port::FileSystem,
    path: &Path,
    fallback: &str,
) -> String {
    match fs.read_to_string(path) {
        Ok(text) => hash(&text),
        Err(_) => hash(fallback),
    }
}

/// Run Ingest.
pub fn run(
    project: &Project,
    scanner: &dyn FileScanner,
    parsers: &dyn ParserRegistry,
    fs: &dyn gt_domain::port::FileSystem,
    markers: &dyn MarkerProvider,
) -> Result<IngestResult> {
    // Sub-project markers come from the tech-stack registration (`MarkerProvider`), not from a hard-coded
    // kernel table: which manifest file marks which language is tech-stack knowledge.
    let known = markers.markers();
    let marker_names: Vec<&str> = known.iter().map(|m| m.file.as_str()).collect();
    let found = scanner.find_markers(&project.root_path, &marker_names, 4)?;

    let mut subs: Vec<NewSubProject> = Vec::new();
    for path in &found {
        let Some(dir) = path.parent() else { continue };
        let (language, tier, detected_by) = marker_of(path, &known);
        if subs.iter().any(|s| s.root_path == dir) {
            continue;
        }
        // On top of "frontend / backend", refine the sub-project **type** from the directory name
        // (mini-program / admin console / mobile / API / Worker …), so the role string looks like `frontend:admin`.
        let role = refine_role(&tier, dir);
        subs.push(NewSubProject {
            project_id: project.id,
            name: sub_name(project.id, &project.root_path, dir),
            root_path: dir.to_path_buf(),
            language: Language::new(language),
            role,
            detected_by: detected_by.to_string(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        });
    }

    // With no marker at all, treat the project root as a single sub-project
    if subs.is_empty() {
        subs.push(NewSubProject {
            project_id: project.id,
            name: project.name.clone(),
            root_path: project.root_path.clone(),
            language: Language(Language::UNKNOWN.to_string()),
            role: "unknown".to_string(),
            detected_by: "fallback:root".to_string(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        });
    }

    let language_extensions: Vec<(String, Vec<String>)> = parsers
        .supported_languages()
        .into_iter()
        .filter_map(|l| {
            parsers.parser_for(&l).map(|p| {
                (
                    l.as_str().to_string(),
                    p.extensions().iter().map(|e| e.to_string()).collect(),
                )
            })
        })
        .collect();
    let request = ScanRequest {
        root: project.root_path.clone(),
        extra_excludes: project.config.exclude_globs.clone(),
        languages: Vec::new(),
        language_extensions,
    };
    let scanned = scanner.scan(&request)?;
    info!(
        "Ingest: {} sub-projects, {} source files",
        subs.len(),
        scanned.len()
    );

    let mut files = Vec::with_capacity(scanned.len());
    for f in scanned {
        let fallback = format!("{}:{}", f.relative, f.size_bytes);
        let abs = project.root_path.join(&f.relative);
        files.push(NewSourceFile {
            project_id: project.id,
            sub_project_id: None,
            path: f.relative.clone(),
            language: f.language,
            size_bytes: f.size_bytes,
            // A genuine content hash: only this detects "changed but same size"
            content_hash: fingerprint(fs, &abs, &fallback),
        });
    }

    Ok(IngestResult { sub_projects: subs, files })
}

/// Assign a file to the most specific sub-project.
pub fn assign_files(files: &mut [SourceFile], subs: &[SubProject], root: &Path) {
    let roots: Vec<(usize, String)> = subs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                i,
                s.root_path
                    .strip_prefix(root)
                    .unwrap_or(&s.root_path)
                    .to_string_lossy()
                    .replace('\\', "/")
                    .trim_matches('/')
                    .to_string(),
            )
        })
        .collect();

    for f in files.iter_mut() {
        let mut best: Option<(usize, usize)> = None;
        for (idx, prefix) in &roots {
            let matches = if prefix.is_empty() {
                true
            } else {
                f.path.starts_with(prefix.as_str())
                    && (f.path.len() == prefix.len()
                        || f.path.as_bytes().get(prefix.len()) == Some(&b'/'))
            };
            if matches {
                let score = prefix.len();
                if best.map(|(_, s)| score >= s).unwrap_or(true) {
                    best = Some((*idx, score));
                }
            }
        }
        f.sub_project_id = best.map(|(i, _)| subs[i].id);
    }
}

/// Resolve a discovered marker file to `(language, tier, detected_by)` using the injected markers.
fn marker_of(path: &Path, known: &[Marker]) -> (String, String, String) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    for m in known {
        if m.file.eq_ignore_ascii_case(name) {
            return (m.language.as_str().to_string(), m.role.clone(), m.file.clone());
        }
    }
    ("unknown".to_string(), "unknown".to_string(), name.to_string())
}

/// Beyond the `frontend` / `backend` tiers, further recognise the sub-project **type**, so the legend and filters
/// can tell apart "mini-program / admin console / mobile / API / Worker" instead of compressing every frontend into one blue dot.
///
/// Recognition order: look at the **directory name** first (fast, zero IO) and use it on a hit; when the directory
/// naming is irregular, fall back to reading `package.json` / `manifest.json` and other config (e.g. uni-app only
/// counts as a "mini-program" if `manifest.json` declares a mini-program target like `mp-weixin`, otherwise it is
/// just ordinary web).
///
/// Returns a role string of the form `tier:kind` (`frontend:admin`, `backend:worker` …); when a concrete type cannot
/// be recognised it falls back to bare `frontend` / `backend`, keeping backward compatibility with old data.
/// Other tiers (`library` / `unknown`) are returned as-is.
fn refine_role(tier: &str, dir: &Path) -> String {
    let kind: Option<&str> = match tier {
        "frontend" => refine_frontend_kind(dir),
        "backend" => refine_backend_kind(dir),
        _ => return tier.to_string(),
    };
    match kind {
        Some(k) => format!("{}:{}", tier, k),
        None => tier.to_string(),
    }
}

/// Frontend sub-project type: directory name first, config as fallback.
fn refine_frontend_kind(dir: &Path) -> Option<&'static str> {
    let rel = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let full = dir.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let has = |kw: &str| rel.contains(kw) || full.contains(kw);

    // 1) Directory-name hit wins first
    if has("miniprogram")
        || has("mini-program")
        || has("miniapp")
        || has("weapp")
        || has("wxapp")
        || has("mp-weixin")
        || has("mp-alipay")
        || has("wechat")
        || has("alipay")
    {
        return Some("mini-program");
    }
    if has("admin") || has("manage") || has("console") || has("dashboard") || has("cms") {
        return Some("admin");
    }
    if has("mobile")
        || has("react-native")
        || has("flutter")
        || has("ios")
        || has("android")
        || has("h5")
    {
        return Some("mobile");
    }
    // 2) Config fallback: uni-app / React Native etc.
    detect_frontend_kind_via_config(dir)
}

/// Backend sub-project type: a directory-name hit suffices (backend type differences are mostly directory-based, config not read for now).
fn refine_backend_kind(dir: &Path) -> Option<&'static str> {
    let rel = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let full = dir.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let has = |kw: &str| rel.contains(kw) || full.contains(kw);

    if has("worker")
        || has("job")
        || has("schedule")
        || has("cron")
        || has("queue")
        || has("consumer")
        || has("daemon")
        || has("task")
    {
        return Some("worker");
    }
    if has("bff") || has("gateway") || has("aggregate") {
        return Some("bff");
    }
    if has("admin") {
        return Some("admin");
    }
    None
}

/// Read `package.json` and similar config to recognise the frontend framework, covering misses caused by
/// "irregular directory naming". Mainly covers:
/// - React Native -> `mobile`
/// - uni-app: if it directly depends on a `uni-mp-*` compiler package, or `manifest.json` declares an mp-* mini-program target, then `mini-program`;
///   otherwise it is just ordinary web and falls back to the default `frontend` (no forced mini-program tag).
fn detect_frontend_kind_via_config(dir: &Path) -> Option<&'static str> {
    let pkg = read_json(&dir.join("package.json"))?;
    let deps = collect_deps(&pkg);

    // React Native -> mobile
    if deps.iter().any(|d| d == "react-native" || d.contains("react-native")) {
        return Some("mobile");
    }

    // uni-app family
    let is_uni = deps.iter().any(|d| d.contains("uni-app") || d.contains("@dcloudio/uni"));
    if is_uni {
        // Directly depends on a concrete mini-program-platform compiler package
        if deps.iter().any(|d| d.contains("uni-mp-")) {
            return Some("mini-program");
        }
        // Otherwise check whether manifest.json declares an mp-* target
        for m in [dir.join("manifest.json"), dir.join("src").join("manifest.json")] {
            if let Some(mani) = read_json(&m) {
                if manifest_has_mp_target(&mani) {
                    return Some("mini-program");
                }
            }
        }
    }
    None
}

/// Merge the key lists of dependencies / devDependencies / peerDependencies.
fn collect_deps(pkg: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for field in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = pkg.get(field).and_then(|v| v.as_object()) {
            for k in obj.keys() {
                out.push(k.clone());
            }
        }
    }
    out
}

/// Whether `manifest.json` (uni-app) declares any mini-program compiler target (mp-weixin / mp-alipay …).
fn manifest_has_mp_target(mani: &serde_json::Value) -> bool {
    let lower = mani.to_string().to_ascii_lowercase();
    [
        "mp-weixin", "mp-alipay", "mp-toutiao", "mp-baidu", "mp-qq", "mp-360",
        "mp-kuaishou", "mp-jd", "mp-lark", "mp-xhs", "mp-qsn",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Safely read and parse a JSON file; return None when missing / parse fails.
fn read_json(path: &Path) -> Option<serde_json::Value> {
    let s = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

fn sub_name(_project: ProjectId, root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    let s = rel.to_string_lossy().replace('\\', "/").trim_matches('/').to_string();
    if s.is_empty() {
        "root".to_string()
    } else {
        s.replace('/', "-")
    }
}

fn hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())
}

/// Validate that the project root path is legal.
pub fn validate_root(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        return Err(DomainError::InvalidArgument(format!(
            "path does not exist: {}",
            path.display()
        )));
    }
    if !path.is_dir() {
        return Err(DomainError::InvalidArgument(format!(
            "not a directory: {}",
            path.display()
        )));
    }
    std::fs::canonicalize(path).map_err(DomainError::infra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refines_frontend_role_by_dir_name() {
        assert_eq!(
            refine_role("frontend", Path::new("/p/miniprogram")),
            "frontend:mini-program"
        );
        assert_eq!(refine_role("frontend", Path::new("/p/admin")), "frontend:admin");
        assert_eq!(
            refine_role("frontend", Path::new("/p/mobile-app")),
            "frontend:mobile"
        );
        assert_eq!(refine_role("frontend", Path::new("/p/web")), "frontend");
    }

    #[test]
    fn refines_backend_role_by_dir_name() {
        assert_eq!(refine_role("backend", Path::new("/p/admin")), "backend:admin");
        assert_eq!(refine_role("backend", Path::new("/p/worker")), "backend:worker");
        assert_eq!(refine_role("backend", Path::new("/p/api")), "backend");
    }

    #[test]
    fn passes_through_non_tier_roles() {
        assert_eq!(refine_role("library", Path::new("/p/lib")), "library");
        assert_eq!(refine_role("unknown", Path::new("/p/x")), "unknown");
    }

    #[test]
    fn detects_uni_app_mini_program_from_manifest() {
        let dir = std::env::temp_dir().join(format!("refine_mp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"@dcloudio/uni-app":"^3.0.0","@dcloudio/uni-mp-weixin":"^3.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"mp-weixin":{"appid":"wx123"},"mp-alipay":{}}"#,
        )
        .unwrap();
        assert_eq!(refine_role("frontend", &dir), "frontend:mini-program");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uni_app_without_mp_target_stays_web() {
        let dir = std::env::temp_dir().join(format!("refine_web_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"@dcloudio/uni-app":"^3.0.0","@dcloudio/uni-h5":"^3.0.0"}}"#,
        )
        .unwrap();
        // No mp-* target declared -> ordinary web, fall back to bare frontend
        assert_eq!(refine_role("frontend", &dir), "frontend");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_react_native_as_mobile() {
        let dir = std::env::temp_dir().join(format!("refine_rn_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"react-native":"^0.72.0","react":"^18.2.0"}}"#,
        )
        .unwrap();
        assert_eq!(refine_role("frontend", &dir), "frontend:mobile");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
