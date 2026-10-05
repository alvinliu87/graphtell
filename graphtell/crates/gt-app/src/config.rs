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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // env vars are process-global; serialize the tests that mutate them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn explicit_dir_wins_over_everything() {
        // The integration suite always passes explicit dirs, so this is the path it exercises — pin it so a
        // future refactor that ignores the explicit override regresses loudly.
        let cfg = AppConfig {
            views_dir: Some(PathBuf::from("/explicit/views")),
            ..AppConfig::default()
        };
        assert_eq!(cfg.resolve_views_dir(), PathBuf::from("/explicit/views"));
        let cfg = AppConfig {
            fkb_dir: Some(PathBuf::from("/explicit/fkb")),
            ..AppConfig::default()
        };
        assert_eq!(cfg.resolve_fkb_dir(), PathBuf::from("/explicit/fkb"));
        let cfg = AppConfig {
            rules_dir: Some(PathBuf::from("/explicit/rules")),
            ..AppConfig::default()
        };
        assert_eq!(cfg.resolve_rules_dir(), PathBuf::from("/explicit/rules"));
    }

    #[test]
    fn env_var_is_used_when_no_explicit() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("GRAPHTELL_VIEWS_DIR", "/env/views");
        let cfg = AppConfig::default();
        assert_eq!(cfg.resolve_views_dir(), PathBuf::from("/env/views"));
        std::env::remove_var("GRAPHTELL_VIEWS_DIR");
    }

    /// When nothing (explicit / env / manifest / exe-relative) resolves, the bare `views`/`fkb`/`rules` name is the
    /// last-ditch fallback. The integration suite never hits this branch, so a typo in the fallback name would be
    /// invisible there.
    #[test]
    fn falls_back_to_bare_name_when_nothing_set() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("GRAPHTELL_VIEWS_DIR");
        std::env::remove_var("GRAPHTELL_FKB_DIR");
        std::env::remove_var("GRAPHTELL_RULES_DIR");
        let cfg = AppConfig::default();
        assert_eq!(cfg.resolve_views_dir(), PathBuf::from("views"));
        assert_eq!(cfg.resolve_fkb_dir(), PathBuf::from("fkb"));
        assert_eq!(cfg.resolve_rules_dir(), PathBuf::from("rules"));
    }

    #[test]
    fn ui_dir_is_none_when_not_configured() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("GRAPHTELL_UI_DIR");
        let cfg = AppConfig::default();
        assert_eq!(cfg.resolve_ui_dir(), None, "dev default must not host the SPA");
    }

    #[test]
    fn database_path_under_data_dir() {
        let cfg = AppConfig {
            data_dir: PathBuf::from("./data"),
            ..AppConfig::default()
        };
        assert_eq!(cfg.database_path(), PathBuf::from("./data/graphtell.sqlite"));
    }

    // ---- resolve_ui_dir: real logic (returns Option; exe-relative branch checks index.html, distinct from fkb/views) ----

    #[test]
    fn ui_dir_explicit_override_wins() {
        let cfg = AppConfig {
            ui_dir: Some(PathBuf::from("/explicit/ui")),
            ..AppConfig::default()
        };
        assert_eq!(cfg.resolve_ui_dir(), Some(PathBuf::from("/explicit/ui")));
    }

    #[test]
    fn ui_dir_env_var_used_when_no_explicit() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("GRAPHTELL_UI_DIR", "/env/ui");
        let cfg = AppConfig::default();
        assert_eq!(cfg.resolve_ui_dir(), Some(PathBuf::from("/env/ui")));
        std::env::remove_var("GRAPHTELL_UI_DIR");
    }

    #[test]
    fn ui_dir_explicit_wins_over_env() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("GRAPHTELL_UI_DIR", "/env/ui");
        let cfg = AppConfig {
            ui_dir: Some(PathBuf::from("/explicit/ui")),
            ..AppConfig::default()
        };
        assert_eq!(cfg.resolve_ui_dir(), Some(PathBuf::from("/explicit/ui")));
        std::env::remove_var("GRAPHTELL_UI_DIR");
    }
}
