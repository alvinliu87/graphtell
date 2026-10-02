//! Application configuration.

use std::path::PathBuf;

/// Global configuration.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Data directory (where the SQLite file lives).
    pub data_dir: PathBuf,
    /// FKB (framework knowledge) directory; empty means the built-in default location.
    pub fkb_dir: Option<PathBuf>,
    /// Perspective-declaration directory; empty means the built-in default location.
    pub views_dir: Option<PathBuf>,
    /// Compliance-rule directory; empty means the built-in default location.
    pub rules_dir: Option<PathBuf>,
    /// Directory of the built Web UI (the React SPA); empty means the backend does not host the frontend and the
    /// root path `/` still falls back to the embedded standalone "prompt augmentation" page (`/compose`).
    /// A production deployment (e.g. Docker) points this at `ui/dist` and the backend hosts the SPA as well.
    pub ui_dir: Option<PathBuf>,
    /// HTTP listen address.
    pub bind: String,
    pub port: u16,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            fkb_dir: None,
            views_dir: None,
            rules_dir: None,
            ui_dir: None,
            bind: "127.0.0.1".into(),
            port: 5177,
        }
    }
}

impl AppConfig {
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("graphtell.sqlite")
    }

    /// Resolve the views directory: explicit -> environment variable -> built-in directory -> `views` under the current directory.
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

    /// Resolve the FKB directory: explicit -> environment variable -> built-in directory -> `fkb` under the current directory.
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

    /// Resolve the rules directory: explicit -> environment variable -> built-in directory -> `rules` under the current directory.
    pub fn resolve_rules_dir(&self) -> PathBuf {
        if let Some(dir) = &self.rules_dir {
            return dir.clone();
        }
        if let Ok(dir) = std::env::var("GRAPHTELL_RULES_DIR") {
            return PathBuf::from(dir);
        }
        if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
            let candidate = PathBuf::from(manifest).join("rules");
            if candidate.is_dir() {
                return candidate;
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                for up in [
                    parent.join("rules"),
                    parent.join("../rules"),
                    parent.join("../../rules"),
                ] {
                    if up.is_dir() {
                        return up;
                    }
                }
            }
        }
        PathBuf::from("rules")
    }

    /// Resolve the Web UI (React SPA) directory: explicit -> environment variable -> a directory next to the executable.
    ///
    /// `None` means unconfigured and the backend does not host the frontend (the development default, where Vite's
    /// dev server serves the frontend). A production deployment (Docker) points `GRAPHTELL_UI_DIR` or `--ui-dir`
    /// at `ui/dist`.
    pub fn resolve_ui_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.ui_dir {
            return Some(dir.clone());
        }
        if let Ok(dir) = std::env::var("GRAPHTELL_UI_DIR") {
            return Some(PathBuf::from(dir));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                for up in [
                    parent.join("ui/dist"),
                    parent.join("../ui/dist"),
                    parent.join("../../ui/dist"),
                ] {
                    if up.join("index.html").is_file() {
                        return Some(up);
                    }
                }
            }
        }
        None
    }
}
