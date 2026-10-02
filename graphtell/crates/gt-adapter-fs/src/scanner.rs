//! Directory scanning: exclude dependency directories, static assets and build artifacts.

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::Language;
use gt_domain::port::{FileScanner, ScanRequest, ScannedFile};
use tracing::debug;

/// Directories any tech stack should exclude (dependencies / caches / build artifacts / version control).
pub const COMMON_EXCLUDE_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    "out",
    ".idea",
    ".vscode",
    ".cache",
    "coverage",
    "__pycache__",
    ".gradle",
    "runtime",
    "unpackage",
];

/// Static assets and non-source directories.
pub const ASSET_DIRS: &[&str] = &["static", "assets", "public", "images", "img", "fonts", "medias"];

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
    /// Extra excluded directories (layered on top of the common rules).
    extra_excludes: Vec<String>,
}

impl WalkDirScanner {
    pub fn new(extra_excludes: Vec<String>) -> Self {
        Self { extra_excludes }
    }
}

impl Default for WalkDirScanner {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl WalkDirScanner {
    fn is_excluded_dir(&self, name: &str) -> bool {
        if COMMON_EXCLUDE_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name)) {
            return true;
        }
        if ASSET_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name)) {
            return true;
        }
        self.extra_excludes
            .iter()
            .any(|e| e.trim_end_matches("/**").eq_ignore_ascii_case(name))
    }

    fn is_excluded_file(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with('.') && lower.ends_with("-lock") {
            return true;
        }
        match lower.rsplit('.').next() {
            Some(ext) => ASSET_EXTENSIONS.contains(&ext),
            None => false,
        }
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
            if entry.file_type().is_dir() {
                if entry.depth() > 0
                    && self.is_excluded_dir(&entry.file_name().to_string_lossy())
                {
                    walker.skip_current_dir();
                }
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if self.is_excluded_file(&name) {
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
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
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
                if entry.depth() > 0
                    && self.is_excluded_dir(&entry.file_name().to_string_lossy())
                {
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
