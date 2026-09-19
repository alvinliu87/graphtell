//! P0 Ingest：确定子工程与待分析文件。
//!
//! 排除 `vendor/` / `node_modules/` 等依赖目录、静态资源、编译产物
//! （含 Rust 的 `target/` 与 JS 的 `dist/`、`node_modules/`）。

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    Language, NewSourceFile, NewSubProject, Project, ProjectId, SourceFile, SubProject,
};
use gt_domain::port::{FileScanner, ParserRegistry, ScanRequest};
use tracing::info;

/// 子工程标记文件 → (语言, 角色)。
pub const MARKERS: &[(&str, &str, &str)] = &[
    ("composer.json", "php", "backend"),
    ("package.json", "javascript", "frontend"),
    ("pom.xml", "java", "backend"),
    ("build.gradle", "java", "backend"),
    ("Cargo.toml", "rust", "backend"),
    ("go.mod", "go", "backend"),
    ("pyproject.toml", "python", "backend"),
];

/// Ingest 阶段的产物。
pub struct IngestResult {
    pub sub_projects: Vec<NewSubProject>,
    pub files: Vec<NewSourceFile>,
}

/// 执行 Ingest。
pub fn run(
    project: &Project,
    scanner: &dyn FileScanner,
    parsers: &dyn ParserRegistry,
) -> Result<IngestResult> {
    let markers: Vec<&str> = MARKERS.iter().map(|m| m.0).collect();
    let found = scanner.find_markers(&project.root_path, &markers, 4)?;

    let mut subs: Vec<NewSubProject> = Vec::new();
    for path in &found {
        let Some(dir) = path.parent() else { continue };
        let (language, tier, detected_by) = marker_of(path);
        if subs.iter().any(|s| s.root_path == dir) {
            continue;
        }
        // 在「前端 / 后端」之上，再按目录名细化出子工程**类型**
        // （小程序 / 管理后台 / 移动端 / API / Worker …），角色串形如 `frontend:admin`。
        let role = refine_role(tier, dir);
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

    // 没有任何标记时，把工程根当作单一子工程
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

    // 扫描全部源文件，再按路径前缀归属到最具体的子工程
    // 「语言 → 扩展名」取自解析器注册表，保证它与子工程标记文件表同源：
    // 不会出现 `go.mod` 能识别子工程、`.go` 文件却被扫不进来的错位。
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
        "Ingest: {} 个子工程，{} 个源文件",
        subs.len(),
        scanned.len()
    );

    let mut files = Vec::with_capacity(scanned.len());
    for f in scanned {
        files.push(NewSourceFile {
            project_id: project.id,
            sub_project_id: None,
            path: f.relative.clone(),
            language: f.language,
            size_bytes: f.size_bytes,
            content_hash: hash(&format!("{}:{}", f.relative, f.size_bytes)),
        });
    }

    Ok(IngestResult { sub_projects: subs, files })
}

/// 把文件分配到最具体的子工程。
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

fn marker_of(path: &Path) -> (&'static str, &'static str, String) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    for (marker, language, role) in MARKERS {
        if marker.eq_ignore_ascii_case(name) {
            return (language, role, marker.to_string());
        }
    }
    ("unknown", "unknown", name.to_string())
}

/// 在 `frontend` / `backend` 这两层之外，进一步识别子工程**类型**，让图例与过滤能区分
/// 「小程序 / 管理后台 / 移动端 / API / Worker」等，而不是把所有前端压成一个蓝点。
///
/// 识别顺序：先看**目录名**（快、零 IO），命中即用；目录命名不规范时再读
/// `package.json` / `manifest.json` 等配置兜底（例如 uni-app 只有在 `manifest.json`
/// 里声明了 `mp-weixin` 等小程序目标才算「小程序」，否则只是普通 web）。
///
/// 返回形如 `tier:kind` 的角色串（`frontend:admin`、`backend:worker` …）；
/// 无法识别具体类型时回落为裸 `frontend` / `backend`，与旧数据兼容。
/// 其它 tier（`library` / `unknown`）原样返回。
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

/// 前端子工程类型：目录名优先，配置兜底。
fn refine_frontend_kind(dir: &Path) -> Option<&'static str> {
    let rel = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let full = dir.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let has = |kw: &str| rel.contains(kw) || full.contains(kw);

    // 1) 目录名命中优先
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
    // 2) 配置兜底：uni-app / React Native 等
    detect_frontend_kind_via_config(dir)
}

/// 后端子工程类型：目录名命中即可（后端类型差异主要靠目录区分，暂不读配置）。
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

/// 读 `package.json` 等配置识别前端框架，弥补「目录命名不规范」导致的漏判。主要覆盖：
/// - React Native → `mobile`
/// - uni-app：若直接依赖 `uni-mp-*` 编译包，或 `manifest.json` 声明了 mp-* 小程序目标，则 `mini-program`；
///   否则只是普通 web，回落为默认 `frontend`（不强行打小程序标签）。
fn detect_frontend_kind_via_config(dir: &Path) -> Option<&'static str> {
    let pkg = read_json(&dir.join("package.json"))?;
    let deps = collect_deps(&pkg);

    // React Native → 移动端
    if deps.iter().any(|d| d == "react-native" || d.contains("react-native")) {
        return Some("mobile");
    }

    // uni-app 系
    let is_uni = deps.iter().any(|d| d.contains("uni-app") || d.contains("@dcloudio/uni"));
    if is_uni {
        // 直接依赖了具体小程序平台编译包
        if deps.iter().any(|d| d.contains("uni-mp-")) {
            return Some("mini-program");
        }
        // 否则看 manifest.json 是否声明了 mp-* 目标
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

/// 合并 dependencies / devDependencies / peerDependencies 的 key 列表。
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

/// manifest.json（uni-app）里是否声明了任意小程序编译目标（mp-weixin / mp-alipay …）。
fn manifest_has_mp_target(mani: &serde_json::Value) -> bool {
    let lower = mani.to_string().to_ascii_lowercase();
    [
        "mp-weixin", "mp-alipay", "mp-toutiao", "mp-baidu", "mp-qq", "mp-360",
        "mp-kuaishou", "mp-jd", "mp-lark", "mp-xhs", "mp-qsn",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// 安全地读取并解析 JSON 文件；不存在 / 解析失败返回 None。
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

/// 校验工程根路径合法。
pub fn validate_root(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        return Err(DomainError::InvalidArgument(format!(
            "路径不存在: {}",
            path.display()
        )));
    }
    if !path.is_dir() {
        return Err(DomainError::InvalidArgument(format!(
            "不是目录: {}",
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
        // 未声明 mp-* 目标 → 普通 web，回落为裸 frontend
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
