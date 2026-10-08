//! End-to-end self-check: a JS frontend sub-project's **type** (mobile / mini-program) is recognised through the
//! *full pipeline*, not merely at the adapter unit level.
//!
//! `JsTechStackAdapter.sub_project_kind` already has unit tests in `gt-adapter-techstack/src/lib.rs`
//! (`js_detects_react_native_as_mobile`, …). Those only assert the adapter returns the right string.
//! This file closes the remaining gap: after `gt_pipeline::runner::run` completes, the
//! `SubProject.role` written into the store must carry the refined kind — i.e. the adapter result is
//! actually plumbed through `P0 Ingest`'s `refine_role` → `refine_frontend_kind` → `techstack.adapter_for`.
//!
//! Uses synthetic projects in a temp dir (no external sample needed), exercising the *config-fallback* branch
//! (directory name does not contain a keyword, so the decision comes from `package.json` / `manifest.json`).

mod common;

use gt_domain::model::ProjectConfig;
use gt_domain::port::ProjectReader;

/// Build a one-sub-project synthetic repo: `app/package.json` (+ optional `app/manifest.json`) plus a tiny
/// JS file so the JS parser has something to chew on. The sub-project directory is named `app` on purpose —
/// it contains no mini-program / mobile keyword, so `refine_frontend_kind` must fall through to the adapter.
fn synthetic_frontend_root(tag: &str, package_json: &str, manifest_json: Option<&str>) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gt-subkind-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app")).expect("mkdir app");

    std::fs::write(dir.join("app/package.json"), package_json).expect("write package.json");
    if let Some(m) = manifest_json {
        std::fs::write(dir.join("app/manifest.json"), m).expect("write manifest.json");
    }
    std::fs::write(dir.join("app/index.js"), "const greeting = 'hi';\n").expect("write index.js");

    dir
}

/// Role of the `app` sub-project after a full build.
fn app_role(root: &std::path::Path) -> String {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graph build should succeed");
    let subs = b.store.list_sub_projects(b.project.id).expect("the sub-projects must be readable");
    subs.into_iter()
        .find(|s| s.name == "app")
        .map(|s| s.role)
        .expect("app sub-project should be recognised")
}

#[test]
fn react_native_sub_project_classified_as_mobile() {
    let root = synthetic_frontend_root(
        "rn",
        r#"{"dependencies":{"react-native":"^0.72.0","react":"^18.2.0"}}"#,
        None,
    );
    assert_eq!(
        app_role(&root),
        "frontend:mobile",
        "react-native must be recognised as frontend:mobile through the full pipeline"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn uni_app_with_mp_target_classified_as_mini_program() {
    let root = synthetic_frontend_root(
        "mp",
        r#"{"dependencies":{"@dcloudio/uni-app":"^3.0.0","@dcloudio/uni-mp-weixin":"^3.0.0"}}"#,
        Some(r#"{"mp-weixin":{"appid":"wx123"},"mp-alipay":{}}"#),
    );
    assert_eq!(
        app_role(&root),
        "frontend:mini-program",
        "uni-app + uni-mp-* must be recognised as frontend:mini-program through the full pipeline"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn uni_app_without_mp_target_stays_plain_frontend() {
    let root = synthetic_frontend_root(
        "web",
        r#"{"dependencies":{"@dcloudio/uni-app":"^3.0.0","@dcloudio/uni-h5":"^3.0.0"}}"#,
        None,
    );
    assert_eq!(
        app_role(&root),
        "frontend",
        "uni-app with no mp-* target must not declare a kind; it stays bare frontend"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Like `synthetic_frontend_root`, but the sub-project directory carries a **type keyword** in its name (instead of
/// the neutral `app`) so the decision must come from `refine_frontend_kind`'s *directory-name* branch, not the
/// config fallback. A plain-web `package.json` (no react-native / no uni-app) guarantees the adapter's
/// `sub_project_kind` would return `None`, so any kind on the role can only have come from the directory name.
fn synthetic_frontend_root_named(
    tag: &str,
    dir_name: &str,
    package_json: &str,
    manifest_json: Option<&str>,
) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gt-subkind-named-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(dir_name)).expect("mkdir sub");

    std::fs::write(dir.join(dir_name).join("package.json"), package_json).expect("write package.json");
    if let Some(m) = manifest_json {
        std::fs::write(dir.join(dir_name).join("manifest.json"), m).expect("write manifest.json");
    }
    std::fs::write(dir.join(dir_name).join("index.js"), "const greeting = 'hi';\n").expect("write index.js");

    dir
}

/// Role of a *named* sub-project after a full build.
fn role_of(root: &std::path::Path, name: &str) -> String {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graph build should succeed");
    let subs = b.store.list_sub_projects(b.project.id).expect("the sub-projects must be readable");
    subs.into_iter()
        .find(|s| s.name == name)
        .map(|s| s.role)
        .expect("the named sub-project should be recognised")
}

/// The directory-name branch of `refine_frontend_kind` (tried *before* the config fallback) must also reach
/// `SubProject.role` through the full pipeline — `ingest.rs` only asserts it at the `refine_role` unit level, so a
/// regression that stopped calling `refine_role` (or passed the wrong dir) from the runner would otherwise go
/// unnoticed. A plain-web `package.json` means the adapter cannot be the source of the kind.
#[test]
fn miniprogram_dir_classified_as_mini_program_through_pipeline() {
    let root = synthetic_frontend_root_named(
        "mpdir",
        "miniprogram",
        r#"{"dependencies":{"react":"^18.2.0","vue":"^3.3.0"}}"#,
        None,
    );
    assert_eq!(
        role_of(&root, "miniprogram"),
        "frontend:mini-program",
        "a directory literally named `miniprogram` must refine to frontend:mini-program through the full pipeline"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn admin_dir_classified_as_admin_through_pipeline() {
    let root = synthetic_frontend_root_named(
        "admindir",
        "admin",
        r#"{"dependencies":{"react":"^18.2.0"}}"#,
        None,
    );
    assert_eq!(
        role_of(&root, "admin"),
        "frontend:admin",
        "a directory literally named `admin` must refine to frontend:admin through the full pipeline"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn mobile_dir_classified_as_mobile_through_pipeline() {
    let root = synthetic_frontend_root_named(
        "mobdir",
        "mobile",
        r#"{"dependencies":{"react":"^18.2.0"}}"#,
        None,
    );
    assert_eq!(
        role_of(&root, "mobile"),
        "frontend:mobile",
        "a directory literally named `mobile` must refine to frontend:mobile through the full pipeline"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The directory-name branch must **win over** the config fallback. A directory literally named `mobile` whose
/// `package.json` is a uni-app mini-program project (which the adapter alone would classify `mini-program`) must
/// still be `frontend:mobile`: `refine_frontend_kind` returns from the name match (lines 280-304) *before* consulting
/// the adapter. This ordering invariant is only asserted at the `ingest.rs` unit level today; pinning it through the
/// full pipeline catches a regression that reordered the two branches — which would silently flip real `mobile/`
/// directories to `mini-program`.
#[test]
fn directory_name_wins_over_config_fallback() {
    let root = synthetic_frontend_root_named(
        "namewins",
        "mobile",
        r#"{"dependencies":{"@dcloudio/uni-app":"^3.0.0","@dcloudio/uni-mp-weixin":"^3.0.0"}}"#,
        Some(r#"{"mp-weixin":{"appid":"wx123"}}"#),
    );
    assert_eq!(
        role_of(&root, "mobile"),
        "frontend:mobile",
        "directory name `mobile` must win over the uni-app mini-program config fallback"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `h5` is intentionally grouped into the `mobile` family (`refine_frontend_kind` line 301), so a directory whose
/// name contains `h5` refines to `frontend:mobile` even for a plain-web project. Pinned as the current intended
/// behaviour so a later reordering / trimming of the keyword families cannot silently drop it.
#[test]
fn h5_dir_classified_as_mobile_through_pipeline() {
    let root = synthetic_frontend_root_named(
        "h5dir",
        "h5",
        r#"{"dependencies":{"react":"^18.2.0"}}"#,
        None,
    );
    assert_eq!(
        role_of(&root, "h5"),
        "frontend:mobile",
        "a directory named `h5` must refine to frontend:mobile (current intended grouping)"
    );
    let _ = std::fs::remove_dir_all(&root);
}
