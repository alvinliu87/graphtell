//! JavaScript / TypeScript tech-stack knowledge for GraphTell.
//!
//! JS has no PSR-4-style autoload map and no PHP-style `return [...]` config, so this adapter only
//! implements [`TechStackAdapter::sub_project_kind`] (frontend framework recognition) plus the
//! `package.json` / `package-lock.json` dependency and i18n-path conventions. It serves both the
//! `javascript` and `typescript` language tags.

use std::path::Path;

use gt_domain::model::Language;
use gt_domain::port::{FileSystem, TechStackAdapter};

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

    /// One ecosystem, several language tags: a `.ts` sub-project must not lose its adapter
    /// (which would silently degrade dependency detection to a whole-file text probe).
    fn serves(&self, language: &Language) -> bool {
        matches!(language.as_str(), Language::JAVASCRIPT | Language::TYPESCRIPT)
    }

    fn i18n_path_patterns(&self) -> Vec<String> {
        // Directory forms first: `src/locales/en/translation.json` must yield `en`, not
        // `en/translation` — the flat-file forms are the fallback for one-file-per-locale layouts.
        vec![
            "locales/{locale}/".into(),
            "src/locales/{locale}/".into(),
            "public/locales/{locale}/".into(),
            "src/i18n/{locale}/".into(),
            "locales/{locale}.json".into(),
            "src/locales/{locale}.json".into(),
        ]
    }

    fn manifest_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        if file_name != "package.json" {
            return None;
        }
        let pkg: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(collect_deps(&pkg))
    }

    fn lock_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        if file_name != "package-lock.json" {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let mut names: Vec<String> = Vec::new();
        // v2+: `packages` is an object keyed by install path (`node_modules/foo`).
        if let Some(map) = v.get("packages").and_then(|s| s.as_object()) {
            for (key, entry) in map {
                let n = entry.get("name").and_then(|n| n.as_str()).unwrap_or(key);
                names.push(n.trim_start_matches("node_modules/").to_string());
            }
        }
        // v1: `dependencies` is an object keyed by package name.
        if let Some(map) = v.get("dependencies").and_then(|s| s.as_object()) {
            names.extend(map.keys().cloned());
        }
        Some(names)
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

    /// One ecosystem, several language tags: a `.ts` sub-project must keep this adapter, otherwise dependency
    /// detection silently degrades to a whole-file text probe.
    #[test]
    fn js_adapter_also_serves_typescript() {
        let a = JsTechStackAdapter::new();
        assert!(a.serves(&Language::new(Language::JAVASCRIPT)));
        assert!(a.serves(&Language::new(Language::TYPESCRIPT)), "a TS sub-project must not lose its adapter");
        assert!(!a.serves(&Language::new(Language::PHP)));
        assert!(!a.serves(&Language::new("python")));
    }

    /// **Directory forms must come before flat forms**: `src/locales/en/translation.json` has to yield `en`,
    /// not `en/translation`. The flat forms are only the fallback for one-file-per-locale layouts.
    #[test]
    fn i18n_patterns_put_directory_forms_first() {
        let pats = JsTechStackAdapter::new().i18n_path_patterns();
        assert_eq!(
            pats,
            vec![
                "locales/{locale}/".to_string(),
                "src/locales/{locale}/".to_string(),
                "public/locales/{locale}/".to_string(),
                "src/i18n/{locale}/".to_string(),
                "locales/{locale}.json".to_string(),
                "src/locales/{locale}.json".to_string(),
            ]
        );
        let first_flat = pats
            .iter()
            .position(|p| !p.ends_with('/'))
            .expect("the flat form must be present");
        assert!(
            pats[..first_flat].iter().all(|p| p.ends_with('/')),
            "every directory form must be ordered before the flat forms: {pats:?}"
        );
    }

    /// `dependencies` / `devDependencies` / `peerDependencies` all count — a framework installed as a peer
    /// dependency is still in use.
    #[test]
    fn collect_deps_covers_all_three_dependency_fields() {
        let pkg = serde_json::json!({
            "dependencies": { "react": "^18" },
            "devDependencies": { "@dcloudio/uni-mp-weixin": "^2" },
            "peerDependencies": { "react-native": "*" }
        });
        let deps = collect_deps(&pkg);
        assert!(deps.contains(&"react".to_string()), "{deps:?}");
        assert!(deps.contains(&"@dcloudio/uni-mp-weixin".to_string()), "devDependencies must count: {deps:?}");
        assert!(deps.contains(&"react-native".to_string()), "peerDependencies must count: {deps:?}");

        // A non-object section or a missing one is ignored rather than erroring.
        assert!(collect_deps(&serde_json::json!({ "dependencies": [] })).is_empty());
        assert!(collect_deps(&serde_json::json!({})).is_empty());
    }

    /// Every mini-program compiler target must be recognised, and matching is case-insensitive (the check runs
    /// over the lowercased document, so `MP-WEIXIN` in a hand-edited manifest still counts).
    #[test]
    fn manifest_has_mp_target_recognises_every_platform() {
        for t in [
            "mp-weixin", "mp-alipay", "mp-toutiao", "mp-baidu", "mp-qq", "mp-360",
            "mp-kuaishou", "mp-jd", "mp-lark", "mp-xhs", "mp-qsn",
        ] {
            assert!(
                manifest_has_mp_target(&serde_json::json!({ "app": { t: {} } })),
                "target `{t}` must be recognised as a mini program"
            );
        }
        assert!(manifest_has_mp_target(&serde_json::json!({ "x": "MP-WEIXIN" })), "it must be case-insensitive");
        assert!(!manifest_has_mp_target(&serde_json::json!({ "h5": {} })), "plain h5 is not a mini program");
    }

    /// `manifest_dependencies` only accepts `package.json` and merges the three dependency scopes; a wrong file
    /// or unparseable body degrades to `None` (the kernel then falls back to a whole-file text probe).
    #[test]
    fn manifest_dependencies_parses_package_json() {
        let a = JsTechStackAdapter::new();

        // wrong file -> None, so the kernel keeps its text-probe fallback
        assert_eq!(a.manifest_dependencies("composer.json", "{}"), None);
        assert_eq!(a.manifest_dependencies("package-lock.json", "{}"), None);
        // unparseable JSON -> None
        assert_eq!(a.manifest_dependencies("package.json", "{not json"), None);

        // valid but dependency-less -> `Some(empty)`, NOT `None` (keeps "no manifest" distinct from "no deps")
        assert_eq!(a.manifest_dependencies("package.json", "{}"), Some(vec![]));

        // all three dependency scopes are merged
        let text = serde_json::json!({
            "name": "x",
            "dependencies": { "react": "^18" },
            "devDependencies": { "vite": "^5" },
            "peerDependencies": { "typescript": "^5" }
        })
        .to_string();
        let mut deps = a.manifest_dependencies("package.json", &text).unwrap();
        deps.sort();
        assert_eq!(
            deps,
            vec!["react".to_string(), "typescript".to_string(), "vite".to_string()]
        );
    }

    /// `lock_dependencies` parses the v2 `packages` map and the v1 `dependencies` map, stripping the
    /// `node_modules/` install-path prefix and preferring an explicit `name` field.
    #[test]
    fn lock_dependencies_parses_v1_and_v2() {
        let a = JsTechStackAdapter::new();

        assert_eq!(a.lock_dependencies("package.json", "{}"), None);
        assert_eq!(a.lock_dependencies("yarn.lock", "{}"), None);
        assert_eq!(a.lock_dependencies("package-lock.json", "not json"), None);

        // v2: `packages` keyed by install path; an entry without `name` falls back to the (prefix-stripped) key,
        // an entry with `name` uses it verbatim.
        let v2 = serde_json::json!({
            "packages": {
                "": { "name": "root" },
                "node_modules/react": { "version": "18" },
                "node_modules/@scope/bar": { "name": "@scope/bar-custom" },
                "node_modules/foo": {}
            }
        })
        .to_string();
        let mut names = a.lock_dependencies("package-lock.json", &v2).unwrap();
        names.sort();
        assert_eq!(
            names,
            vec![
                "@scope/bar-custom".to_string(),
                "foo".to_string(),
                "react".to_string(),
                "root".to_string(),
            ]
        );

        // v1: `dependencies` keyed directly by package name
        let v1 =
            serde_json::json!({ "dependencies": { "lodash": "^4", "axios": "^1" } }).to_string();
        let mut v1names = a.lock_dependencies("package-lock.json", &v1).unwrap();
        v1names.sort();
        assert_eq!(v1names, vec!["axios".to_string(), "lodash".to_string()]);
    }

    /// The React Native test is `d == "react-native" || d.contains("react-native")` — a **substring** match, so
    /// any package whose name contains `react-native` claims the `mobile` kind. Pinned as-is:
    /// * `react-native-web` is a **web** library, yet it is currently classified `mobile`;
    /// * `@react-native-community/cli` matches too (RN tooling — what the substring form is really for).
    ///
    /// Also pinned: React Native is evaluated **before** the uni-app family, so a project carrying both is
    /// `mobile` rather than `mini-program`.
    #[test]
    fn react_native_is_matched_by_substring_and_wins_over_uni_app() {
        let fs = StdFileSystem::new();
        let kind_of = |tag: &str, pkg: &str| -> Option<String> {
            let dir = scratch(tag);
            std::fs::write(dir.join("package.json"), pkg).unwrap();
            let k = JsTechStackAdapter::new().sub_project_kind(&dir, &fs);
            let _ = std::fs::remove_dir_all(&dir);
            k
        };

        assert_eq!(
            kind_of("rnw", r#"{"dependencies":{"react-native-web":"^0.19"}}"#),
            Some("mobile".to_string()),
            "pinned: `react-native-web` currently claims `mobile` — it is a web library, matched only by substring"
        );
        assert_eq!(
            kind_of("rnc", r#"{"dependencies":{"@react-native-community/cli":"^1"}}"#),
            Some("mobile".to_string()),
            "RN tooling matches through the substring form"
        );
        assert_eq!(
            kind_of("precedence", r#"{"dependencies":{"@dcloudio/uni-mp-weixin":"1","react-native":"1"}}"#),
            Some("mobile".to_string()),
            "react-native is checked before the uni-app family, so it wins"
        );
    }

    /// The lookup tries `<dir>/manifest.json` then `<dir>/src/manifest.json`, and a manifest that **exists but
    /// declares no mp-* target** must not end the search — a uni-app project whose real manifest sits under
    /// `src/` (with a root one that only has `h5`) would otherwise be mislabelled plain web.
    #[test]
    fn manifest_lookup_continues_past_a_manifest_without_a_target() {
        let fs = StdFileSystem::new();
        let dir = scratch("root_no_target");
        std::fs::write(dir.join("package.json"), r#"{"dependencies":{"@dcloudio/uni-app":"1"}}"#)
            .unwrap();
        std::fs::write(dir.join("manifest.json"), r#"{"h5":{}}"#).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("manifest.json"), r#"{"mp-weixin":{}}"#).unwrap();

        assert_eq!(
            JsTechStackAdapter::new().sub_project_kind(&dir, &fs),
            Some("mini-program".to_string()),
            "a root manifest with no mp target must not shadow the one under src/"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lockfile carrying **both** the v2 `packages` map and the v1 `dependencies` map contributes names from
    /// each, so one package can appear twice — `lock_dependencies` does not dedupe (unlike the POM/Gradle
    /// extractors in the Java adapter). Pinned: harmless for "does this name occur" matching, but it must not
    /// later be mistaken for a regression.
    #[test]
    fn lock_dependencies_does_not_dedupe_across_the_v1_and_v2_sections() {
        let a = JsTechStackAdapter::new();
        let dual = serde_json::json!({
            "packages": { "node_modules/react": { "version": "18" } },
            "dependencies": { "react": { "version": "18" } }
        })
        .to_string();
        assert_eq!(
            a.lock_dependencies("package-lock.json", &dual),
            Some(vec!["react".to_string(), "react".to_string()]),
            "both sections contribute their own entry"
        );
    }

    /// `read_json` degrades gracefully: `None` on a missing file or an unparseable one, `Some` on valid JSON.
    #[test]
    fn read_json_error_paths() {
        let dir = scratch("readjson");
        let fs = StdFileSystem::new();
        assert_eq!(read_json(&dir.join("nope.json"), &fs), None);
        std::fs::write(dir.join("bad.json"), "{not json").unwrap();
        assert_eq!(read_json(&dir.join("bad.json"), &fs), None);
        std::fs::write(dir.join("ok.json"), "{\"a\":1}").unwrap();
        assert_eq!(
            read_json(&dir.join("ok.json"), &fs),
            Some(serde_json::json!({"a":1}))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
