//! End-to-end: the exclusions P0 resolves from FKB must actually keep those files **out of the scan**.
//!
//! `exclude.rs`'s tests pin the **resolution** (the right globs, with no unresolved diagnostics). This pins
//! the other half — that the resolved globs travel through `ingest::run` into `ScanRequest.extra_excludes`
//! and really keep the files out.
//!
//! Why this is not just another sample-gated test: the only existing assertion of that half lives in
//! `sample_project_pipeline.rs` (which skips without the sample), and it asserts on the sample's `runtime/` — a directory
//! the scanner's **built-in** defaults (`**/runtime/**`) exclude anyway. So it cannot tell "FKB resolved
//! it" from "a built-in caught it".
//!
//! The directories below are chosen for exactly that reason: `storage/framework/**` and `bootstrap/cache/**`
//! come from `fkb/php/laravel.yaml` only — they appear in neither `DEFAULT_EXCLUDE_GLOBS` nor `ASSET_GLOBS`,
//! so the only thing that can keep them out is the FKB-resolved exclusion.

use gt_domain::model::ProjectConfig;
use gt_domain::port::ProjectReader;

mod common;

/// A minimal Laravel project carrying generated content inside the directories its FKB excludes.
fn synthetic_laravel_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-excl-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let files: &[(&str, &str)] = &[
        // Detected as Laravel, which is what makes its exclude rules apply.
        ("composer.json", r#"{"require":{"laravel/framework":"^10"}}"#),
        // Positive control: an ordinary source file, so "nothing was scanned" cannot pass vacuously.
        (
            "app/Http/Controller.php",
            r#"<?php
namespace App\Http;

class Controller
{
    public function index()
    {
        return 1;
    }
}
"#,
        ),
        // Generated / cached content that must never reach the parser.
        (
            "storage/framework/cache/x.php",
            "<?php\nreturn ['cached' => 1];\n",
        ),
        ("bootstrap/cache/y.php", "<?php\nreturn ['compiled' => 1];\n"),
    ];
    for (rel, body) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(&path, body).expect("write");
    }
    dir
}

#[test]
fn fkb_resolved_exclusions_keep_generated_dirs_out_of_the_scan() {
    let root = synthetic_laravel_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Laravel project graph build should succeed");
    };

    let files = b.store.list_files(b.project.id, None).expect("files readable");
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();

    // Positive control first: the scan really ran and picked up ordinary sources.
    assert!(
        paths.iter().any(|p| *p == "app/Http/Controller.php"),
        "an ordinary source file must be scanned, got: {paths:?}"
    );

    for forbidden in ["storage/framework/cache/x.php", "bootstrap/cache/y.php"] {
        assert!(
            !paths.iter().any(|p| *p == forbidden),
            "`{forbidden}` can only be blocked by an exclude rule resolved from the FKB (it is neither a scanner built-in nor an asset directory),\
             but it was still scanned: {paths:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
