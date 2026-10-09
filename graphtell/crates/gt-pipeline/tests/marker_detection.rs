//! End-to-end self-check of the **sub-project marker table** (`MarkerProvider`, `gt-adapter-techstack`).
//!
//! The table is *bootstrap* knowledge: Ingest decides a sub-project's language from it **before** any FKB is
//! loaded, so a wrong or unreachable entry silently disables every language-gated step (dependency detection,
//! the namespace policy, i18n conventions …). `gt-adapter-techstack/src/lib.rs` only asserts the table's
//! *contents* (and that each marker's language has an adapter); nothing asserted that Ingest can actually
//! consume each entry. `crates/gt-adapter-fs/tests/sample_project_scan.rs` does check marker discovery, but
//! only for `composer.json` / `package.json` and behind a sample that is `#[ignore]`d by default.
//!
//! These cases run the **full pipeline** on synthetic projects (no external sample), table-driven over
//! `DefaultMarkerProvider::markers()` — so a marker added to the table is covered automatically, and one that
//! is declared but never reached (name mismatch, scanner exclusion, …) fails here instead of in production.

mod common;

use std::path::{Path, PathBuf};

use gt_adapter_techstack::DefaultMarkerProvider;
use gt_domain::model::ProjectConfig;
use gt_domain::port::MarkerProvider;

/// A minimal but well-formed body for each marker file, so a later phase (P0 / P3 reading the manifest)
/// cannot fail the build for a reason unrelated to marker discovery.
fn marker_body(file: &str) -> &'static str {
    match file {
        "composer.json" => r#"{"name":"demo/demo","require":{"php":">=8.1"}}"#,
        "package.json" => r#"{"name":"demo","version":"1.0.0"}"#,
        "pom.xml" => "<project><modelVersion>4.0.0</modelVersion><artifactId>demo</artifactId></project>",
        "build.gradle" => "plugins { id 'java' }\n",
        "build.gradle.kts" => "plugins { java }\n",
        "Cargo.toml" => "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        "go.mod" => "module demo\n\ngo 1.21\n",
        "pyproject.toml" => "[project]\nname = \"demo\"\n",
        "requirements.txt" => "flask>=2.0\n",
        "setup.py" => "from setuptools import setup\n\nsetup(name=\"demo\")\n",
        "Pipfile" => "[packages]\nflask = \"*\"\n",
        // Ruby / Rails markers.
        "Gemfile" => "source 'https://rubygems.org'\ngem 'rails', '~> 7.0'\n",
        "Gemfile.lock" => "GEM\n  remote: https://rubygems.org/\n  specs:\n",
        "*.gemspec" => "Gem::Specification.new do |s|\n  s.name = 'demo'\n  s.version = '0.1.0'\nend\n",
        // .NET markers.
        "*.csproj" => "<Project Sdk=\"Microsoft.NET.Sdk\">\n</Project>\n",
        "*.sln" => "Microsoft Visual Studio Solution File, Format Version 12.00\n# Visual Studio Version 17\n",
        other => panic!("no fixture body for marker `{other}` — add one for the new marker"),
    }
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gt-marker-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// `(language, role, detected_by)` of every sub-project a full build recognised.
fn sub_projects(root: &Path) -> Vec<(String, String, String)> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    b.outcome
        .sub_projects
        .iter()
        .map(|s| {
            (
                s.language.as_str().to_string(),
                s.role.clone(),
                s.detected_by.clone(),
            )
        })
        .collect()
}

/// Every entry in the table must be consumable: a project whose **only** marker is that file must yield a
/// sub-project of the declared language, detected by it. It catches a marker that is **declared but
/// unreachable** (renamed, filtered out by the scanner, …); a marker *missing* from the table is instead
/// caught by `gt-adapter-techstack`'s own table assertions plus a per-stack case such as
/// `library_detection::gradle_kotlin_dsl_project_is_recognised_as_java`.
#[test]
fn every_marker_in_the_table_yields_its_declared_sub_project() {
    let markers = DefaultMarkerProvider::new().markers();
    // Guard against a vacuous pass: an empty table would make the loop below green while Ingest detects
    // nothing at all.
    assert!(!markers.is_empty(), "the marker table must not be empty");
    for m in markers {
        let root = tmp(&m.file.replace(['.', '/'], "-"));
        std::fs::write(root.join(&m.file), marker_body(&m.file)).expect("write marker");

        let subs = sub_projects(&root);
        let hit = subs.iter().find(|(_, _, by)| by == &m.file);
        assert!(
            hit.is_some(),
            "a project carrying only `{}` must be detected by it, got: {subs:?}",
            m.file
        );
        let (lang, role, _) = hit.expect("checked above");
        assert_eq!(
            lang,
            m.language.as_str(),
            "marker `{}` must yield language `{}`, got `{lang}`",
            m.file,
            m.language.as_str()
        );
        assert!(
            role.starts_with(&m.role),
            "marker `{}` declares role `{}`, got `{role}`",
            m.file,
            m.role
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Two **different** markers in the same directory: Ingest keeps one sub-project per directory
/// (`subs.iter().any(|s| s.root_path == dir)`), so the second marker must not spawn a second sub-project
/// for the same root. Which of the two wins is scan order, so only the collapse is asserted — a regression
/// that stopped deduplicating would silently report the same directory twice, under two languages.
#[test]
fn two_markers_in_one_directory_collapse_to_one_sub_project() {
    let root = tmp("same-dir");
    std::fs::write(root.join("composer.json"), marker_body("composer.json")).unwrap();
    std::fs::write(root.join("package.json"), marker_body("package.json")).unwrap();

    let subs = sub_projects(&root);
    assert_eq!(
        subs.len(),
        1,
        "one directory with two markers must still be one sub-project, got: {subs:?}"
    );
    assert!(
        subs[0].2 == "composer.json" || subs[0].2 == "package.json",
        "the surviving sub-project must be detected by one of the two markers, got: {:?}",
        subs[0]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Two markers in **different** directories must give two sub-projects, each with its own language — the
/// monorepo split `find_markers` exists for (pinned sample-free; the sample-gated
/// `sample_project_scan::finds_sub_project_markers` cannot run in CI).
#[test]
fn markers_in_separate_directories_split_the_project() {
    let root = tmp("split");
    let api = root.join("api");
    let web = root.join("web");
    std::fs::create_dir_all(&api).unwrap();
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(api.join("composer.json"), marker_body("composer.json")).unwrap();
    std::fs::write(web.join("package.json"), marker_body("package.json")).unwrap();

    let subs = sub_projects(&root);
    let langs: Vec<&str> = subs.iter().map(|s| s.0.as_str()).collect();
    assert!(
        langs.contains(&"php"),
        "the api/ composer.json must yield a php sub-project, got: {subs:?}"
    );
    assert!(
        langs.contains(&"javascript"),
        "the web/ package.json must yield a javascript sub-project, got: {subs:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// With **no** marker at all, Ingest falls back to a single root sub-project at language `unknown` rather
/// than dropping the project (ingest.rs `fallback:root`). That fallback is what keeps a repo of loose
/// sources analysable, and it is also why "language unknown" must stay a *visible* state.
#[test]
fn a_project_without_any_marker_falls_back_to_one_unknown_root() {
    let root = tmp("no-marker");
    std::fs::write(root.join("main.php"), "<?php\nclass User {}\n").unwrap();

    let subs = sub_projects(&root);
    assert_eq!(
        subs,
        vec![(
            "unknown".to_string(),
            "unknown".to_string(),
            "fallback:root".to_string()
        )],
        "with no marker the project must degrade to one unknown root sub-project, got: {subs:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
