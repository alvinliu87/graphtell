//! Tests of the file-scanning adapter against the real sample_project sample.
//!
//! Verifies two things: ① sub-project marker files are found (composer.json / package.json);
//! ② dependency and asset directories are excluded correctly by the built-in rules.

use std::path::{Path, PathBuf};

use gt_adapter_fs::WalkDirScanner;
use gt_domain::port::ScanRequest;
use gt_domain::port::FileScanner;

fn sample_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    // Search upward from `CARGO_MANIFEST_DIR` for `samples/**/CRMEB-master`:
    // try `samples/CRMEB-master` first, then walk one level of sub-directories (so samples grouped by tech stack also match).
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let samples = cur.join("samples");
        let direct = samples.join("CRMEB-master");
        if direct.is_dir() {
            return Some(direct.canonicalize().unwrap_or(direct));
        }
        let mut hits: Vec<PathBuf> = std::fs::read_dir(&samples)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("CRMEB-master"))
            .filter(|p| p.is_dir())
            .collect();
        hits.sort();
        if let Some(hit) = hits.into_iter().next() {
            return Some(hit.canonicalize().unwrap_or(hit));
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn finds_sub_project_markers() {
    let Some(root) = sample_root() else {
        panic!("sample_project sample missing: this test is marked #[ignore]; running it with --ignored requires GRAPHTELL_SAMPLE_DIR or a samples/**/CRMEB-master checkout");
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
        panic!("sample_project sample missing: this test is marked #[ignore]; running it with --ignored requires GRAPHTELL_SAMPLE_DIR or a samples/**/CRMEB-master checkout");
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

    // Business source code must be present
    assert!(
        files
            .iter()
            .any(|f| f.relative == "crmeb/app/event.php"),
        "app/event.php must be in the scan result"
    );
}
