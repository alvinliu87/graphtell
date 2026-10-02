//! Verify the graph-building result for Laravel / ThinkPHP against **real open-source samples**.
//!
//! Where the samples come from: this directory's `samples/php-projects/laravel-starter` (the official
//! `laravel/laravel` skeleton; `git`-ignored, not committed, downloaded by the developer). When a sample is
//! missing the test **skips** rather than fails, so CI can run without samples.
//!
//! The focus: Laravel 11 middleware aliases can be resolved statically back into classes and attached to contracts
//! —
//! * framework default aliases (`throttle` … from `vendor/`, declared by the FKB `laravel-default-aliases`);
//! * application-level custom aliases (`logreq` … from `$middleware->alias([...])` in `bootstrap/app.php`).

mod common;

use std::path::PathBuf;

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery};

/// Locate the real Laravel sample root: try the environment variable `GRAPHTELL_PHP_SAMPLE_DIR` first,
/// then fall back to the in-repo relative path `samples/php-projects/laravel-starter`.
fn php_sample() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_PHP_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..7 {
        let laravel = cur.join("samples/php-projects/laravel-starter");
        if laravel.is_dir() {
            return Some(laravel);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

fn nodes_of_kind<'a>(b: &'a common::Built, kind: &str) -> Vec<gt_domain::model::Node> {
    b.store
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            ..Default::default()
        })
        .expect("query_nodes")
        .into_iter()
        .collect()
}

/// Whether a semantic node has an in-edge of a given kind (e.g. HttpContract <- Middleware via PassesThrough).
fn has_incoming_edge(b: &common::Built, kind: &str, edge: &str) -> bool {
    nodes_of_kind(b, kind).iter().any(|n| {
        b.store
            .edges_of(n.id, EdgeDirection::Incoming)
            .expect("edges")
            .iter()
            .any(|e| e.kind.as_str() == edge)
    })
}

/// Laravel 11 middleware aliases: both `throttle` (framework default) and `logreq` (application-defined) should be
/// resolved back into classes, and the routes using them should connect to the corresponding `Middleware` node via
/// `PassesThrough`.
#[test]
fn laravel_real_sample_middleware_alias() {
    let Some(root) = php_sample() else {
        eprintln!("skip: Laravel sample not found (samples/php-projects/laravel-starter)");
        return;
    };
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the Laravel sample should succeed");
    };

    let mws: Vec<String> = nodes_of_kind(&b, "Middleware")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    eprintln!("Laravel sample Middleware nodes: {:?}", mws);

    // A framework default alias: `throttle` -> Illuminate\Routing\Middleware\ThrottleRequests
    assert!(
        mws.iter().any(|n| n.contains("ThrottleRequests")),
        "the framework default alias `throttle` should resolve to ThrottleRequests, got: {mws:?}"
    );
    // An application-level custom alias: `logreq` -> App\Http\Middleware\LogRequest
    assert!(
        mws.iter().any(|n| n.contains("LogRequest")),
        "the application-level alias `logreq` (bootstrap/app.php) should resolve to LogRequest, got: {mws:?}"
    );

    // End to end: a route using an alias should connect to a Middleware node via PassesThrough.
    assert!(
        has_incoming_edge(&b, "Middleware", "PassesThrough"),
        "at least one contract should connect to a Middleware via PassesThrough (alias resolved and attached)"
    );

    let count = nodes_of_kind(&b, "Middleware")
        .iter()
        .flat_map(|n| b.store.edges_of(n.id, EdgeDirection::Incoming).expect("edges"))
        .filter(|e| e.kind.as_str() == "PassesThrough")
        .count();
    eprintln!("Laravel sample guard (PassesThrough) edge count = {count}");
}
