//! End-to-end: P9 Taint wiring — a SQL sink whose argument interpolates a variable traced back to request
//! input must be annotated `tainted_raw` (critical), while a sink fed by a local file read (no request
//! source) must be `tainted_raw_unknown` (conservative warning), and a bare `->where($cond)` (no embedded
//! variable) must NOT be flagged at all.
//!
//! The unit tests in `taint.rs` pin the pieces (backward trace, variable marker, keyword, spec resolution).
//! What they cannot see is the wiring: the call sites and variable assignments are produced by the **PHP
//! parser** and the taint vocabulary is loaded from **FKB** (`fkb/php/common.yaml`, `apply_without_detection`)
//! into `ctx.taint_default`; `run` then reads `ctx.ws.calls` / `variable_assignments` / `lang_policy`. If any
//! of those wires broke, P9 would judge nothing — silently — and every SQL-injection gap would be invisible.
//!
//! Driven by `graph_with_root` (real FKB + a synthetic PHP tree), so it needs no external sample_project sample.

use gt_domain::model::ProjectConfig;
use gt_domain::port::GraphQuery;

mod common;

fn synthetic_php_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-taint-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/controller")).expect("mkdir");

    // A composer.json so the marker detects PHP and the PHP parser sets the `$` variable prefix.
    let composer = r#"{
  "name": "demo/app",
  "require": { "php": ">=8.0" }
}
"#;
    let php = r#"<?php
namespace app\controller;

class UserController
{
    // (A) confirmed: a request variable flows into raw SQL
    public function leak()
    {
        $id = input('id');
        $sql = "SELECT * FROM user WHERE id = {$id}";
        Db::query($sql);
    }

    // (B) source unknown: SQL built from a local file read, not request input
    public function fromFile()
    {
        $sql = $this->loadSql();
        Db::query($sql);
    }

    // (C) where-condition interpolation from request input
    public function whereInterp()
    {
        $roles = request()->param('roles');
        $this->db->where("CONCAT(',',roles,',') LIKE '%,$roles,%'");
    }

    // (D) bare where with no embedded variable -> must NOT be flagged
    public function whereBare()
    {
        $cond = $this->buildCond();
        $this->db->where($cond);
    }

    private function loadSql()
    {
        return "SELECT * FROM version";
    }

    private function buildCond()
    {
        return "status = 1";
    }
}
"#;
    std::fs::write(dir.join("composer.json"), composer).expect("write");
    std::fs::write(dir.join("app/controller/UserController.php"), php).expect("write");
    dir
}

#[test]
fn raw_sql_with_request_var_is_critical_and_local_source_is_only_unknown() {
    let root = synthetic_php_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP project graph build should succeed");
    };

    let anns: Vec<_> = b
        .store
        .annotations_of_project(b.project.id)
        .expect("annotations")
        .into_values()
        .flatten()
        .collect();
    let kinds: Vec<&str> = anns.iter().map(|a| a.kind.as_str()).collect();

    // (A) confirmed-from-request raw SQL -> critical `tainted_raw`.
    assert!(
        kinds.contains(&"tainted_raw"),
        "expected a tainted_raw annotation, got: {:?}",
        kinds
    );
    // (B) local source -> conservative `tainted_raw_unknown`, NOT a false critical.
    assert!(
        kinds.contains(&"tainted_raw_unknown"),
        "expected a tainted_raw_unknown annotation, got: {:?}",
        kinds
    );
    // (C) where-condition interpolation from request -> `tainted_where`.
    assert!(
        kinds.contains(&"tainted_where"),
        "expected a tainted_where annotation, got: {:?}",
        kinds
    );
    // (D) bare where must not be flagged at all.
    assert!(
        !kinds.contains(&"tainted_where_unknown"),
        "bare where must not be flagged, got: {:?}",
        kinds
    );

    // Exactly the three expected annotations, and no silent extras.
    let total = anns
        .iter()
        .filter(|a| {
            matches!(
                a.kind.as_str(),
                "tainted_raw" | "tainted_where" | "tainted_raw_unknown" | "tainted_where_unknown"
            )
        })
        .count();
    assert_eq!(total, 3, "expected exactly 3 taint annotations, got: {:?}", kinds);

    // The critical annotation carries the right shape.
    let crit = anns.iter().find(|a| a.kind == "tainted_raw").expect("tainted_raw exists");
    assert_eq!(crit.subkind.as_deref(), Some("SqlInjection"));
    assert_eq!(crit.channel.0, "Taint");
    assert!((crit.confidence - 0.9).abs() < 1e-3);

    let _ = std::fs::remove_dir_all(&root);
}

/// A second synthetic controller pinning sink spellings that the first controller never exercised — the e2e wiring
/// for these is exactly what `taint.rs`'s unit tests cannot see (the call sites come from the **PHP parser** and the
/// sink vocab from **FKB**):
///  - `Db::execute` is the *other* receiver-bound `raw_sql_sink` (the first controller only exercised `Db::query`);
///  - `whereRaw` is a **bare** `raw_sql_sink` (no `receiver:` in FKB, so it must match *whatever* receiver) — the
///    `Some(r) => receiver == r` vs `None => true` branch in `is_raw_sql_sink` had no e2e coverage;
///  - `whereOr` is the *other* `where_interp_sink` (the first controller only exercised `where`);
///  - a bare `whereRaw` fed by a local constant (unknown source) must degrade to `tainted_raw_unknown`, pinning that
///    the bare-sink path also honours the "unknown source -> warning" branch.
fn synthetic_php_root_extra() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-taint-extra-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/controller")).expect("mkdir");

    let composer = r#"{
  "name": "demo/app",
  "require": { "php": ">=8.0" }
}
"#;
    let php = r#"<?php
namespace app\controller;

class TaintMoreController
{
    // (E) the other receiver-bound raw sink: Db::execute with a request var -> tainted_raw.
    public function execLeak()
    {
        $id = input('id');
        $sql = "DELETE FROM user WHERE id = {$id}";
        Db::execute($sql);
    }

    // (F) bare raw sink (no receiver binding in FKB): whereRaw with an embedded request var -> tainted_raw.
    public function rawLeak()
    {
        $id = input('id');
        $sql = "UPDATE user SET v = 1 WHERE id = {$id}";
        $this->db->whereRaw($sql);
    }

    // (G) the other where-interp sink: whereOr with an embedded request var -> tainted_where.
    public function orLeak()
    {
        $roles = request()->param('roles');
        $this->db->whereOr("CONCAT(',',roles,',') LIKE '%,$roles,%'");
    }

    // (H) bare raw sink with an UNKNOWN source (local constant) -> conservative tainted_raw_unknown.
    public function rawUnknown()
    {
        $sql = $this->localSql();
        $this->db->whereRaw($sql);
    }

    private function localSql()
    {
        return "DELETE FROM version";
    }
}
"#;
    std::fs::write(dir.join("composer.json"), composer).expect("write");
    std::fs::write(dir.join("app/controller/TaintMoreController.php"), php).expect("write");
    dir
}

#[test]
fn taint_wiring_covers_execute_whereraw_whereor_and_unknown_bare_sink() {
    let root = synthetic_php_root_extra();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP project graph build should succeed");
    };

    let anns: Vec<_> = b
        .store
        .annotations_of_project(b.project.id)
        .expect("annotations")
        .into_values()
        .flatten()
        .collect();
    let kinds: Vec<&str> = anns.iter().map(|a| a.kind.as_str()).collect();

    // (E) Db::execute (receiver-bound raw sink) -> tainted_raw.
    assert!(
        kinds.contains(&"tainted_raw"),
        "expected a tainted_raw annotation (Db::execute / whereRaw), got: {:?}",
        kinds
    );
    // (G) whereOr (where-interp sink) -> tainted_where.
    assert!(
        kinds.contains(&"tainted_where"),
        "expected a tainted_where annotation (whereOr), got: {:?}",
        kinds
    );
    // (H) bare whereRaw with unknown source -> tainted_raw_unknown (warning), not a false critical.
    assert!(
        kinds.contains(&"tainted_raw_unknown"),
        "expected a tainted_raw_unknown annotation (bare whereRaw from local source), got: {:?}",
        kinds
    );
    // No where-interp-with-unknown was produced.
    assert!(
        !kinds.contains(&"tainted_where_unknown"),
        "no tainted_where_unknown expected, got: {:?}",
        kinds
    );

    // Exactly the four expected taint annotations (E, F, G, H).
    let total = anns
        .iter()
        .filter(|a| {
            matches!(
                a.kind.as_str(),
                "tainted_raw" | "tainted_where" | "tainted_raw_unknown" | "tainted_where_unknown"
            )
        })
        .count();
    assert_eq!(total, 4, "expected exactly 4 taint annotations, got: {:?}", kinds);

    // Per-kind breakdown pins each sink spelling independently:
    //  - 2x tainted_raw: E (Db::execute, receiver-bound) AND F (whereRaw, bare sink) — proves the `receiver: None`
    //    branch in `is_raw_sql_sink` wires through;
    //  - 1x tainted_where: G (whereOr, the other where-interp sink);
    //  - 1x tainted_raw_unknown: H (bare whereRaw fed by a local constant).
    let n_raw = anns.iter().filter(|a| a.kind == "tainted_raw").count();
    let n_where = anns.iter().filter(|a| a.kind == "tainted_where").count();
    let n_raw_unknown = anns.iter().filter(|a| a.kind == "tainted_raw_unknown").count();
    assert_eq!(n_raw, 2, "expected 2 tainted_raw (Db::execute + bare whereRaw), got: {:?}", kinds);
    assert_eq!(n_where, 1, "expected 1 tainted_where (whereOr), got: {:?}", kinds);
    assert_eq!(
        n_raw_unknown, 1,
        "expected 1 tainted_raw_unknown (bare whereRaw, unknown source), got: {:?}",
        kinds
    );

    // The bare-sink path reuses the same critical shape as the receiver-bound sink.
    let crit = anns.iter().find(|a| a.kind == "tainted_raw").expect("tainted_raw exists");
    assert_eq!(crit.subkind.as_deref(), Some("SqlInjection"));
    assert_eq!(crit.channel.0, "Taint");
    assert!((crit.confidence - 0.9).abs() < 1e-3);

    let _ = std::fs::remove_dir_all(&root);
}
