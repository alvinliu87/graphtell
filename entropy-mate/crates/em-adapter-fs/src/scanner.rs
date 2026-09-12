//! 目录扫描：排除依赖目录、静态资源与编译产物。

use std::path::{Path, PathBuf};

use em_domain::error::{DomainError, Result};
use em_domain::model::Language;
use em_domain::port::{FileScanner, ScanRequest, ScannedFile};
use tracing::debug;

/// 任何技术栈都应该排除的目录（依赖 / 缓存 / 构建产物 / 版本控制）。
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

/// 静态资源与非源码目录。
pub const ASSET_DIRS: &[&str] = &["static", "assets", "public", "images", "img", "fonts", "medias"];

/// 二进制 / 静态资源扩展名。
pub const ASSET_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "bmp", "mp4", "mp3", "avi", "mov", "wav",
    "zip", "gz", "tar", "rar", "7z", "pdf", "doc", "docx", "xls", "xlsx", "ttf", "woff", "woff2",
    "eot", "map", "lock",
];

/// 按扩展名推断语言（不依赖解析器注册表，供扫描器独立使用）。
pub fn language_of_extension(ext: &str) -> Option<Language> {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "php" | "phtml" | "php5" | "php7" | "php8" | "inc" => Some(Language::new(Language::PHP)),
        "js" | "jsx" | "mjs" | "cjs" | "vue" => Some(Language::new(Language::JAVASCRIPT)),
        "ts" | "tsx" => Some(Language::new(Language::TYPESCRIPT)),
        "java" => Some(Language::new(Language::JAVA)),
        "rs" => Some(Language::new(Language::RUST)),
        _ => None,
    }
}

pub struct WalkDirScanner {
    /// 额外的排除目录（叠加在通用规则之上）。
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
                "工程根目录不存在或不是目录: {}",
                root.display()
            )));
        }

        let mut walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
        while let Some(entry) = walker.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    debug!("跳过无法访问的条目: {e}");
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
            let language = match language_of_extension(&ext) {
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
                // 跳过超大文件（通常是生成物）
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
