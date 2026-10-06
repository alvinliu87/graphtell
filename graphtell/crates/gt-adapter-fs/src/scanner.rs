//! Directory scanning: exclude dependency directories, static assets and build artifacts.
//!
//! Exclusion is **glob-based** and layered:
//! * [`DEFAULT_EXCLUDE_GLOBS`] / [`ASSET_GLOBS`] — the tech-stack-agnostic baseline (dependency,
//!   cache, build and asset directories), expressed as patterns just like everything else;
//! * `ScanRequest.extra_excludes` — project-level globs plus what P0 resolved from framework
//!   knowledge (`gt-pipeline`'s `phase::exclude`), e.g. `crmeb/runtime/**` rendered from the project's
//!   own `autoload.psr-4`. Matching by **path** rather than by directory name is what makes those
//!   rules expressible at all: `public/static/**` or `storage/logs/**` have no name to compare.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use gt_domain::error::{DomainError, Result};
use gt_domain::model::Language;
use gt_domain::port::{FileScanner, ScanRequest, ScannedFile};
use tracing::{debug, warn};

/// Directories that are never source code in any tech stack: dependencies, caches, VCS metadata,
/// build artifacts.
pub const DEFAULT_EXCLUDE_GLOBS: &[&str] = &[
    "**/.git/**",
    "**/.svn/**",
    "**/.hg/**",
    "**/node_modules/**",
    "**/vendor/**",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/out/**",
    "**/.idea/**",
    "**/.vscode/**",
    "**/.cache/**",
    "**/coverage/**",
    "**/__pycache__/**",
    "**/.gradle/**",
    "**/runtime/**",
    "**/unpackage/**",
];

/// Static assets: the directories that hold images / fonts / media.
pub const ASSET_GLOBS: &[&str] = &[
    "**/static/**",
    "**/assets/**",
    "**/public/**",
    "**/images/**",
    "**/img/**",
    "**/fonts/**",
    "**/medias/**",
];

/// Binary / static asset extensions.
pub const ASSET_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "bmp", "mp4", "mp3", "avi", "mov", "wav",
    "zip", "gz", "tar", "rar", "7z", "pdf", "doc", "docx", "xls", "xlsx", "ttf", "woff", "woff2",
    "eot", "map", "lock",
];

/// Determine the language from the caller-supplied "language -> extensions" map (same source as the parser registry).
fn language_for_ext(map: &[(String, Vec<String>)], ext: &str) -> Option<Language> {
    map.iter()
        .find(|(_, exts)| exts.iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .map(|(lang, _)| Language::new(lang))
}

/// Infer the language from the extension (**fallback**: used when the caller supplies no map).
///
/// This table only guarantees that "every scanned file belongs to some known language", so scanning never drifts
/// away from the parser registry; the real authority is [`ParserRegistry::language_for_extension`].
pub fn language_of_extension(ext: &str) -> Option<Language> {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "php" | "phtml" | "php5" | "php7" | "php8" | "inc" => Some(Language::new(Language::PHP)),
        "js" | "jsx" | "mjs" | "cjs" | "vue" => Some(Language::new(Language::JAVASCRIPT)),
        "ts" | "tsx" => Some(Language::new(Language::TYPESCRIPT)),
        "java" => Some(Language::new(Language::JAVA)),
        "rs" => Some(Language::new(Language::RUST)),
        // The languages below already appear in the sub-project marker table (`go.mod` / `pyproject.toml` / …) but were
        // missing from the extension table — the omission made those sub-projects detected yet left with zero source files.
        "go" => Some(Language::new("go")),
        "py" | "pyi" => Some(Language::new("python")),
        "kt" | "kts" => Some(Language::new("kotlin")),
        "rb" => Some(Language::new("ruby")),
        "cs" => Some(Language::new("csharp")),
        "scala" | "sc" => Some(Language::new("scala")),
        "c" | "h" => Some(Language::new("c")),
        "cpp" | "cc" | "cxx" | "hpp" => Some(Language::new("cpp")),
        _ => None,
    }
}

pub struct WalkDirScanner {
    /// Every pattern in effect: the built-in defaults plus the caller's `extra_excludes`.
    excludes: GlobSet,
}

impl WalkDirScanner {
    pub fn new(extra_excludes: Vec<String>) -> Self {
        Self { excludes: build_set(extra_excludes) }
    }

    /// Whether `relative` (a `/`-separated path against the scan root) is excluded.
    ///
    /// Directories are additionally probed with a synthetic child (`a/b` -> `a/b/__probe__`): a pattern
    /// written `dir/**` means "the whole subtree", and without the probe it would match nothing for the
    /// directory itself, so the walker would descend into a tree it was told to skip.
    fn is_excluded(&self, relative: &str, extra: Option<&GlobSet>) -> bool {
        if relative.is_empty() {
            return false;
        }
        let probe = format!("{relative}/__probe__");
        let hit = |set: &GlobSet| set.is_match(relative) || set.is_match(&probe);
        hit(&self.excludes) || extra.map_or(false, hit)
    }
}

impl Default for WalkDirScanner {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// Compile the exclusion patterns. An invalid pattern is dropped with a warning rather than aborting
/// the scan — one typo in a project's config must not cost the whole graph.
fn build_set(extra_excludes: Vec<String>) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    // Case-insensitive on purpose: `Vendor/` / `DIST/` are the same mistake in every ecosystem, and a
    // scan exclusion that fails to fire costs a whole directory of noise.
    let add = |builder: &mut GlobSetBuilder, pattern: &str| {
        match GlobBuilder::new(pattern).case_insensitive(true).build() {
            Ok(g) => {
                builder.add(g);
            }
            Err(e) => warn!("ignoring an invalid exclude glob `{pattern}`: {e}"),
        }
    };
    for pattern in DEFAULT_EXCLUDE_GLOBS.iter().chain(ASSET_GLOBS.iter()) {
        add(&mut builder, pattern);
    }
    for pattern in &extra_excludes {
        add(&mut builder, pattern);
    }
    builder.build().unwrap_or_else(|e| {
        warn!("exclude glob set failed to build ({e}); falling back to the empty set");
        GlobSet::empty()
    })
}

/// `path` relative to `root`, `/`-separated — the form every glob is matched against.
fn relative_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_asset_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with('.') && lower.ends_with("-lock") {
        return true;
    }
    match lower.rsplit('.').next() {
        Some(ext) => ASSET_EXTENSIONS.contains(&ext),
        None => false,
    }
}

impl FileScanner for WalkDirScanner {
    fn scan(&self, request: &ScanRequest) -> Result<Vec<ScannedFile>> {
        let mut out = Vec::new();
        let root = &request.root;
        if !root.is_dir() {
            return Err(DomainError::InvalidArgument(format!(
                "project root does not exist or is not a directory: {}",
                root.display()
            )));
        }

        // Per-scan exclusions: what P0 resolved from framework knowledge plus the project's own globs.
        // They are known **only here** — the scanner is constructed once per process, long before any
        // scan, so they cannot live in the set `new()` builds; carrying them on the request is the whole
        // point of `ScanRequest.extra_excludes`.
        let extra = if request.extra_excludes.is_empty() {
            None
        } else {
            Some(build_set(request.extra_excludes.clone()))
        };

        // **Must be sorted by file name**: `readdir` order depends on the filesystem and can differ between runs.
        // Ingestion order in turn decides a batch of P2 "first come, first served" results (which same-named class
        // registers first in `by_fqn`, whose FQN the global `imports` symbol table records); a change of order makes
        // the whole graph drift slightly (measured: two builds of the same CRMEB differed by 57 edges), and view
        // output jitters with it.
        let mut walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walker.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    debug!("skipping an inaccessible entry: {e}");
                    continue;
                }
            };
            let relative = relative_of(root, entry.path());
            if entry.file_type().is_dir() {
                if entry.depth() > 0 && self.is_excluded(&relative, extra.as_ref()) {
                    walker.skip_current_dir();
                }
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if is_asset_file(&name) || self.is_excluded(&relative, extra.as_ref()) {
                continue;
            }
            let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            // Prefer the "language -> extensions" map from the parser registry: it shares a source with the sub-project
            // marker table, so "a Go sub-project is detected but no .go file is scanned" cannot happen.
            let language = match language_for_ext(&request.language_extensions, &ext)
                .or_else(|| language_of_extension(&ext))
            {
                Some(l) => l,
                None => continue,
            };
            if !request.languages.is_empty()
                && !request
                    .languages
                    .iter()
                    .any(|l| l.as_str() == language.as_str())
            {
                continue;
            }
            let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size_bytes > 4 * 1024 * 1024 {
                // Skip very large files (usually generated artifacts)
                continue;
            }
            out.push(ScannedFile {
                path: entry.path().to_path_buf(),
                relative,
                language,
                size_bytes,
            });
        }
        out.sort_by(|a, b| a.relative.cmp(&b.relative));
        Ok(out)
    }

    fn find_markers(&self, root: &Path, names: &[&str], max_depth: usize) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        if !root.is_dir() {
            return Ok(out);
        }
        let mut walker = walkdir::WalkDir::new(root)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter();
        while let Some(entry) = walker.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().is_dir() {
                if entry.depth() > 0 && self.is_excluded(&relative_of(root, entry.path()), None) {
                    walker.skip_current_dir();
                }
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                out.push(entry.path().to_path_buf());
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext_lang(ext: &str) -> Option<String> {
        language_of_extension(ext).map(|l| l.as_str().to_string())
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gt-fs-scan-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- extension -> language tables ----

    /// The fallback table must cover every language that has a sub-project marker: an entry missing here
    /// leaves a detected sub-project with **zero** source files (the go / py / kt / rb / cs / scala / c / cpp
    /// group was added for exactly that bug).
    #[test]
    fn language_of_extension_covers_every_marker_language() {
        for (ext, want) in [
            ("php", "php"), ("phtml", "php"), ("php5", "php"), ("php7", "php"), ("php8", "php"), ("inc", "php"),
            ("js", "javascript"), ("jsx", "javascript"), ("mjs", "javascript"), ("cjs", "javascript"), ("vue", "javascript"),
            ("ts", "typescript"), ("tsx", "typescript"),
            ("java", "java"), ("rs", "rust"),
            ("go", "go"), ("py", "python"), ("pyi", "python"),
            ("kt", "kotlin"), ("kts", "kotlin"), ("rb", "ruby"), ("cs", "csharp"),
            ("scala", "scala"), ("sc", "scala"), ("c", "c"), ("h", "c"),
            ("cpp", "cpp"), ("cc", "cpp"), ("cxx", "cpp"), ("hpp", "cpp"),
        ] {
            assert_eq!(ext_lang(ext), Some(want.to_string()), "extension `{ext}` must map to {want}");
        }
        assert_eq!(ext_lang("xyz"), None, "an unknown extension must return None");
        assert_eq!(ext_lang(""), None);
    }

    #[test]
    fn language_of_extension_is_case_insensitive() {
        assert_eq!(ext_lang("PHP"), Some("php".to_string()));
        assert_eq!(ext_lang("PY"), Some("python".to_string()));
        assert_eq!(ext_lang("Tsx"), Some("typescript".to_string()));
    }

    /// The caller-supplied map (parser registry) wins and matches case-insensitively; a miss falls through
    /// so the caller can chain the built-in table.
    #[test]
    fn language_for_ext_prefers_the_supplied_map() {
        let map = vec![("go".to_string(), vec!["go".to_string()])];
        assert_eq!(
            language_for_ext(&map, "go").map(|l| l.as_str().to_string()),
            Some("go".to_string())
        );
        assert_eq!(
            language_for_ext(&map, "GO").map(|l| l.as_str().to_string()),
            Some("go".to_string()),
            "case-insensitive"
        );
        assert!(language_for_ext(&map, "rs").is_none(), "an extension missing from the table falls through to the fallback table");
    }

    // ---- exclusion ----

    #[test]
    fn is_excluded_matches_the_default_globs() {
        let s = WalkDirScanner::default();
        assert!(s.is_excluded("vendor/foo.php", None));
        assert!(s.is_excluded("node_modules/x/y.js", None));
        assert!(s.is_excluded("public/logo.png", None), "static asset directories must be excluded too");
        assert!(s.is_excluded("app/runtime/cache.txt", None));
        assert!(!s.is_excluded("app/Controller.php", None), "ordinary source must not be excluded");
        assert!(!s.is_excluded("", None), "an empty relative path passes straight through (the root)");
    }

    /// A directory is probed with a synthetic child: `dir/**` must exclude the directory itself, otherwise the
    /// walker descends into a tree it was told to skip.
    #[test]
    fn is_excluded_probes_directories_with_a_synthetic_child() {
        let s = WalkDirScanner::default();
        assert!(
            s.is_excluded("vendor", None),
            "the directory itself must be hit by `**/vendor/**` (through the synthesised __probe__ child)"
        );
        assert!(s.is_excluded("app/vendor", None));
        assert!(!s.is_excluded("vendorish", None), "a prefix must not over-match (it is not a substring match)");
    }

    #[test]
    fn is_excluded_is_case_insensitive() {
        let s = WalkDirScanner::default();
        assert!(s.is_excluded("Vendor/x.php", None), "`Vendor/` and `vendor/` are the same trap");
        assert!(s.is_excluded("DIST/a.js", None));
    }

    /// Project globs are **request-scoped**: they must fire only when the per-scan set is passed in, never
    /// leak into the process-wide scanner built by `new()`.
    ///
    /// `storage/logs/**` is used because it is not already covered by the built-ins (unlike e.g. `runtime`,
    /// which `**/runtime/**` already excludes everywhere).
    #[test]
    fn is_excluded_honors_extra_globs_from_the_request() {
        let s = WalkDirScanner::default();
        let extra = build_set(vec!["storage/logs/**".to_string()]);
        assert!(s.is_excluded("storage/logs/app.log", Some(&extra)));
        assert!(!s.is_excluded("storage/app/Controller.php", Some(&extra)));
        // Without the per-scan set the project glob must not fire (it is request-scoped, not global).
        assert!(!s.is_excluded("storage/logs/app.log", None));
    }

    /// One typo'd glob in a project config must not cost the whole scan: it is dropped with a warning and
    /// the built-in patterns still apply.
    #[test]
    fn build_set_survives_an_invalid_glob() {
        let s = WalkDirScanner::new(vec!["[unclosed".to_string()]);
        assert!(s.is_excluded("vendor/a.php", None), "after a bad pattern is dropped the default excludes must still apply");
        assert!(!s.is_excluded("app/A.php", None));
    }

    // ---- path helpers ----

    #[test]
    fn relative_of_strips_root_and_normalizes_separators() {
        assert_eq!(relative_of(Path::new("/root"), Path::new("/root/a/b.php")), "a/b.php");
        assert_eq!(
            relative_of(Path::new("/root"), Path::new("/root/a\\b.php")),
            "a/b.php",
            "Windows separators must be normalised to `/` (globs always match on `/`)"
        );
        // Not under the root: fall back to the path itself rather than panicking.
        assert_eq!(relative_of(Path::new("/other"), Path::new("/root/a.php")), "/root/a.php");
    }

    #[test]
    fn is_asset_file_detects_assets_and_lockfiles() {
        assert!(is_asset_file("logo.PNG"), "case-insensitive");
        assert!(is_asset_file("archive.zip"));
        assert!(is_asset_file("font.woff2"));
        assert!(is_asset_file("composer.lock"));
        assert!(is_asset_file(".composer-lock"), "a dot-file with the -lock suffix counts as a lock file");
        assert!(!is_asset_file("Controller.php"));
        assert!(!is_asset_file("app.js"));
        assert!(!is_asset_file("Makefile"), "a file with no extension must not be misjudged");
    }

    // ---- scan (real temp tree) ----

    fn tree() -> PathBuf {
        let root = tmp("tree");
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("vendor/dep")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("app/A.php"), b"<?php").unwrap();
        std::fs::write(root.join("app/D.js"), b"let a=1").unwrap();
        std::fs::write(root.join("vendor/dep/B.php"), b"<?php").unwrap();
        std::fs::write(root.join("node_modules/x/C.js"), b"let b=1").unwrap();
        std::fs::write(root.join("public/logo.png"), b"png").unwrap();
        std::fs::write(root.join("README.md"), b"# hi").unwrap();
        root
    }

    #[test]
    fn scan_skips_excluded_dirs_assets_and_unknown_extensions() {
        let root = tree();
        let files = WalkDirScanner::default()
            .scan(&ScanRequest {
                root: root.clone(),
                extra_excludes: vec![],
                languages: vec![],
                language_extensions: vec![],
            })
            .unwrap();
        let rel: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rel, vec!["app/A.php", "app/D.js"], "only real source may remain: {rel:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Output must be sorted: ingestion order decides P2 "first come, first served" results, and an unstable
    /// order makes the whole graph drift between runs (measured: two CRMEB builds differed by 57 edges).
    #[test]
    fn scan_output_is_sorted_and_deterministic() {
        let root = tmp("sorted");
        std::fs::create_dir_all(root.join("z")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        for p in ["a/b.php", "z/c.php", "m.php", "a/a.php"] {
            std::fs::write(root.join(p), b"<?php").unwrap();
        }
        let files = WalkDirScanner::default()
            .scan(&ScanRequest {
                root: root.clone(),
                extra_excludes: vec![],
                languages: vec![],
                language_extensions: vec![],
            })
            .unwrap();
        let rel: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        let mut sorted = rel.clone();
        sorted.sort();
        assert_eq!(rel, sorted, "the output must be sorted by relative path: {rel:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_respects_the_language_filter() {
        let root = tree();
        let files = WalkDirScanner::default()
            .scan(&ScanRequest {
                root: root.clone(),
                extra_excludes: vec![],
                languages: vec![Language::new(Language::PHP)],
                language_extensions: vec![],
            })
            .unwrap();
        let rel: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rel, vec!["app/A.php"], "the language filter must keep only php: {rel:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_skips_oversized_files() {
        let root = tmp("big");
        std::fs::write(root.join("small.php"), b"<?php").unwrap();
        std::fs::write(root.join("huge.php"), vec![b'a'; 4 * 1024 * 1024 + 1]).unwrap();
        let files = WalkDirScanner::default()
            .scan(&ScanRequest {
                root: root.clone(),
                extra_excludes: vec![],
                languages: vec![],
                language_extensions: vec![],
            })
            .unwrap();
        let rel: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rel, vec!["small.php"], "files over 4MB must be skipped: {rel:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_rejects_a_root_that_is_not_a_directory() {
        let r = WalkDirScanner::default().scan(&ScanRequest {
            root: PathBuf::from("/no/such/scan/root"),
            extra_excludes: vec![],
            languages: vec![],
            language_extensions: vec![],
        });
        assert!(matches!(r, Err(DomainError::InvalidArgument(_))), "a non-existent root directory must error");
    }

    // ---- find_markers ----

    #[test]
    fn find_markers_respects_depth_and_exclusions() {
        let root = tmp("markers");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::write(root.join("composer.json"), b"{}").unwrap();
        std::fs::write(root.join("sub/composer.json"), b"{}").unwrap();
        std::fs::write(root.join("vendor/composer.json"), b"{}").unwrap();

        let found = WalkDirScanner::default()
            .find_markers(&root, &["composer.json"], 1)
            .unwrap();
        assert_eq!(found.len(), 1, "max_depth=1 must hit only the one under the root (vendor is excluded)");
        assert_eq!(found[0], root.join("composer.json"));

        let deeper = WalkDirScanner::default()
            .find_markers(&root, &["composer.json"], 2)
            .unwrap();
        assert_eq!(deeper.len(), 2, "max_depth=2 must also hit the one under sub/ (vendor still excluded)");

        let upper = WalkDirScanner::default()
            .find_markers(&root, &["COMPOSER.JSON"], 1)
            .unwrap();
        assert_eq!(upper.len(), 1, "marker name matching must be case-insensitive");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_markers_returns_empty_for_a_missing_root() {
        let found = WalkDirScanner::default()
            .find_markers(Path::new("/no/such/marker/root"), &["composer.json"], 3)
            .unwrap();
        assert!(found.is_empty(), "a missing directory returns empty instead of erroring");
    }

    // ---- scan paths the existing tests never exercise ----

    /// P0 resolves a framework's cache / generated directories and hands them over as
    /// `ScanRequest.extra_excludes`. `is_excluded` is covered for this, but the `scan` path that **builds**
    /// the request-scoped set is not — if that wiring broke, every resolved cache directory would be parsed.
    /// `storage/logs/**` is used because the built-ins do not already exclude it.
    #[test]
    fn scan_applies_request_scoped_exclude_globs() {
        let root = tmp("scan-extra");
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("storage/logs")).unwrap();
        std::fs::create_dir_all(root.join("storage/app")).unwrap();
        std::fs::write(root.join("app/A.php"), b"<?php").unwrap();
        std::fs::write(root.join("storage/app/B.php"), b"<?php").unwrap();
        // A *scannable* file inside the excluded directory (a `.log` would be dropped anyway as unknown).
        std::fs::write(root.join("storage/logs/Generated.php"), b"<?php").unwrap();

        let req = |extra: Vec<String>| ScanRequest {
            root: root.clone(),
            extra_excludes: extra,
            languages: vec![],
            language_extensions: vec![],
        };

        let plain = WalkDirScanner::default().scan(&req(vec![])).unwrap();
        let rel: Vec<&str> = plain.iter().map(|f| f.relative.as_str()).collect();
        assert!(
            rel.contains(&"storage/logs/Generated.php"),
            "without the exclude rule it would be scanned (otherwise this test does not test what it claims): {rel:?}"
        );

        let filtered = WalkDirScanner::default()
            .scan(&req(vec!["storage/logs/**".to_string()]))
            .unwrap();
        let rel2: Vec<&str> = filtered.iter().map(|f| f.relative.as_str()).collect();
        assert!(
            !rel2.contains(&"storage/logs/Generated.php"),
            "the request-level glob must take effect in scan: {rel2:?}"
        );
        assert!(
            rel2.contains(&"storage/app/B.php"),
            "other directories sharing the prefix must not be hit: {rel2:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Language resolution **inside `scan`**: the caller's "language -> extensions" map (parser registry)
    /// wins, and an extension it does not know falls through to the built-in table — otherwise a stack
    /// registered in the parser registry would silently lose every file whose extension the map omits.
    #[test]
    fn scan_prefers_the_registry_map_and_falls_back_to_the_builtin_table() {
        let root = tmp("scan-map");
        std::fs::write(root.join("A.php"), b"<?php").unwrap();
        std::fs::write(root.join("B.js"), b"let a=1").unwrap();

        // The map claims `php` for a private language: it must win over the built-in `php`.
        let files = WalkDirScanner::default()
            .scan(&ScanRequest {
                root: root.clone(),
                extra_excludes: vec![],
                languages: vec![],
                language_extensions: vec![("mylang".to_string(), vec!["php".to_string()])],
            })
            .unwrap();
        let got: Vec<(&str, &str)> = files
            .iter()
            .map(|f| (f.relative.as_str(), f.language.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![("A.php", "mylang"), ("B.js", "javascript")],
            "extensions in the table are judged by it, others fall back to the built-in table: {got:?}"
        );
        // Ingestion needs the absolute path and the size.
        assert_eq!(files[0].path, root.join("A.php"));
        assert_eq!(files[0].size_bytes, 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The limit is `> 4 MiB`, not `>=`: a file of exactly 4 MiB is still source.
    #[test]
    fn scan_keeps_a_file_at_the_size_limit() {
        let root = tmp("limit");
        let limit = 4 * 1024 * 1024;
        std::fs::write(root.join("exact.php"), vec![b'a'; limit]).unwrap();
        std::fs::write(root.join("over.php"), vec![b'a'; limit + 1]).unwrap();

        let files = WalkDirScanner::default()
            .scan(&ScanRequest {
                root: root.clone(),
                extra_excludes: vec![],
                languages: vec![],
                language_extensions: vec![],
            })
            .unwrap();
        let rel: Vec<&str> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rel, vec!["exact.php"], "exactly 4MiB is kept, only larger files are dropped: {rel:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `find_markers` has no `ScanRequest`: it consults the scanner's **own** set, so a project glob handed
    /// to `scan()` cannot hide a sub-project marker, while one handed to `new()` does apply.
    #[test]
    fn find_markers_uses_only_the_scanners_own_excludes() {
        let root = tmp("markers-own");
        std::fs::create_dir_all(root.join("skipme")).unwrap();
        std::fs::write(root.join("composer.json"), b"{}").unwrap();
        std::fs::write(root.join("skipme/composer.json"), b"{}").unwrap();

        let plain = WalkDirScanner::default()
            .find_markers(&root, &["composer.json"], 3)
            .unwrap();
        assert_eq!(plain.len(), 2, "the default excludes do not contain skipme: {plain:?}");

        let scoped = WalkDirScanner::new(vec!["skipme/**".to_string()])
            .find_markers(&root, &["composer.json"], 3)
            .unwrap();
        assert_eq!(scoped.len(), 1, "excludes passed at construction time must apply: {scoped:?}");
        assert_eq!(scoped[0], root.join("composer.json"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
