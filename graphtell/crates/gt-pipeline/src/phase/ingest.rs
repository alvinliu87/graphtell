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
        let (language, role, detected_by) = marker_of(path);
        if subs.iter().any(|s| s.root_path == dir) {
            continue;
        }
        subs.push(NewSubProject {
            project_id: project.id,
            name: sub_name(project.id, &project.root_path, dir),
            root_path: dir.to_path_buf(),
            language: Language::new(language),
            role: role.to_string(),
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
