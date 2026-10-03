//! `gt-adapter-techstack` — cross-language tech-stack knowledge (implementation of the outbound port
//! `MarkerProvider`).
//!
//! Ingest needs to decide a sub-project's language **before** any FKB is loaded (bootstrap), so the
//! "which manifest file marks which language / role" table cannot live in FKB. It lives here, in an adapter,
//! and the kernel only knows the `MarkerProvider` port.

use std::path::Path;

use gt_domain::model::Language;
use gt_domain::port::{FileSystem, Marker, MarkerProvider, TechStackAdapter};

/// The default sub-project detection markers, one entry per ecosystem.
pub struct DefaultMarkerProvider;

impl DefaultMarkerProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DefaultMarkerProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkerProvider for DefaultMarkerProvider {
    /// Marker file -> (language, role).
    ///
    /// `role` is the **default** system tier; Ingest may still refine it from the directory name
    /// (e.g. `frontend:admin`), so this is only the bootstrap tier, not the final role.
    fn markers(&self) -> Vec<Marker> {
        vec![
            Marker {
                file: "composer.json".into(),
                language: Language::new(Language::PHP),
                role: "backend".into(),
            },
            Marker {
                file: "package.json".into(),
                language: Language::new(Language::JAVASCRIPT),
                role: "frontend".into(),
            },
            Marker {
                file: "pom.xml".into(),
                language: Language::new(Language::JAVA),
                role: "backend".into(),
            },
            Marker {
                file: "build.gradle".into(),
                language: Language::new(Language::JAVA),
                role: "backend".into(),
            },
            Marker {
                file: "Cargo.toml".into(),
                language: Language::new(Language::RUST),
                role: "backend".into(),
            },
            Marker {
                file: "go.mod".into(),
                language: Language::new("go"),
                role: "backend".into(),
            },
            Marker {
                file: "pyproject.toml".into(),
                language: Language::new(Language::PYTHON),
                role: "backend".into(),
            },
        ]
    }
}

/// The JavaScript / TypeScript tech-stack adapter.
///
/// JS has no PSR-4-style autoload map and no PHP-style `return [...]` config, so it only implements
/// [`TechStackAdapter::sub_project_kind`]: recognising the frontend framework from `package.json` /
/// `manifest.json` when the directory name alone is inconclusive.
pub struct JsTechStackAdapter;

impl JsTechStackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsTechStackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl TechStackAdapter for JsTechStackAdapter {
    fn language(&self) -> Language {
        Language::new(Language::JAVASCRIPT)
    }

    fn sub_project_kind(&self, dir: &Path, fs: &dyn FileSystem) -> Option<String> {
        detect_frontend_kind_via_config(dir, fs).map(|k| k.to_string())
    }
}

/// Read `package.json` and similar config to recognise the frontend framework, covering misses caused by
/// "irregular directory naming". Mainly covers:
/// - React Native -> `mobile`
/// - uni-app: if it directly depends on a `uni-mp-*` compiler package, or `manifest.json` declares an
///   mp-* mini-program target, then `mini-program`; otherwise it is just ordinary web and falls back to
///   the default `frontend` (no forced mini-program tag).
fn detect_frontend_kind_via_config(dir: &Path, fs: &dyn FileSystem) -> Option<&'static str> {
    let pkg = read_json(&dir.join("package.json"), fs)?;
    let deps = collect_deps(&pkg);

    // React Native -> mobile
    if deps.iter().any(|d| d == "react-native" || d.contains("react-native")) {
        return Some("mobile");
    }

    // uni-app family
    let is_uni = deps
        .iter()
        .any(|d| d.contains("uni-app") || d.contains("@dcloudio/uni"));
    if is_uni {
        // Directly depends on a concrete mini-program-platform compiler package
        if deps.iter().any(|d| d.contains("uni-mp-")) {
            return Some("mini-program");
        }
        // Otherwise check whether manifest.json declares an mp-* target
        for m in [dir.join("manifest.json"), dir.join("src").join("manifest.json")] {
            if let Some(mani) = read_json(&m, fs) {
                if manifest_has_mp_target(&mani) {
                    return Some("mini-program");
                }
            }
        }
    }
    None
}

/// Merge the key lists of dependencies / devDependencies / peerDependencies.
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

/// Whether `manifest.json` (uni-app) declares any mini-program compiler target (mp-weixin / mp-alipay …).
fn manifest_has_mp_target(mani: &serde_json::Value) -> bool {
    let lower = mani.to_string().to_ascii_lowercase();
    [
        "mp-weixin", "mp-alipay", "mp-toutiao", "mp-baidu", "mp-qq", "mp-360",
        "mp-kuaishou", "mp-jd", "mp-lark", "mp-xhs", "mp-qsn",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Safely read and parse a JSON file; return None when missing / parse fails.
fn read_json(path: &Path, fs: &dyn FileSystem) -> Option<serde_json::Value> {
    let s = fs.read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `go.mod` / `Cargo.toml` must stay in the table: `gt-pipeline/tests/unsupported_language.rs` pins that a
    /// language which is recognised but has **no parser** still produces a `NoParserForLanguage` diagnostic —
    /// dropping those markers would silently make that test vacuous (and rust/go projects undetected).
    #[test]
    fn covers_parserless_languages_too() {
        let markers = DefaultMarkerProvider::new().markers();
        let lookup = |file: &str| {
            markers
                .iter()
                .find(|m| m.file == file)
                .map(|m| (m.language.as_str().to_string(), m.role.clone()))
        };
        assert_eq!(
            lookup("go.mod"),
            Some(("go".to_string(), "backend".to_string()))
        );
        assert_eq!(
            lookup("Cargo.toml"),
            Some(("rust".to_string(), "backend".to_string()))
        );
    }

    #[test]
    fn maps_each_ecosystem_marker() {
        let markers = DefaultMarkerProvider::new().markers();
        let expected = [
            ("composer.json", "php", "backend"),
            ("package.json", "javascript", "frontend"),
            ("pom.xml", "java", "backend"),
            ("build.gradle", "java", "backend"),
            ("Cargo.toml", "rust", "backend"),
            ("go.mod", "go", "backend"),
            ("pyproject.toml", "python", "backend"),
        ];
        for (file, lang, role) in expected {
            let m = markers
                .iter()
                .find(|m| m.file == file)
                .unwrap_or_else(|| panic!("marker {file} 缺失"));
            assert_eq!(m.language.as_str(), lang, "marker {file} 的语言不对");
            assert_eq!(m.role, role, "marker {file} 的角色不对");
        }
        assert_eq!(markers.len(), expected.len(), "marker 数量应与预期一致");
    }

    // ---- JsTechStackAdapter: frontend framework recognition moved out of the kernel ----

    use gt_adapter_fs::StdFileSystem;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jsad_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn js_detects_react_native_as_mobile() {
        let dir = scratch("rn");
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"react-native":"^0.72.0","react":"^18.2.0"}}"#,
        )
        .unwrap();
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            Some("mobile".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn js_detects_uni_app_mini_program_from_manifest() {
        let dir = scratch("mp");
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
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            Some("mini-program".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn js_uni_app_without_mp_target_is_not_mini_program() {
        let dir = scratch("web");
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"@dcloudio/uni-app":"^3.0.0","@dcloudio/uni-h5":"^3.0.0"}}"#,
        )
        .unwrap();
        // No mp-* target declared -> ordinary web: the adapter must not claim a kind, so the kernel
        // keeps the bare `frontend` role.
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A project that only declares a concrete uni-app mini-program compiler package (no `@dcloudio/uni-app`
    /// umbrella) must still be recognised as `mini-program`: `is_uni` matches via the `@dcloudio/uni` substring
    /// and the `uni-mp-` branch fires on the compiler package itself.
    #[test]
    fn js_mp_compiler_dep_alone_is_mini_program() {
        let dir = scratch("mp_only");
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"@dcloudio/uni-mp-weixin":"^3.0.0"}}"#,
        )
        .unwrap();
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            Some("mini-program".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// uni-app projects often keep `manifest.json` under `src/`; the adapter must fall back to that path rather
    /// than only looking at the project root (a regression silently mislabels such sub-projects as plain web).
    #[test]
    fn js_reads_manifest_from_src_dir() {
        let dir = scratch("src_mani");
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"@dcloudio/uni-app":"^3.0.0"}}"#,
        )
        .unwrap();
        // No uni-mp-* dep, so the kind must come from the manifest — deliberately placed under src/.
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src").join("manifest.json"),
            r#"{"mp-qq":{}}"#, // a non-weixin mp target, exercises the full target list
        )
        .unwrap();
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            Some("mini-program".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `collect_deps` must merge `devDependencies` too: a mini-program compiler listed under devDependencies
    /// must not be missed (a regression that only scans `dependencies` would silently mislabel the sub-project).
    #[test]
    fn js_dev_dependency_mp_target_is_detected() {
        let dir = scratch("mp_dev");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devDependencies":{"@dcloudio/uni-mp-weixin":"^3.0.0"}}"#,
        )
        .unwrap();
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            Some("mini-program".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plain web project (no react-native, no uni family) must NOT get a forced kind, so Ingest keeps the
    /// bare `frontend` role — guards against over-detection.
    #[test]
    fn js_plain_web_returns_none() {
        let dir = scratch("web_plain");
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"react":"^18.2.0","vue":"^3.3.0"}}"#,
        )
        .unwrap();
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Missing `package.json`: the adapter must degrade to `None` rather than erroring.
    #[test]
    fn js_missing_package_json_returns_none() {
        let dir = scratch("no_pkg");
        // scratch() already created the (empty) dir; deliberately write no package.json.
        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &StdFileSystem::new()),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn js_reports_javascript_language() {
        assert_eq!(
            JsTechStackAdapter::new().language().as_str(),
            Language::JAVASCRIPT
        );
    }
}
