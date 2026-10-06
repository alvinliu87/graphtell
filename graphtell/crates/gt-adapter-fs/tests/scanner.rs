//! Self-contained tests of the file-scanning adapter (no external sample needed).
//!
//! `sample_project_scan.rs` is `#[ignore]`d because the sample_project sample is too large to ship, so it never runs in CI.
//! This file pins the scanner's core invariants on synthetic temp-dir trees so the exclusion rules, marker
//! lookup and language inference stay guarded on every build.

use std::path::Path;

use gt_adapter_fs::WalkDirScanner;
use gt_adapter_fs::system::{normalize, StdFileSystem};
use gt_domain::model::Language;
use gt_domain::port::{FileScanner, ScanRequest, ScannedFile};

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gt-fs-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

fn scan_all(root: &Path) -> Vec<ScannedFile> {
    WalkDirScanner::new(Vec::new())
        .scan(&ScanRequest {
            root: root.to_path_buf(),
            extra_excludes: Vec::new(),
            languages: Vec::new(),
            language_extensions: Vec::new(),
        })
        .expect("scan")
}

fn rels(files: &[ScannedFile]) -> Vec<String> {
    files.iter().map(|f| f.relative.clone()).collect()
}

/// Dependency / build-artifact / static-asset directories must never reach the set to analyse.
#[test]
fn scan_excludes_dependency_and_asset_dirs() {
    let root = scratch("excl");
    write(&root, "app/foo.php", "<?php");
    write(&root, "vendor/dep.php", "<?php");
    write(&root, "node_modules/x.js", "x");
    write(&root, "static/img.png", "x");
    write(&root, "dist/bundle.js", "x");
    write(&root, "target/debug/o", "x");
    write(&root, "build/out.js", "x");
    let rels = rels(&scan_all(&root));
    assert!(rels.contains(&"app/foo.php".to_string()), "business source must be scanned");
    for forbidden in ["vendor/", "node_modules/", "static/", "dist/", "target/", "build/"] {
        assert!(
            !rels.iter().any(|r| r.starts_with(forbidden)),
            "the scan result must not contain {forbidden}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Binary / static-asset extensions are excluded even outside the named asset directories.
#[test]
fn scan_excludes_asset_extensions() {
    let root = scratch("asset");
    write(&root, "photo.png", "x");
    write(&root, "icon.jpg", "x");
    write(&root, "font.woff2", "x");
    write(&root, "archive.zip", "x");
    write(&root, "app.go", "x");
    let rels = rels(&scan_all(&root));
    for ext in [".png", ".jpg", ".woff2", ".zip"] {
        assert!(
            !rels.iter().any(|r| r.ends_with(ext)),
            "extension {ext} must be excluded"
        );
    }
    assert!(rels.iter().any(|r| r == "app.go"), "go source must be kept");
    let _ = std::fs::remove_dir_all(&root);
}

/// The extension table must line up with the sub-project marker table: a Go / Python sub-project that is
/// detected must also yield source files.
#[test]
fn scan_infers_language_from_extension() {
    let root = scratch("lang");
    write(&root, "a.php", "");
    write(&root, "b.js", "");
    write(&root, "c.ts", "");
    write(&root, "d.py", "");
    write(&root, "e.go", "");
    write(&root, "f.kt", "");
    let files = scan_all(&root);
    let lang_of = |r: &str| files.iter().find(|f| f.relative == r).map(|f| f.language.as_str().to_string());
    assert_eq!(lang_of("a.php").as_deref(), Some("php"));
    assert_eq!(lang_of("b.js").as_deref(), Some("javascript"));
    assert_eq!(lang_of("c.ts").as_deref(), Some("typescript"));
    assert_eq!(lang_of("d.py").as_deref(), Some("python"));
    assert_eq!(lang_of("e.go").as_deref(), Some("go"));
    assert_eq!(lang_of("f.kt").as_deref(), Some("kotlin"));
    let _ = std::fs::remove_dir_all(&root);
}

/// Excludes are matched as **globs against the path**, not as bare directory names: that is what makes
/// framework-resolved rules (`sample_project/runtime/**`, `public/static/**`, `storage/logs/**`) expressible at
/// all — a name has nowhere to say "only this one, at this depth".
#[test]
fn scan_matches_extra_excludes_as_path_globs() {
    let root = scratch("globs");
    write(&root, "app/controller/Index.php", "<?php");
    write(&root, "app/generated/a/b.php", "<?php");
    write(&root, "admin/generated/c.php", "<?php");
    write(&root, "sample_project/service/Order.php", "<?php");
    let files = WalkDirScanner::new(vec!["app/generated/**".into()])
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: Vec::new(),
            language_extensions: Vec::new(),
        })
        .expect("scan");
    let rels = rels(&files);
    assert!(rels.contains(&"app/controller/Index.php".to_string()));
    assert!(rels.contains(&"sample_project/service/Order.php".to_string()));
    assert!(
        !rels.iter().any(|r| r.starts_with("app/generated/")),
        "app/generated must be excluded: {rels:?}"
    );
    assert!(
        rels.contains(&"admin/generated/c.php".to_string()),
        "a directory of the same name under another path must be kept: {rels:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A `dir/**` pattern must prune the whole subtree, not just its direct children.
#[test]
fn scan_prunes_the_whole_subtree() {
    let root = scratch("subtree");
    write(&root, "keep/keep.php", "<?php");
    write(&root, "drop/a/b/c/deep.php", "<?php");
    let files = WalkDirScanner::new(vec!["drop/**".into()])
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: Vec::new(),
            language_extensions: Vec::new(),
        })
        .expect("scan");
    let rels = rels(&files);
    assert_eq!(rels, vec!["keep/keep.php".to_string()]);
    let _ = std::fs::remove_dir_all(&root);
}

/// Unknown extensions (e.g. markdown) are not graphed.
#[test]
fn scan_skips_unknown_extensions() {
    let root = scratch("unknown");
    write(&root, "notes.md", "x");
    write(&root, "readme", "x");
    write(&root, "app.php", "<?php");
    let rels = rels(&scan_all(&root));
    assert!(!rels.iter().any(|r| r.ends_with(".md")), ".md must not be scanned");
    assert!(!rels.contains(&"readme".to_string()), "a file with no extension must not be scanned");
    assert!(rels.contains(&"app.php".to_string()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn scan_respects_language_filter() {
    let root = scratch("filter");
    write(&root, "a.php", "");
    write(&root, "b.js", "");
    let files = WalkDirScanner::new(Vec::new())
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: vec![Language::new("php")],
            language_extensions: Vec::new(),
        })
        .expect("scan");
    let rels = rels(&files);
    assert!(rels.contains(&"a.php".to_string()));
    assert!(!rels.contains(&"b.js".to_string()), "non-target languages must be filtered out");
    let _ = std::fs::remove_dir_all(&root);
}

/// The patterns passed to the **constructor** are honoured (the scanner is built once per process).
/// The per-scan channel is a separate thing — see `scan_honours_the_requests_extra_excludes`.
#[test]
fn scan_respects_extra_excludes() {
    let root = scratch("extra");
    write(&root, "secret/code.php", "<?php");
    write(&root, "app/code.php", "<?php");
    let files = WalkDirScanner::new(vec!["secret".to_string()])
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: Vec::new(),
            language_extensions: Vec::new(),
        })
        .expect("scan");
    let rels = rels(&files);
    assert!(rels.contains(&"app/code.php".to_string()));
    assert!(
        !rels.iter().any(|r| r.starts_with("secret/")),
        "extra_excludes must take effect"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `ScanRequest.extra_excludes` must be honoured — not only the patterns given at construction.
///
/// This field is the **only** channel P0 has: `ingest::run` puts the framework's resolved globs plus the
/// project's own there, while the scanner is constructed once per process, long before any scan. A scanner
/// that consulted only its constructor's patterns silently dropped every FKB-resolved exclusion, so
/// framework caches / generated directories were parsed as source.
///
/// The directories below are picked because they are in neither `DEFAULT_EXCLUDE_GLOBS` nor `ASSET_GLOBS`,
/// so a built-in default cannot make this pass for the wrong reason.
#[test]
fn scan_honours_the_requests_extra_excludes() {
    let root = scratch("request-extra");
    write(&root, "app/code.php", "<?php");
    write(&root, "storage/framework/cache/x.php", "<?php");
    write(&root, "bootstrap/cache/y.php", "<?php");

    let request = |extra: Vec<String>| ScanRequest {
        root: root.clone(),
        extra_excludes: extra,
        languages: Vec::new(),
        language_extensions: Vec::new(),
    };

    // Control: with no per-scan excludes these are scanned, so the assertions below can only pass because
    // of the request field.
    let before = rels(
        &WalkDirScanner::new(Vec::new())
            .scan(&request(Vec::new()))
            .expect("scan"),
    );
    assert!(
        before.contains(&"storage/framework/cache/x.php".to_string()),
        "precondition: without the exclude it must be scanned, otherwise this test does not test what it claims: {before:?}"
    );

    let after = rels(
        &WalkDirScanner::new(Vec::new())
            .scan(&request(vec![
                "storage/framework/**".to_string(),
                "bootstrap/cache/**".to_string(),
            ]))
            .expect("scan"),
    );
    assert!(
        after.contains(&"app/code.php".to_string()),
        "ordinary source files must be kept: {after:?}"
    );
    assert!(
        !after.contains(&"storage/framework/cache/x.php".to_string()),
        "the exclude in the request must take effect: {after:?}"
    );
    assert!(
        !after.contains(&"bootstrap/cache/y.php".to_string()),
        "the exclude in the request must take effect: {after:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn scan_skips_large_files() {
    let root = scratch("size");
    write(&root, "big.php", &"x".repeat(5 * 1024 * 1024)); // > 4 MiB
    write(&root, "small.php", "<?php");
    let rels = rels(&scan_all(&root));
    assert!(rels.contains(&"small.php".to_string()));
    assert!(!rels.contains(&"big.php".to_string()), "files larger than 4 MiB must be skipped");
    let _ = std::fs::remove_dir_all(&root);
}

/// `find_markers` must surface real sub-project markers but never a `package.json` buried in `node_modules`.
#[test]
fn find_markers_finds_markers_and_skips_excluded_dirs() {
    let root = scratch("markers");
    write(&root, "composer.json", "{}");
    write(&root, "web/package.json", "{}");
    write(&root, "node_modules/package.json", "{}"); // must NOT be reported
    write(&root, "deep/a/b/package.json", "{}");
    let found = WalkDirScanner::new(Vec::new())
        .find_markers(&root, &["composer.json", "package.json"], 5)
        .expect("find_markers");
    let names: Vec<String> = found
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with("/composer.json")),
        "the root composer.json must be found"
    );
    assert!(
        names.iter().any(|n| n.ends_with("/web/package.json")),
        "web/package.json must be found"
    );
    assert!(
        names.iter().any(|n| n.ends_with("/deep/a/b/package.json")),
        "a marker in a deep directory must be found"
    );
    assert!(
        !names.iter().any(|n| n.contains("node_modules")),
        "a package.json inside node_modules must not count as a sub-project marker"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn find_markers_respects_max_depth() {
    let root = scratch("depth");
    write(&root, "shallow/package.json", "{}");
    write(&root, "deep/a/b/package.json", "{}"); // root0 deep1 a2 b3 file4
    let within = WalkDirScanner::new(Vec::new())
        .find_markers(&root, &["package.json"], 3)
        .expect("find_markers");
    let names: Vec<String> = within
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert!(names.iter().any(|n| n.ends_with("/shallow/package.json")));
    assert!(
        !names.iter().any(|n| n.contains("deep/a/b")),
        "a marker beyond max_depth must not be found"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn normalize_resolves_dot_and_dotdot() {
    assert_eq!(normalize(Path::new("a/./b/../c")), Path::new("a/c"));
    assert_eq!(normalize(Path::new("a/b/../../c")), Path::new("c"));
    assert_eq!(normalize(Path::new("../a")), Path::new("../a"));
}

#[test]
fn scan_errors_on_missing_root() {
    let root = scratch("missing");
    let _ = std::fs::remove_dir_all(&root); // ensure gone
    let res = WalkDirScanner::new(Vec::new()).scan(&ScanRequest {
        root: root.clone(),
        extra_excludes: Vec::new(),
        languages: Vec::new(),
        language_extensions: Vec::new(),
    });
    assert!(res.is_err(), "scan must return an error when the root directory does not exist");
}

/// The parser-registry map (`ScanRequest.language_extensions`) is the **preferred** language source: it shares a
/// source with the sub-project marker table, so "a Go sub-project is detected but no `.go` file is scanned" cannot
/// happen. Every other test passes an empty map, so this pins the map path — using an extension the fallback table
/// does NOT know, which can only be resolved through the map.
#[test]
fn scan_language_extensions_map_is_the_preferred_channel() {
    let root = scratch("langmap");
    write(&root, "svc/main.go", "package main");
    write(&root, "gen/code.unknownext", "x"); // unknown to `language_of_extension`

    // Control: empty map -> the unknown extension is skipped (fallback yields `None`).
    let without = WalkDirScanner::new(Vec::new())
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: Vec::new(),
            language_extensions: Vec::new(),
        })
        .expect("scan");
    assert!(
        !rels(&without).contains(&"gen/code.unknownext".to_string()),
        "precondition: with an empty map an unknown extension must be skipped"
    );

    // With the map declaring it, the file is scanned and tagged with the map's language; the fallback still
    // resolves known extensions (`.go`).
    let map = vec![("customlang".to_string(), vec!["unknownext".to_string()])];
    let with = WalkDirScanner::new(Vec::new())
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: Vec::new(),
            language_extensions: map,
        })
        .expect("scan");
    let lang_of = |r: &str| with.iter().find(|f| f.relative == r).map(|f| f.language.as_str().to_string());
    assert_eq!(lang_of("gen/code.unknownext").as_deref(), Some("customlang"));
    assert_eq!(lang_of("svc/main.go").as_deref(), Some("go"));
    let _ = std::fs::remove_dir_all(&root);
}

/// Exclude globs are compiled case-insensitively: `Vendor/` / `DIST/` are the same mistake in every ecosystem, and a
/// scan exclusion that fails to fire costs a whole directory of noise. The default globs are lowercase, so this
/// exercises the case-insensitive matching against a capitalised directory.
#[test]
fn scan_excludes_are_case_insensitive() {
    let root = scratch("ci");
    write(&root, "Vendor/lib.php", "<?php"); // matches `**/vendor/**`
    write(&root, "DIST/bundle.js", "x"); // matches `**/dist/**`
    write(&root, "app/code.php", "<?php");
    let rels = rels(&scan_all(&root));
    assert!(rels.contains(&"app/code.php".to_string()), "business source must be kept");
    assert!(
        !rels.iter().any(|r| r.to_lowercase().starts_with("vendor/")),
        "Vendor/ must be excluded (case-insensitive)"
    );
    assert!(
        !rels.iter().any(|r| r.to_lowercase().starts_with("dist/")),
        "DIST/ must be excluded (case-insensitive)"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The thin `std::fs` wrappers behind `FileSystem`: existence / dir-ness / read / length, plus the read error
/// mapping to `DomainError` (no panic on a missing path).
#[test]
fn std_file_system_basic_ops() {
    use gt_domain::port::FileSystem;

    let root = scratch("fs");
    let file = root.join("f.txt");
    std::fs::write(&file, "hello").unwrap();
    let dir = root.join("d");
    std::fs::create_dir_all(&dir).unwrap();

    let fs = StdFileSystem::new();
    assert!(fs.exists(&file));
    assert!(fs.exists(&dir));
    assert!(!fs.exists(&root.join("missing")));
    assert!(fs.is_dir(&dir));
    assert!(!fs.is_dir(&file));
    assert_eq!(fs.read_to_string(&file).unwrap(), "hello");
    assert_eq!(fs.len(&file).unwrap(), 5);
    assert!(fs.read_to_string(&root.join("nope")).is_err(), "reading a missing file must return an error rather than panic");
    assert!(fs.len(&root.join("nope")).is_err());
    let _ = std::fs::remove_dir_all(&root);
}
