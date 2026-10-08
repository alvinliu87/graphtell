//! Detector coverage on the **real** samples — a diagnostic, `#[ignore]`d because it needs `samples/`.
//!
//! What it is for: library knowledge is now split out of the framework files and gated on detection, so the
//! question that decides whether that is safe is **do the detectors ever miss a library the code really
//! uses?** A miss is silent — the knowledge just does not switch on, and the rules that needed it quietly
//! stop firing with nothing reported.
//!
//! So this prints, per sample, a ground-truth count (does the source actually mention the library?) beside
//! whether the detectors fired. `used>0, detected=no` is a **miss**, and each one is an argument against the
//! split, or a detector that needs one more signal.
//!
//! Run: `cargo test -p gt-pipeline --test detector_coverage -- --ignored --nocapture`
//!
//! By default this is a **report**: it prints each `(used, detected)` row and the miss count but still passes,
//! so it can be eyeballed in a dev loop. To turn it into a real regression gate (fail the test the moment any
//! probe is `used>0, detected=no`), set `GT_DETECTOR_FAIL_ON_MISS=1` — e.g. in the CI job that runs against the
//! real samples. The gate is opt-in because the substring ground truth can false-positive (a library named in a
//! docblock without being used), and we do not want a benign sample to break a default run.

mod common;

use std::path::{Path, PathBuf};

use gt_domain::model::ProjectConfig;
use gt_domain::port::ProjectReader;

/// Directories that are never "the project's own code": dependencies, build output, generated assets.
const SKIP_DIRS: &[&str] = &[
    "vendor",
    "node_modules",
    ".git",
    "public",
    "storage",
    "runtime",
    "target",
    "dist",
    "build",
];

/// One library under test: which FKB id, and what counts as "the code really uses it".
struct Probe {
    /// The FKB id whose detectors are being measured.
    lib: &'static str,
    /// Any one of these occurring in the project's own sources counts as "used".
    needles: &'static [&'static str],
    /// File extensions considered "the project's own sources" for this probe.
    exts: &'static [&'static str],
}

const PHP_PROBES: &[Probe] = &[
    Probe {
        lib: "guzzle",
        // Deliberately the **class**, not the `GuzzleHttp` namespace: a project can lean on that namespace
        // heavily (`Psr7`, `Exception`) or name it only in a `@throws` docblock without ever making a request,
        // and then there is nothing for the `external_calls` entries to match.
        needles: &["GuzzleHttp\\Client"],
        exts: &["php"],
    },
    Probe {
        lib: "illuminate-database",
        needles: &["DB::transaction"],
        exts: &["php"],
    },
    // The remaining PHP frameworks the detector can recognise (from FKB) — each pinned against a precise
    // in-source signal so a miss is a real "detector did not switch on", not a docblock coincidence.
    Probe {
        lib: "laravel",
        needles: &["Illuminate\\Support\\Facades", "Illuminate\\Foundation\\"],
        exts: &["php"],
    },
    Probe {
        lib: "symfony",
        needles: &["Symfony\\"],
        exts: &["php"],
    },
    Probe {
        lib: "thinkphp",
        needles: &["think\\", "Think\\"],
        exts: &["php"],
    },
    Probe {
        lib: "spatie-permission",
        needles: &["Spatie\\Permission", "givePermissionTo", "HasRoles"],
        exts: &["php"],
    },
];

const JAVA_PROBES: &[Probe] = &[
    Probe {
        lib: "mybatis",
        // `org.apache.ibatis` covers annotation-driven mappers; bare `mybatis` covers the starter in
        // `pom.xml` and the mapper XML doctype — i.e. the XML-only style that has no Java import at all.
        needles: &["org.apache.ibatis", "mybatis"],
        exts: &["java", "xml"],
    },
    Probe { lib: "spring-amqp", needles: &["RabbitListener"], exts: &["java"] },
    Probe { lib: "spring-kafka", needles: &["KafkaListener"], exts: &["java"] },
    // The remaining Java frameworks the detector can recognise (from FKB).
    Probe {
        lib: "spring-boot",
        needles: &["org.springframework.boot"],
        exts: &["java"],
    },
    Probe {
        lib: "spring-cache",
        needles: &["@Cacheable", "@CacheEvict", "spring.cache"],
        exts: &["java"],
    },
    Probe {
        lib: "spring-jpa",
        needles: &["JpaRepository", "jakarta.persistence", "javax.persistence"],
        exts: &["java"],
    },
];

/// How often any of `needles` occurs in the project's own sources (by extension).
fn count_occurrences(root: &Path, needles: &[&str], exts: &[&str]) -> usize {
    fn walk(dir: &Path, needles: &[&str], exts: &[&str], out: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skip = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| SKIP_DIRS.contains(&n));
                if !skip {
                    walk(&path, needles, exts, out);
                }
                continue;
            }
            let wanted = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e));
            if !wanted {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                *out += needles.iter().map(|n| text.matches(n).count()).sum::<usize>();
            }
        }
    }
    let mut n = 0;
    walk(root, needles, exts, &mut n);
    n
}

/// Every sample under `samples/<stack-dir>`, as `(label, root)`.
fn samples_under(stack_dir: &str) -> Vec<(String, PathBuf)> {
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let base = loop {
        let candidate = cur.join("samples").join(stack_dir);
        if candidate.is_dir() {
            break Some(candidate);
        }
        if !cur.pop() {
            break None;
        }
    };
    let Some(base) = base else { return Vec::new() };

    let mut out = Vec::new();
    // `php-projects` groups by framework (laravel/, thinkphp/), `java-projects` is flat.
    let mut groups: Vec<PathBuf> = if stack_dir == "php-projects" {
        ["laravel", "thinkphp"].iter().map(|g| base.join(g)).collect()
    } else {
        vec![base.clone()]
    };
    groups.retain(|g| g.is_dir());
    for group in groups {
        let Ok(rd) = std::fs::read_dir(&group) else { continue };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for p in dirs {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
            out.push((name, p));
        }
    }
    out
}

fn report(label: &str, root: &Path, probes: &[Probe], counts: &mut (usize, usize)) {
    let Some(built) = common::graph_with_root(root, ProjectConfig::default()) else {
        eprintln!("{label:<24} graphing failed, skipping");
        return;
    };
    let frameworks: Vec<String> = built
        .store
        .list_sub_projects(built.project.id)
        .unwrap_or_default()
        .into_iter()
        .flat_map(|s| s.frameworks)
        .collect();

    eprintln!("{label}");
    for probe in probes {
        let used = count_occurrences(root, probe.needles, probe.exts);
        let detected = frameworks.iter().any(|f| f == probe.lib);
        let miss = used > 0 && !detected;
        if miss {
            counts.1 += 1;
        }
        eprintln!(
            "    {:<22} used={:<5} detected={:<5} {}",
            probe.lib,
            used,
            detected,
            if miss { "<-- MISS" } else { "" }
        );
    }
    eprintln!("    frameworks={frameworks:?}");
    counts.0 += 1;
}

#[test]
#[ignore]
fn detector_coverage_on_real_samples() {
    let mut counts = (0usize, 0usize); // (samples, misses)

    for (label, root) in samples_under("php-projects") {
        report(&format!("php/{label}"), &root, PHP_PROBES, &mut counts);
    }
    for (label, root) in samples_under("java-projects") {
        report(&format!("java/{label}"), &root, JAVA_PROBES, &mut counts);
    }

    let (rows, misses) = counts;
    assert!(rows >= 3, "too few real samples ({rows}), so the coverage conclusion is not trustworthy");
    eprintln!("\n{rows} samples, {misses} missed (used>0 but detected=no)");

    // Opt-in hard gate: only when `GT_DETECTOR_FAIL_ON_MISS` is set do misses fail the test. Off by default so a
    // substring false-positive (a lib named in a docblock) cannot break a normal `--ignored` run.
    if misses > 0 && std::env::var("GT_DETECTOR_FAIL_ON_MISS").is_ok() {
        panic!(
            "{misses} detector miss(es) (library used in source but not detected as a framework); \
             the gated knowledge for it would stay switched off. See the MISS rows printed above."
        );
    }
}
