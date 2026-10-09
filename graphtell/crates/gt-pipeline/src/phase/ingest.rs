//! P0 Ingest: determine sub-projects and the files to analyse.
//!
//! Excludes dependency directories like `vendor/` / `node_modules/`, static assets and build artifacts
//! (including Rust's `target/` and JS's `dist/` / `node_modules/`).

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    Language, NewSourceFile, NewSubProject, Project, ProjectId, SourceFile, SubProject,
};
use gt_domain::port::{
    FileScanner, FileSystem, KnowledgeProvider, Marker, MarkerProvider, ParserRegistry, ScanRequest,
    TechStackRegistry,
};
use tracing::info;

use super::exclude::{self, ExcludeDiagnostic};

/// The product of the Ingest phase.
pub struct IngestResult {
    pub sub_projects: Vec<NewSubProject>,
    pub files: Vec<NewSourceFile>,
    /// Exclusion rules that could not be resolved (a knowledge-authoring signal, not a failure).
    pub diagnostics: Vec<ExcludeDiagnostic>,
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
    fs: &dyn FileSystem,
    markers: &dyn MarkerProvider,
    techstack: &dyn TechStackRegistry,
    kb: &dyn KnowledgeProvider,
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
        let role = refine_role(&tier, dir, &Language::new(&language), techstack, fs);
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

    // Exclusions are resolved **here** and not in P3: a framework's cache / generated directories must
    // never reach the scan, let alone the parser. `resolve_for_sub` only reads manifests and probes
    // directories, which is exactly what makes it usable before anything has been parsed — and the
    // directories themselves come from the project's own configuration (see `phase::exclude`).
    let mut exclude_globs: Vec<String> = project.config.exclude_globs.clone();
    let mut diagnostics: Vec<ExcludeDiagnostic> = Vec::new();
    for sub in subs.iter_mut() {
        let resolved = exclude::resolve_for_sub(
            &project.root_path,
            &sub.root_path,
            &sub.language,
            kb,
            fs,
            parsers,
            techstack,
        );
        // The rules are relative to the sub-project root, the scan is rooted at the project root.
        let prefix = sub
            .root_path
            .strip_prefix(&project.root_path)
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .replace('\\', "/")
            .trim_matches('/')
            .to_string();
        for glob in &resolved.globs {
            exclude_globs.push(if prefix.is_empty() {
                glob.clone()
            } else {
                format!("{prefix}/{glob}")
            });
        }
        sub.facts = resolved.facts;
        diagnostics.extend(resolved.diagnostics);
        if !resolved.globs.is_empty() {
            info!("Ingest: {} exclusions: {:?}", sub.name, resolved.globs);
        }
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
        extra_excludes: exclude_globs,
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

    Ok(IngestResult { sub_projects: subs, files, diagnostics })
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
        if marker_name_matches(&m.file, name) {
            return (m.language.as_str().to_string(), m.role.clone(), m.file.clone());
        }
    }
    ("unknown".to_string(), "unknown".to_string(), name.to_string())
}

/// Match a marker name against a discovered file basename (see `gt_adapter_fs::scanner::marker_name_matches`
/// for the same convention used by marker discovery).
fn marker_name_matches(pattern: &str, name: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        let suffix = suffix.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        name.len() >= suffix.len() && name.ends_with(&suffix)
    } else {
        pattern.eq_ignore_ascii_case(name)
    }
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
fn refine_role(
    tier: &str,
    dir: &Path,
    language: &Language,
    techstack: &dyn TechStackRegistry,
    fs: &dyn FileSystem,
) -> String {
    let kind: Option<String> = match tier {
        "frontend" => refine_frontend_kind(dir, language, techstack, fs),
        "backend" => refine_backend_kind(dir).map(|k| k.to_string()),
        _ => return tier.to_string(),
    };
    match kind {
        Some(k) => format!("{}:{}", tier, k),
        None => tier.to_string(),
    }
}

/// Frontend sub-project type: directory name first, ecosystem config as fallback.
fn refine_frontend_kind(
    dir: &Path,
    language: &Language,
    techstack: &dyn TechStackRegistry,
    fs: &dyn FileSystem,
) -> Option<String> {
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
        return Some("mini-program".to_string());
    }
    if has("admin") || has("manage") || has("console") || has("dashboard") || has("cms") {
        return Some("admin".to_string());
    }
    if has("mobile")
        || has("react-native")
        || has("flutter")
        || has("ios")
        || has("android")
        || has("h5")
    {
        return Some("mobile".to_string());
    }
    // 2) Config fallback: ecosystem-specific (`package.json` / `manifest.json` for JS, …). Which package
    // names mean "mini-program" is tech-stack knowledge, so it lives in the tech-stack adapter.
    techstack
        .adapter_for(language)
        .and_then(|a| a.sub_project_kind(dir, fs))
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

    use std::sync::Mutex;

    use gt_adapter_fs::StdFileSystem;
    use gt_adapter_techstack::JsTechStackAdapter;
    use gt_domain::model::{
        ExcludeRule, FileId, FrameworkKnowledge, ProjectConfig, ProjectStatus, SubProjectId,
        SyntaxFacts,
    };
    use gt_domain::port::{
        DefaultTechStackRegistry, KnowledgeProvider, LanguageParser, MarkerProvider, ParserRegistry,
        ScanRequest, ScannedFile,
    };

    /// `refine_role` consults the tech-stack adapter as a fallback, so the tests inject the JS adapter
    /// (recognition of `react-native` / uni-app lives in `gt-adapter-techstack`, not in the kernel).
    fn js() -> (DefaultTechStackRegistry, StdFileSystem, Language) {
        (
            DefaultTechStackRegistry::new().register(Box::new(JsTechStackAdapter::new())),
            StdFileSystem::new(),
            Language::new(Language::JAVASCRIPT),
        )
    }

    fn role(tier: &str, dir: &Path) -> String {
        let (reg, fs, lang) = js();
        refine_role(tier, dir, &lang, &reg, &fs)
    }

    #[test]
    fn refines_frontend_role_by_dir_name() {
        assert_eq!(role("frontend", Path::new("/p/miniprogram")), "frontend:mini-program");
        assert_eq!(role("frontend", Path::new("/p/admin")), "frontend:admin");
        assert_eq!(role("frontend", Path::new("/p/mobile-app")), "frontend:mobile");
        assert_eq!(role("frontend", Path::new("/p/web")), "frontend");
    }

    #[test]
    fn refines_backend_role_by_dir_name() {
        assert_eq!(role("backend", Path::new("/p/admin")), "backend:admin");
        assert_eq!(role("backend", Path::new("/p/worker")), "backend:worker");
        assert_eq!(role("backend", Path::new("/p/api")), "backend");
    }

    #[test]
    fn passes_through_non_tier_roles() {
        assert_eq!(role("library", Path::new("/p/lib")), "library");
        assert_eq!(role("unknown", Path::new("/p/x")), "unknown");
    }

    /// Without a registered adapter the kernel must degrade gracefully (bare tier, no panic).
    #[test]
    fn falls_back_to_bare_tier_without_adapter() {
        let lang = Language::new(Language::JAVASCRIPT);
        let empty = DefaultTechStackRegistry::new();
        assert_eq!(
            refine_role("frontend", Path::new("/p/web"), &lang, &empty, &StdFileSystem::new()),
            "frontend"
        );
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
        assert_eq!(role("frontend", &dir), "frontend:mini-program");
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
        assert_eq!(role("frontend", &dir), "frontend");
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
        assert_eq!(role("frontend", &dir), "frontend:mobile");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------- fingerprint
    //
    // The regression this function exists for: it used to be `hash("path:file-size")`, which never read
    // the content — so "changed one line but the byte count is unchanged" went undetected, and incremental
    // updates silently missed changes while the user believed the graph was up to date.

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gt-ingest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Equal byte size, different content -> **different** fingerprint. Without this, incremental rebuilds
    /// miss real edits (worse than no incrementality, because it looks up to date).
    #[test]
    fn fingerprint_changes_when_content_changes_at_the_same_size() {
        let dir = scratch("fp");
        let fs = StdFileSystem::new();
        let p = dir.join("a.php");

        std::fs::write(&p, "$a = 1;\n").unwrap();
        let before = fingerprint(&fs, &p, "a.php:8");
        std::fs::write(&p, "$b = 1;\n").unwrap();
        let after = fingerprint(&fs, &p, "a.php:8");

        assert_eq!(
            std::fs::metadata(&p).unwrap().len(),
            8,
            "precondition: both writes must have the same byte count, otherwise this test does not test what it claims"
        );
        assert_ne!(before, after, "the same byte count but different content means a different fingerprint");

        // Reordering statements keeps the size, so a size-based hash misses it too.
        std::fs::write(&p, "$a = 1;\n$b = 2;\n").unwrap();
        let ordered = fingerprint(&fs, &p, "x");
        std::fs::write(&p, "$b = 2;\n$a = 1;\n").unwrap();
        let swapped = fingerprint(&fs, &p, "x");
        assert_ne!(ordered, swapped, "merely reordering statements must also change the fingerprint");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Identical content -> identical fingerprint, or every rebuild looks "changed".
    #[test]
    fn fingerprint_is_stable_for_identical_content() {
        let dir = scratch("fp-stable");
        let fs = StdFileSystem::new();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "same content").unwrap();
        std::fs::write(&b, "same content").unwrap();

        assert_eq!(
            fingerprint(&fs, &a, "a:12"),
            fingerprint(&fs, &b, "b:12"),
            "identical content means an identical fingerprint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unreadable / deleted files degrade to the `path:size` fallback — Ingest must not abort on them.
    #[test]
    fn fingerprint_falls_back_to_path_and_size_when_unreadable() {
        let fs = StdFileSystem::new();
        let missing = std::path::Path::new("/definitely/not/here.php");
        assert_eq!(
            fingerprint(&fs, missing, "fallback:9"),
            hash("fallback:9"),
            "when the file cannot be read it must degrade to a path:size fingerprint"
        );
    }

    // ------------------------------------------------------- assign_files
    //
    // Multi-module ownership: which sub-project a file belongs to decides which FKB applies to it, so a
    // wrong bucket silently applies the wrong framework's knowledge.

    fn sub(id: i64, root: &Path) -> SubProject {
        SubProject {
            id: SubProjectId(id),
            project_id: ProjectId::new(1),
            name: format!("sub{id}"),
            root_path: root.to_path_buf(),
            language: Language::new(Language::PHP),
            role: "backend".to_string(),
            detected_by: "composer.json".to_string(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        }
    }

    fn file(id: i64, path: &str) -> SourceFile {
        SourceFile {
            id: FileId(id),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            path: path.to_string(),
            language: Language::new(Language::PHP),
            size_bytes: 0,
            content_hash: String::new(),
        }
    }

    /// The **most specific** (longest matching) sub-project wins — otherwise a nested module falls into the
    /// outer bucket and multi-module projects collapse into one.
    #[test]
    fn file_goes_to_the_most_specific_sub_project() {
        let root = Path::new("/p");
        let subs = vec![sub(1, &root.join("app")), sub(2, &root.join("app/admin"))];
        let mut files = vec![file(1, "app/admin/Controller.php")];

        assign_files(&mut files, &subs, root);

        assert_eq!(
            files[0].sub_project_id,
            Some(SubProjectId(2)),
            "must be attributed to the more specific app/admin, not the outer app"
        );
    }

    /// The prefix match must stop at a `/` boundary: `app` must not claim `application/...`.
    #[test]
    fn prefix_match_stops_at_a_path_boundary() {
        let root = Path::new("/p");
        let subs = vec![sub(1, &root.join("app"))];

        let mut files = vec![file(1, "application/Model.php")];
        assign_files(&mut files, &subs, root);
        assert_eq!(
            files[0].sub_project_id,
            None,
            "`app` must not match `application/` (the prefix must stop at a `/` boundary)"
        );

        let mut files = vec![file(2, "app/Model.php")];
        assign_files(&mut files, &subs, root);
        assert_eq!(files[0].sub_project_id, Some(SubProjectId(1)), "files under its own directory must still be attributed to it");
    }

    /// Outside every sub-project root -> unassigned, never "nearest by proximity".
    #[test]
    fn file_outside_every_sub_project_stays_unassigned() {
        let root = Path::new("/p");
        let subs = vec![sub(1, &root.join("app"))];
        let mut files = vec![file(1, "docs/readme.md")];

        assign_files(&mut files, &subs, root);

        assert_eq!(files[0].sub_project_id, None);
    }

    /// A sub-project sitting **at the project root** (empty relative prefix) owns every file.
    #[test]
    fn root_sub_project_claims_every_file() {
        let root = Path::new("/p");
        let subs = vec![sub(1, root)];
        let mut files = vec![file(1, "anywhere/deep/File.php")];

        assign_files(&mut files, &subs, root);

        assert_eq!(files[0].sub_project_id, Some(SubProjectId(1)));
    }

    // ------------------------------------------------------- validate_root / sub_name

    #[test]
    fn validate_root_accepts_a_directory_and_rejects_the_rest() {
        let dir = scratch("root");
        assert_eq!(
            validate_root(&dir).expect("a valid directory must be accepted"),
            std::fs::canonicalize(&dir).unwrap(),
            "must return the normalised path"
        );

        let missing = dir.join("nope");
        let err = validate_root(&missing).expect_err("a non-existent path must error");
        assert!(err.to_string().contains("does not exist"), "got: {err}");

        let file = dir.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let err = validate_root(&file).expect_err("a path that is a file, not a directory, must error");
        assert!(err.to_string().contains("not a directory"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sub_name_is_root_at_the_project_root_and_dashed_below_it() {
        let root = Path::new("/p");
        assert_eq!(sub_name(ProjectId::new(1), root, root), "root");
        assert_eq!(
            sub_name(ProjectId::new(1), root, Path::new("/p/app/admin")),
            "app-admin"
        );
    }

    // ------------------------------------------------------- marker_of (pure)
    //
    // The only untested pure fn: maps a discovered marker file name to (language, tier, detected_by).
    // `detected_by` keeps the **configured** marker name (its original case), and an unknown marker
    // falls back to ("unknown", "unknown", filename) — both silent-failure-prone if the case-insensitive
    // match is ever broken.

    /// Marker name matching is ascii-case-insensitive, and `detected_by` is the configured marker name
    /// (preserving its case), not the on-disk filename.
    #[test]
    fn marker_of_matches_case_insensitively() {
        let known = vec![Marker {
            file: "composer.json".into(),
            language: Language::new("php"),
            role: "backend".into(),
        }];
        // On-disk name is upper-cased; the configured name stays lower-cased.
        assert_eq!(
            marker_of(Path::new("/x/COMPOSER.JSON"), &known),
            ("php".to_string(), "backend".to_string(), "composer.json".to_string())
        );
    }

    /// A filename that matches no known marker yields the unknown triple, with the filename as detected_by.
    #[test]
    fn marker_of_unknown_falls_back_to_unknown() {
        let known = vec![Marker {
            file: "composer.json".into(),
            language: Language::new("php"),
            role: "backend".into(),
        }];
        assert_eq!(
            marker_of(Path::new("/x/random.txt"), &known),
            (
                "unknown".to_string(),
                "unknown".to_string(),
                "random.txt".to_string()
            )
        );
    }

    // ------------------------------------------------------- run (integration, stub ports)
    //
    // The orchestration that wires markers -> sub-projects -> exclude globs -> scan -> fingerprint is only
    // exercised here. Two glue branches are otherwise never reached: the no-marker root fallback and the
    // sub-project-relative prefixing of framework exclude globs.

    struct StubScanner {
        markers: Vec<PathBuf>,
        scanned: Vec<ScannedFile>,
        last_request: Mutex<Option<ScanRequest>>,
    }
    impl FileScanner for StubScanner {
        fn scan(&self, request: &ScanRequest) -> Result<Vec<ScannedFile>> {
            *self.last_request.lock().unwrap() = Some(request.clone());
            Ok(self.scanned.clone())
        }
        fn find_markers(
            &self,
            _root: &Path,
            _names: &[&str],
            _max_depth: usize,
        ) -> Result<Vec<PathBuf>> {
            Ok(self.markers.clone())
        }
    }

    struct StubParser;
    impl LanguageParser for StubParser {
        fn language(&self) -> Language {
            Language::new("php")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["php"]
        }
        fn parse(&self, _path: &str, _source: &str) -> gt_domain::error::Result<SyntaxFacts> {
            Ok(SyntaxFacts::default())
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['\\']
        }
        fn member_separator(&self) -> &'static str {
            "::"
        }
    }

    struct StubRegistry {
        parser: StubParser,
    }
    impl ParserRegistry for StubRegistry {
        fn parser_for(&self, _: &Language) -> Option<&dyn LanguageParser> {
            Some(&self.parser)
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("php")]
        }
    }

    struct StubMarkers {
        markers: Vec<Marker>,
    }
    impl MarkerProvider for StubMarkers {
        fn markers(&self) -> Vec<Marker> {
            self.markers.clone()
        }
    }

    struct StaticKb(Vec<FrameworkKnowledge>);
    impl KnowledgeProvider for StaticKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            self.0.iter().collect()
        }
        fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
            self.0.iter().find(|fk| fk.id == id)
        }
    }

    fn project_at(root: &Path) -> Project {
        Project {
            id: ProjectId::new(1),
            name: "demo".into(),
            root_path: root.to_path_buf(),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// With no marker at all the project root becomes a single `unknown` sub-project detected via
    /// `fallback:root` — and the scan still runs.
    #[test]
    fn run_falls_back_to_root_sub_project_when_no_marker() {
        let root = scratch("run-nomarker");
        let project = project_at(&root);

        let scanner = StubScanner {
            markers: vec![],
            scanned: vec![],
            last_request: Mutex::new(None),
        };
        let parsers = StubRegistry { parser: StubParser };
        let markers = StubMarkers { markers: vec![] };
        let kb = StaticKb(vec![]);

        let res = run(
            &project,
            &scanner,
            &parsers,
            &StdFileSystem::new(),
            &markers,
            &DefaultTechStackRegistry::new(),
            &kb,
        )
        .expect("run ok");

        assert_eq!(res.sub_projects.len(), 1, "with no marker it must degrade to a single-root sub-project");
        let sub = &res.sub_projects[0];
        assert_eq!(sub.root_path, root, "the degraded sub-project root must equal the project root");
        assert_eq!(sub.role, "unknown");
        assert_eq!(sub.detected_by, "fallback:root");
        assert_eq!(sub.name, "demo");
        assert_eq!(sub.language, Language::new(Language::UNKNOWN));
        assert!(scanner.last_request.lock().unwrap().is_some(), "scan must still be called");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A framework's exclude glob (relative to the sub-project root) must be re-prefixed with the
    /// sub-project's path before reaching the scan, otherwise the cache dir is silently never excluded.
    #[test]
    fn run_prefixes_exclude_globs_with_sub_project_path() {
        let root = scratch("run-prefix");
        let sub_dir = root.join("app");
        std::fs::create_dir_all(&sub_dir).unwrap();
        // The scanned file must exist on disk so the content fingerprint can read it.
        let src = sub_dir.join("Controller.php");
        std::fs::write(&src, "<?php\n").unwrap();

        let mut project = project_at(&root);
        project.config.exclude_globs = vec!["public/**".into()];

        let scanner = StubScanner {
            markers: vec![sub_dir.join("composer.json")],
            scanned: vec![ScannedFile {
                path: src.clone(),
                relative: "app/Controller.php".into(),
                language: Language::new("php"),
                size_bytes: 6,
            }],
            last_request: Mutex::new(None),
        };
        let parsers = StubRegistry { parser: StubParser };
        let markers = StubMarkers {
            markers: vec![Marker {
                file: "composer.json".into(),
                language: Language::new("php"),
                role: "backend".into(),
            }],
        };
        // A framework that applies unconditionally to php (no manifest needed) and declares a static exclude glob.
        let kb = StaticKb(vec![FrameworkKnowledge {
            id: "tp".into(),
            display_name: "TP".into(),
            language: Language::new("php"),
            apply_without_detection: true,
            exclude_rules: vec![ExcludeRule {
                id: "cache".into(),
                glob: "runtime/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            ..Default::default()
        }]);

        let res = run(
            &project,
            &scanner,
            &parsers,
            &StdFileSystem::new(),
            &markers,
            &DefaultTechStackRegistry::new(),
            &kb,
        )
        .expect("run ok");

        assert_eq!(res.sub_projects.len(), 1, "exactly one sub-project must be recognised");
        assert_eq!(res.sub_projects[0].root_path, sub_dir);
        assert_eq!(res.sub_projects[0].role, "backend");

        let req = scanner.last_request.lock().unwrap();
        let req = req.as_ref().expect("scan invoked");
        assert!(
            req.extra_excludes.contains(&"public/**".to_string()),
            "the user-configured glob must be kept verbatim: {:?}",
            req.extra_excludes
        );
        assert!(
            req.extra_excludes.contains(&"app/runtime/**".to_string()),
            "the framework glob must be joined with the sub-project prefix: {:?}",
            req.extra_excludes
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two marker files in the *same* directory must collapse to a single sub-project (the
    /// `if subs.iter().any(|s| s.root_path == dir) { continue; }` dedup), while two markers in different
    /// directories produce two.
    #[test]
    fn run_deduplicates_markers_in_the_same_directory() {
        let root = scratch("run-dedup");
        let project = project_at(&root);
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        // Two markers resolve to the same directory -> one sub-project.
        let scanner = StubScanner {
            markers: vec![app.join("composer.json"), app.join("package.json")],
            scanned: vec![],
            last_request: Mutex::new(None),
        };
        let markers = StubMarkers {
            markers: vec![
                Marker {
                    file: "composer.json".into(),
                    language: Language::new("php"),
                    role: "backend".into(),
                },
                Marker {
                    file: "package.json".into(),
                    language: Language::new(Language::JAVASCRIPT),
                    role: "frontend".into(),
                },
            ],
        };
        let res = run(
            &project,
            &scanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &markers,
            &DefaultTechStackRegistry::new(),
            &StaticKb(vec![]),
        )
        .expect("run ok");
        assert_eq!(res.sub_projects.len(), 1, "two markers in the same directory must de-duplicate into one sub-project");
        assert_eq!(res.sub_projects[0].root_path, app);

        // Two different directories -> two sub-projects (sanity for the non-dedup path).
        let root2 = scratch("run-dedup2");
        let a = root2.join("a");
        let b = root2.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let scanner2 = StubScanner {
            markers: vec![a.join("composer.json"), b.join("package.json")],
            scanned: vec![],
            last_request: Mutex::new(None),
        };
        let res2 = run(
            &project_at(&root2),
            &scanner2,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &StubMarkers {
                markers: vec![
                    Marker {
                        file: "composer.json".into(),
                        language: Language::new("php"),
                        role: "backend".into(),
                    },
                    Marker {
                        file: "package.json".into(),
                        language: Language::new(Language::JAVASCRIPT),
                        role: "frontend".into(),
                    },
                ],
            },
            &DefaultTechStackRegistry::new(),
            &StaticKb(vec![]),
        )
        .expect("run ok");
        assert_eq!(res2.sub_projects.len(), 2, "two markers in different directories must each become a sub-project");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    /// A sub-project sitting at the project root carries an *empty* relative prefix, so a framework's
    /// exclude glob must reach the scan verbatim (the `prefix.is_empty()` branch) — never prefixed with a
    /// `root/` segment or a leading slash. The prefix test above only exercises the non-empty branch.
    #[test]
    fn run_passes_framework_glob_through_when_sub_at_root() {
        let root = scratch("run-rootglob");
        let project = project_at(&root);
        // No marker -> fallback root sub-project (language unknown). A `*` framework with an exclude glob.
        let scanner = StubScanner {
            markers: vec![],
            scanned: vec![],
            last_request: Mutex::new(None),
        };
        let kb = StaticKb(vec![FrameworkKnowledge {
            id: "tp".into(),
            display_name: "TP".into(),
            language: Language::new("*"),
            apply_without_detection: true,
            exclude_rules: vec![ExcludeRule {
                id: "cache".into(),
                glob: "runtime/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            ..Default::default()
        }]);
        let res = run(
            &project,
            &scanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &StubMarkers { markers: vec![] },
            &DefaultTechStackRegistry::new(),
            &kb,
        )
        .expect("run ok");
        assert_eq!(res.sub_projects.len(), 1, "there must be exactly one degraded root-level sub-project");
        let req = scanner.last_request.lock().unwrap();
        let req = req.as_ref().expect("scan invoked");
        assert!(
            req.extra_excludes.contains(&"runtime/**".to_string()),
            "the root-level sub-project's glob must be passed through verbatim: {:?}",
            req.extra_excludes
        );
        assert!(
            !req
                .extra_excludes
                .iter()
                .any(|g| g.contains("root/runtime") || g.starts_with('/')),
            "no root/ prefix and no leading slash: {:?}",
            req.extra_excludes
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An exclude rule whose `{value}` placeholder can never be resolved (no source, no fallback, no
    /// matching root rule) must surface as a diagnostic that `run` propagates into the result
    /// (`diagnostics.extend` was otherwise never asserted).
    #[test]
    fn run_propagates_unresolved_exclude_diagnostics() {
        let root = scratch("run-diag");
        let project = project_at(&root);
        let scanner = StubScanner {
            markers: vec![],
            scanned: vec![],
            last_request: Mutex::new(None),
        };
        let kb = StaticKb(vec![FrameworkKnowledge {
            id: "tp".into(),
            display_name: "TP".into(),
            language: Language::new("*"),
            apply_without_detection: true,
            exclude_rules: vec![ExcludeRule {
                id: "bogus".into(),
                glob: "{missing}/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            ..Default::default()
        }]);
        let res = run(
            &project,
            &scanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &StubMarkers { markers: vec![] },
            &DefaultTechStackRegistry::new(),
            &kb,
        )
        .expect("run ok");
        assert!(
            !res.diagnostics.is_empty(),
            "an unresolvable exclude rule must produce a diagnostic: {:?}",
            res.diagnostics
        );
        assert_eq!(res.diagnostics[0].code, "ExcludeRuleUnresolved");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `run` wires `fingerprint` into the produced `NewSourceFile`s: the content hash must be a genuine
    /// content hash of the scanned file on disk (not the `path:size` fallback), pinning the integration.
    #[test]
    fn run_computes_content_hash_from_disk_file() {
        let root = scratch("run-hash");
        let sub_dir = root.join("app");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let src = sub_dir.join("Controller.php");
        std::fs::write(&src, "<?php\n").unwrap();
        let project = project_at(&root);
        let scanner = StubScanner {
            markers: vec![sub_dir.join("composer.json")],
            scanned: vec![ScannedFile {
                path: src.clone(),
                relative: "app/Controller.php".into(),
                language: Language::new("php"),
                size_bytes: 6,
            }],
            last_request: Mutex::new(None),
        };
        let markers = StubMarkers {
            markers: vec![Marker {
                file: "composer.json".into(),
                language: Language::new("php"),
                role: "backend".into(),
            }],
        };
        let res = run(
            &project,
            &scanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &markers,
            &DefaultTechStackRegistry::new(),
            &StaticKb(vec![]),
        )
        .expect("run ok");
        assert_eq!(res.files.len(), 1);
        assert_eq!(
            res.files[0].content_hash,
            hash("<?php\n"),
            "content_hash must be the fingerprint of the file content, not the path:size fallback"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `refine_backend_kind` distinguishes more than `admin` / `worker`: bff/gateway and the other worker
    /// keywords (queue/job/…) must resolve to `backend:bff` / `backend:worker` — only the first two were
    /// pinned by `refines_backend_role_by_dir_name`.
    #[test]
    fn refines_backend_kind_bff_and_worker_keywords() {
        assert_eq!(role("backend", Path::new("/p/bff")), "backend:bff");
        assert_eq!(role("backend", Path::new("/p/api-gateway")), "backend:bff");
        assert_eq!(role("backend", Path::new("/p/queue")), "backend:worker");
        assert_eq!(role("backend", Path::new("/p/job-worker")), "backend:worker");
    }

    // ------------------------------------------------------- negative / degradation coverage
    //
    // The happy-path tests above exercise the won cases; these pin the branches that must *not*
    // produce anything (or must degrade gracefully), plus the keyword-mapping tables that silently
    // shrink if a keyword is ever dropped. A wrong bucket or a swallowed failure here is silent: the
    // graph just ends up missing files or wrongly attributed.

    /// A scanner whose `scan` always fails — pins that `run` propagates the error instead of panicking,
    /// so a broken filesystem / scanner does not take down the whole ingest.
    struct ErrScanner;
    impl FileScanner for ErrScanner {
        fn scan(&self, _request: &ScanRequest) -> Result<Vec<ScannedFile>> {
            Err(DomainError::infra("scan exploded"))
        }
        fn find_markers(
            &self,
            _root: &Path,
            _names: &[&str],
            _max_depth: usize,
        ) -> Result<Vec<PathBuf>> {
            Ok(vec![])
        }
    }

    /// With no sub-projects, *no* file can be attributed — never silently bucketed into a phantom root.
    #[test]
    fn assign_files_no_subs_leaves_every_file_unassigned() {
        let root = Path::new("/p");
        let mut files = vec![file(1, "app/Model.php"), file(2, "lib/Util.php")];
        assign_files(&mut files, &[], root);
        assert_eq!(
            files[0].sub_project_id, None,
            "with no sub-projects, no file can be attributed"
        );
        assert_eq!(files[1].sub_project_id, None);
    }

    /// The prefix match has an exact-equal branch (`f.path.len() == prefix.len()`): a file whose path
    /// *is* the prefix (no trailing `/`) must still match, or a top-level file would fall through.
    #[test]
    fn assign_files_exact_prefix_without_trailing_slash_matches() {
        let root = Path::new("/p");
        let subs = vec![sub(1, &root.join("app"))];
        let mut files = vec![file(1, "app")];
        assign_files(&mut files, &subs, root);
        assert_eq!(
            files[0].sub_project_id,
            Some(SubProjectId(1)),
            "a path equal to the prefix must still match"
        );
    }

    /// A failing scanner must surface as an error from `run`, not a panic.
    #[test]
    fn run_propagates_scan_errors() {
        let root = scratch("run-scanerr");
        let project = project_at(&root);
        let res = run(
            &project,
            &ErrScanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &StubMarkers { markers: vec![] },
            &DefaultTechStackRegistry::new(),
            &StaticKb(vec![]),
        );
        assert!(res.is_err(), "a failing scanner must surface as an error, not a panic");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A single scanned file that cannot be read must degrade to a `path:size` fingerprint and let the
    /// rest of Ingest proceed — never abort the whole ingest because of one unreadable file.
    #[test]
    fn run_degrades_to_path_size_hash_for_unreadable_scanned_file() {
        let root = scratch("run-unreadable");
        let sub_dir = root.join("app");
        std::fs::create_dir_all(&sub_dir).unwrap();
        // Marker present, but the scanned file is *not* written to disk.
        let project = project_at(&root);
        let scanner = StubScanner {
            markers: vec![sub_dir.join("composer.json")],
            scanned: vec![ScannedFile {
                path: sub_dir.join("Missing.php"),
                relative: "app/Missing.php".into(),
                language: Language::new("php"),
                size_bytes: 0,
            }],
            last_request: Mutex::new(None),
        };
        let markers = StubMarkers {
            markers: vec![Marker {
                file: "composer.json".into(),
                language: Language::new("php"),
                role: "backend".into(),
            }],
        };
        let res = run(
            &project,
            &scanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &markers,
            &DefaultTechStackRegistry::new(),
            &StaticKb(vec![]),
        )
        .expect("run must not abort on an unreadable file");
        assert_eq!(res.files.len(), 1);
        assert_eq!(
            res.files[0].content_hash,
            hash("app/Missing.php:0"),
            "an unreadable scanned file must degrade to a path:size fingerprint, not abort Ingest"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With no user exclude globs and no framework knowledge, nothing should be excluded — and `run`
    /// must still succeed (the `extra_excludes` base list stays empty, never `None` or panicking).
    #[test]
    fn run_keeps_empty_exclude_list_when_nothing_resolves() {
        let root = scratch("run-noexclude");
        let sub_dir = root.join("app");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let project = project_at(&root);
        let scanner = StubScanner {
            markers: vec![sub_dir.join("composer.json")],
            scanned: vec![],
            last_request: Mutex::new(None),
        };
        let markers = StubMarkers {
            markers: vec![Marker {
                file: "composer.json".into(),
                language: Language::new("php"),
                role: "backend".into(),
            }],
        };
        let res = run(
            &project,
            &scanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &markers,
            &DefaultTechStackRegistry::new(),
            &StaticKb(vec![]),
        )
        .expect("run ok");
        assert_eq!(res.sub_projects.len(), 1);
        let req = scanner.last_request.lock().unwrap();
        let req = req.as_ref().expect("scan invoked");
        assert!(
            req.extra_excludes.is_empty(),
            "with no user glob and no framework, nothing should be excluded: {:?}",
            req.extra_excludes
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `refine_backend_kind` maps a whole table of directory keywords; dropping any one is a silent
    /// regression. Pin every entry so a deletion fails loudly.
    #[test]
    fn refine_backend_kind_covers_all_worker_and_bff_keywords() {
        for kw in [
            "worker", "job", "schedule", "cron", "queue", "consumer", "daemon", "task",
        ] {
            assert_eq!(
                role("backend", &Path::new(&format!("/p/{kw}"))),
                "backend:worker",
                "backend keyword `{kw}` must map to worker"
            );
        }
        for kw in ["bff", "gateway", "aggregate"] {
            assert_eq!(
                role("backend", &Path::new(&format!("/p/{kw}"))),
                "backend:bff",
                "backend keyword `{kw}` must map to bff"
            );
        }
        assert_eq!(role("backend", Path::new("/p/admin")), "backend:admin");
    }

    /// `refine_frontend_kind` maps three keyword families; pin every entry.
    #[test]
    fn refine_frontend_kind_covers_all_keywords() {
        for kw in [
            "miniprogram", "mini-program", "miniapp", "weapp", "wxapp", "mp-weixin", "mp-alipay",
            "wechat", "alipay",
        ] {
            assert_eq!(
                role("frontend", &Path::new(&format!("/p/{kw}"))),
                "frontend:mini-program",
                "frontend keyword `{kw}` must map to mini-program"
            );
        }
        for kw in ["admin", "manage", "console", "dashboard", "cms"] {
            assert_eq!(
                role("frontend", &Path::new(&format!("/p/{kw}"))),
                "frontend:admin",
                "frontend keyword `{kw}` must map to admin"
            );
        }
        for kw in ["mobile", "react-native", "flutter", "ios", "android", "h5"] {
            assert_eq!(
                role("frontend", &Path::new(&format!("/p/{kw}"))),
                "frontend:mobile",
                "frontend keyword `{kw}` must map to mobile"
            );
        }
    }

    /// Each sub-project's framework exclusion is prefixed with *its own* relative path; two sub-projects
    /// must not share or leak each other's globs.
    #[test]
    fn run_aggregates_excludes_across_multiple_sub_projects() {
        let root = scratch("run-multisub");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let project = project_at(&root);
        let scanner = StubScanner {
            markers: vec![a.join("composer.json"), b.join("composer.json")],
            scanned: vec![],
            last_request: Mutex::new(None),
        };
        let markers = StubMarkers {
            markers: vec![Marker {
                file: "composer.json".into(),
                language: Language::new("php"),
                role: "backend".into(),
            }],
        };
        let kb = StaticKb(vec![FrameworkKnowledge {
            id: "tp".into(),
            display_name: "TP".into(),
            language: Language::new("php"),
            apply_without_detection: true,
            exclude_rules: vec![ExcludeRule {
                id: "cache".into(),
                glob: "runtime/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            ..Default::default()
        }]);
        let res = run(
            &project,
            &scanner,
            &StubRegistry { parser: StubParser },
            &StdFileSystem::new(),
            &markers,
            &DefaultTechStackRegistry::new(),
            &kb,
        )
        .expect("run ok");
        assert_eq!(
            res.sub_projects.len(), 2,
            "two directories with a marker each become two sub-projects"
        );
        let req = scanner.last_request.lock().unwrap();
        let req = req.as_ref().expect("scan invoked");
        assert!(
            req.extra_excludes.contains(&"a/runtime/**".to_string()),
            "the first sub's framework glob must be prefixed with its path: {:?}",
            req.extra_excludes
        );
        assert!(
            req.extra_excludes.contains(&"b/runtime/**".to_string()),
            "the second sub's framework glob must be prefixed independently: {:?}",
            req.extra_excludes
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `sub_name` strips the project-root prefix; when the dir is *not* under the root (defensive — a
    /// malformed sub root), it must fall back to the raw dir path rather than panicking on the strip.
    #[test]
    fn sub_name_outside_root_falls_back_to_dir_path() {
        let root = Path::new("/p");
        let outside = Path::new("/elsewhere/x/y");
        assert_eq!(
            sub_name(ProjectId::new(1), root, outside),
            "elsewhere-x-y",
            "when the dir is not under the project root, sub_name must fall back to the raw dir path"
        );
    }
}
