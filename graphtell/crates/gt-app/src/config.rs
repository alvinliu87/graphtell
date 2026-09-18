//! 应用配置。

use std::path::PathBuf;

/// 全局配置。
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// 数据目录（SQLite 文件存放位置）。
    pub data_dir: PathBuf,
    /// FKB（框架知识）目录；为空时使用内置默认位置。
    pub fkb_dir: Option<PathBuf>,
    /// 视角声明目录；为空时使用内置默认位置。
    pub views_dir: Option<PathBuf>,
    /// HTTP 监听地址。
    pub bind: String,
    pub port: u16,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            fkb_dir: None,
            views_dir: None,
            bind: "127.0.0.1".into(),
            port: 5177,
        }
    }
}

impl AppConfig {
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("graphtell.sqlite")
    }

    /// 解析视角目录：显式指定 → 环境变量 → 内置目录 → 当前目录下的 `views`。
    pub fn resolve_views_dir(&self) -> PathBuf {
        if let Some(dir) = &self.views_dir {
            return dir.clone();
        }
        if let Ok(dir) = std::env::var("GRAPHTELL_VIEWS_DIR") {
            return PathBuf::from(dir);
        }
        if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
            let candidate = PathBuf::from(manifest).join("views");
            if candidate.is_dir() {
                return candidate;
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                for up in [parent.join("views"), parent.join("../views"), parent.join("../../views")] {
                    if up.is_dir() {
                        return up;
                    }
                }
            }
        }
        PathBuf::from("views")
    }

    /// 解析 FKB 目录：显式指定 → 环境变量 → 内置目录 → 当前目录下的 `fkb`。
    pub fn resolve_fkb_dir(&self) -> PathBuf {
        if let Some(dir) = &self.fkb_dir {
            return dir.clone();
        }
        if let Ok(dir) = std::env::var("GRAPHTELL_FKB_DIR") {
            return PathBuf::from(dir);
        }
        if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
            let candidate = PathBuf::from(manifest).join("fkb");
            if candidate.is_dir() {
                return candidate;
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                for up in [parent.join("fkb"), parent.join("../fkb"), parent.join("../../fkb")] {
                    if up.is_dir() {
                        return up;
                    }
                }
            }
        }
        PathBuf::from("fkb")
    }
}
