//! 文件系统与文件扫描端口。

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::Language;

/// 文件系统只读能力。
///
/// 只暴露流水线真正需要的方法（接口隔离原则），
/// 便于测试时以内存文件系统替换。
pub trait FileSystem: Send + Sync {
    fn exists(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn len(&self, path: &Path) -> Result<u64>;
}

/// 扫描请求。
#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub root: PathBuf,
    /// 叠加在语言默认规则之上的排除 glob。
    pub extra_excludes: Vec<String>,
    /// 目标语言；为空表示全部支持的语言。
    pub languages: Vec<Language>,
    /// 「语言 → 扩展名列表」，由解析器注册表提供。
    ///
    /// 用于扩展名到语言的判定。此前扫描器自带一张写死的扩展名表，与子工程标记
    /// 文件表（`composer.json` / `go.mod` / `pyproject.toml` …）**对不上** ——
    /// 结果 Go / Python 子工程能被识别出来，却一个源文件都扫不到。
    /// 为空时回退到扫描器的内置表（保持向后兼容）。
    pub language_extensions: Vec<(String, Vec<String>)>,
}

/// 扫描得到的候选文件。
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    /// 相对扫描根的路径，`/` 分隔。
    pub relative: String,
    pub language: Language,
    pub size_bytes: u64,
}

/// 文件扫描器。
///
/// 负责排除 `vendor/`、`node_modules/`、静态资源、编译产物等，
/// 只返回「值得建图」的源文件。
pub trait FileScanner: Send + Sync {
    /// 扫描待建图的源文件。
    fn scan(&self, request: &ScanRequest) -> Result<Vec<ScannedFile>>;

    /// 在限定深度内查找特定文件名的"标记文件"（如 `composer.json`）。
    ///
    /// 用于子工程识别：一个工程根目录下可能嵌套多个技术栈子工程。
    fn find_markers(&self, root: &Path, names: &[&str], max_depth: usize) -> Result<Vec<PathBuf>>;
}
