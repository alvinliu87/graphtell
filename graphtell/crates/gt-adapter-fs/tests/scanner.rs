//! Self-contained tests of the file-scanning adapter (no external sample needed).
//!
//! `crmeb_scan.rs` is `#[ignore]`d because the CRMEB sample is too large to ship, so it never runs in CI.
//! This file pins the scanner's core invariants on synthetic temp-dir trees so the exclusion rules, marker
//! lookup and language inference stay guarded on every build.

use std::path::Path;

use gt_adapter_fs::WalkDirScanner;
use gt_adapter_fs::system::normalize;
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
    assert!(rels.contains(&"app/foo.php".to_string()), "业务源码必须被扫描");
    for forbidden in ["vendor/", "node_modules/", "static/", "dist/", "target/", "build/"] {
        assert!(
            !rels.iter().any(|r| r.starts_with(forbidden)),
            "扫描结果不得包含 {forbidden}"
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
            "扩展名 {ext} 必须排除"
        );
    }
    assert!(rels.iter().any(|r| r == "app.go"), "go 源码必须保留");
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
/// framework-resolved rules (`crmeb/runtime/**`, `public/static/**`, `storage/logs/**`) expressible at
/// all — a name has nowhere to say "only this one, at this depth".
#[test]
fn scan_matches_extra_excludes_as_path_globs() {
    let root = scratch("globs");
    write(&root, "app/controller/Index.php", "<?php");
    write(&root, "app/generated/a/b.php", "<?php");
    write(&root, "admin/generated/c.php", "<?php");
    write(&root, "crmeb/service/Order.php", "<?php");
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
    assert!(rels.contains(&"crmeb/service/Order.php".to_string()));
    assert!(
        !rels.iter().any(|r| r.starts_with("app/generated/")),
        "app/generated 必须排除: {rels:?}"
    );
    assert!(
        rels.contains(&"admin/generated/c.php".to_string()),
        "同名目录在别的路径下必须保留: {rels:?}"
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
    assert!(!rels.iter().any(|r| r.ends_with(".md")), ".md 不应被扫描");
    assert!(!rels.contains(&"readme".to_string()), "无扩展名文件不应被扫描");
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
    assert!(!rels.contains(&"b.js".to_string()), "非目标语言应被过滤");
    let _ = std::fs::remove_dir_all(&root);
}

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
        "extra_excludes 应生效"
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
    assert!(!rels.contains(&"big.php".to_string()), "超过 4 MiB 的文件应被跳过");
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
        "应找到根 composer.json"
    );
    assert!(
        names.iter().any(|n| n.ends_with("/web/package.json")),
        "应找到 web/package.json"
    );
    assert!(
        names.iter().any(|n| n.ends_with("/deep/a/b/package.json")),
        "深目录 marker 应被找到"
    );
    assert!(
        !names.iter().any(|n| n.contains("node_modules")),
        "node_modules 内的 package.json 不应被当作子工程标记"
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
        "超过 max_depth 的 marker 不应被找到"
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
    assert!(res.is_err(), "根目录不存在时 scan 应返回错误");
}
