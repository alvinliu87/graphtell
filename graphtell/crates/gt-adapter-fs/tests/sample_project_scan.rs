//! Tests of the file-scanning adapter against the real sample_project sample.
//!
//! Verifies two things: ① sub-project marker files are found (composer.json / package.json);
//! ② dependency and asset directories are excluded correctly by the built-in rules.

use std::path::Path;

use gt_adapter_fs::WalkDirScanner;
use gt_domain::port::ScanRequest;
use gt_domain::port::FileScanner;
use gt_sample_support::{missing_hint, sample_root};

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn finds_sub_project_markers() {
    let Some(root) = sample_root() else {
        panic!("{}", missing_hint());
    };
    let scanner = WalkDirScanner::new(Vec::new());
    let markers = scanner
        .find_markers(Path::new(&root), &["composer.json", "package.json"], 4)
        .expect("scanning for marker files");
    let names: Vec<String> = markers
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "composer.json"),
        "expected to find the backend composer.json"
    );
    assert!(
        names.iter().any(|n| n == "package.json"),
        "expected to find the frontend package.json"
    );
}

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn scan_excludes_vendor_and_assets() {
    let Some(root) = sample_root() else {
        panic!("{}", missing_hint());
    };
    let scanner = WalkDirScanner::new(Vec::new());
    let files = scanner
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: Vec::new(), // empty = every supported language
            language_extensions: Vec::new(), // empty = fall back to the scanner built-in extension table
        })
        .expect("scan");

    assert!(!files.is_empty(), "expected source files to be scanned");

    for forbidden in ["/vendor/", "/node_modules/", "/target/", "/.git/"] {
        let leaked = files
            .iter()
            .any(|f| f.path.to_string_lossy().contains(forbidden));
        assert!(!leaked, "the scan result must not contain {forbidden}");
    }
    // Static asset extensions should be excluded
    let has_asset = files
        .iter()
        .any(|f| {
            let name = f.path.to_string_lossy().to_ascii_lowercase();
            name.ends_with(".png")
                || name.ends_with(".jpg")
                || name.ends_with(".woff2")
                || name.ends_with(".zip")
        });
    assert!(!has_asset, "static assets must not enter the set to analyse");

    // Business source code must be present (assert on the file name, not the sample's internal
    // directory layout, so the check survives a sample checkout being renamed).
    assert!(
        files
            .iter()
            .any(|f| f.relative.ends_with("app/event.php")),
        "app/event.php must be in the scan result"
    );
}
