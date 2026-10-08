//! A language that "is recognised but has no parser" must be **visible**, not silently produce an empty graph.
//!
//! The marker table (`MarkerProvider`, default `gt-adapter-techstack`) already has sub-project markers like
//! `go.mod` / `Cargo.toml`, and the extension fallback table can
//! also scan `.go` / `.rs` into the file list; but `gt-adapter-parser` has no parser for them. So these projects
//! **get recognised as sub-projects** yet produce not a single syntax fact in P2 — the project graph is silently
//! empty while the pipeline still reports success. Python was exactly in this state before its parser was added,
//! and nobody noticed (at the time `cf_ast` was a bare `continue`).
//!
//! This case pins: such a language must produce a `NoParserForLanguage` diagnostic.

use gt_domain::model::{NodeKind, ProjectConfig, Severity};
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

fn synthetic_go_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-go-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    std::fs::write(dir.join("go.mod"), "module demo\n\ngo 1.21\n").expect("write go.mod");
    std::fs::write(
        dir.join("main.go"),
        r#"package main

type Server struct {
	Addr string
}

func (s *Server) Run() error {
	return nil
}

func main() {
	s := &Server{Addr: ":8080"}
	_ = s.Run()
}
"#,
    )
    .expect("write main.go");

    dir
}

#[test]
fn unsupported_language_is_reported_instead_of_silently_skipped() {
    let root = synthetic_go_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Go project should succeed");
    };

    // Precondition: the sub-project really **is** recognised (otherwise this trap does not exist at all)
    assert!(
        b.outcome.sub_projects.iter().any(|s| s.language.as_str() == "go"),
        "the go sub-project should be recognised via go.mod, got: {:?}",
        b.outcome
            .sub_projects
            .iter()
            .map(|s| (s.language.as_str(), s.detected_by.as_str()))
            .collect::<Vec<_>>()
    );

    // The key point: not silent — the CfAst phase must produce a diagnostic naming that language
    let report = common::phase_report(&b, "CfAst").expect("expected a CfAst phase report");
    let diag = report
        .diagnostics
        .iter()
        .find(|d| d.code == "NoParserForLanguage")
        .unwrap_or_else(|| {
            panic!(
                "expected a NoParserForLanguage diagnostic, actual diagnostics: {:?}",
                report
                    .diagnostics
                    .iter()
                    .map(|d| (d.code.as_str(), d.message.as_str()))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        diag.message.contains("go"),
        "the diagnostic should name the language, got: {}",
        diag.message
    );
    assert_eq!(diag.severity, Severity::Warning);

    // Also confirm "the semantic layer is empty": the sub-project exists but has not one class node
    let classes = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Class".to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("query");
    assert!(classes.is_empty(), "with no parser, no class node should be produced");

    // Degrade rather than discard: **the structural layer must still be there** — at least the project's files are visible.
    // Otherwise, facing an empty canvas the user cannot tell "the project has nothing" from "the tool cannot handle it".
    let files = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("File".to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("query");
    assert!(
        files.iter().any(|f| f.name.ends_with("main.go")),
        "with no parser, File nodes (the structural layer) should still be built, got: {:?}",
        files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>()
    );
}

/// A mixed repo: one sub-project is parserless (`go`), another is fully supported (`php`). This pins the
/// per-sub-project scoping of `NoParserForLanguage` — a pure-Go project alone cannot reveal a regression that
/// flags *every* sub-project regardless of whether it has a parser. The go side must be warned, the php side must
/// NOT, and php must still be parsed into real semantic nodes (a Class).
fn synthetic_go_php_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-go-php-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("svc")).expect("mkdir svc");
    std::fs::write(dir.join("svc/go.mod"), "module demo\n\ngo 1.21\n").expect("write go.mod");
    std::fs::write(
        dir.join("svc/main.go"),
        "package main\n\nfunc main() {}\n",
    )
    .expect("write main.go");

    std::fs::create_dir_all(dir.join("app")).expect("mkdir app");
    std::fs::write(dir.join("app/composer.json"), "{\"require\":{\"php\":\">=8.0\"}}\n").expect("write composer.json");
    std::fs::write(
        dir.join("app/User.php"),
        "<?php\nnamespace app;\nclass User {\n    public function name(): string { return 'x'; }\n}\n",
    )
    .expect("write User.php");

    dir
}

#[test]
fn unsupported_language_diagnostic_is_scoped_per_sub_project() {
    let root = synthetic_go_php_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the mixed go+php project should succeed");
    };

    // Precondition: both sub-projects recognised (otherwise the scoping question is moot).
    assert!(
        b.outcome.sub_projects.iter().any(|s| s.language.as_str() == "go"),
        "the go sub-project should be recognised, got: {:?}",
        b.outcome
            .sub_projects
            .iter()
            .map(|s| (s.language.as_str(), s.detected_by.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(
        b.outcome.sub_projects.iter().any(|s| s.language.as_str() == "php"),
        "the php sub-project should be recognised, got: {:?}",
        b.outcome
            .sub_projects
            .iter()
            .map(|s| (s.language.as_str(), s.detected_by.as_str()))
            .collect::<Vec<_>>()
    );

    let report = common::phase_report(&b, "CfAst").expect("expected a CfAst phase report");
    let no_parser: Vec<&_> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "NoParserForLanguage")
        .collect();

    // Exactly one, and it must name `go` — never `php` (php has a parser).
    assert_eq!(
        no_parser.len(),
        1,
        "exactly one NoParserForLanguage expected (per language), got: {:?}",
        no_parser
            .iter()
            .map(|d| (d.code.as_str(), d.message.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(
        no_parser[0].message.contains("go"),
        "the diagnostic must name go, got: {}",
        no_parser[0].message
    );
    assert!(
        !no_parser.iter().any(|d| d.message.contains("php")),
        "php has a parser and must NOT be flagged as unsupported"
    );
    assert_eq!(no_parser[0].severity, Severity::Warning);

    // The supported sibling is parsed normally: it yields a Class node.
    let classes = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Class".to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("query");
    assert!(
        classes.iter().any(|c| c.name == "User"),
        "php must be parsed into a Class node, got: {:?}",
        classes.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
    );

    // The parserless sibling still keeps its structural layer.
    let files = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("File".to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("query");
    assert!(
        files.iter().any(|f| f.name.ends_with("main.go")),
        "go files must still be built as File nodes"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The diagnostic is aggregated **per language**, not per file (cf_ast.rs:60 — "not per file, to avoid flooding").
/// Two `.go` files must still produce exactly one `NoParserForLanguage`, and both files must still reach the
/// structural layer as `File` nodes.
fn synthetic_go_root_two_files() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-go-2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    std::fs::write(dir.join("go.mod"), "module demo\n\ngo 1.21\n").expect("write go.mod");
    std::fs::write(
        dir.join("main.go"),
        "package main\n\nfunc main() {}\n",
    )
    .expect("write main.go");
    std::fs::write(
        dir.join("util.go"),
        "package main\n\nfunc Help() string { return \"\" }\n",
    )
    .expect("write util.go");

    dir
}

#[test]
fn unsupported_language_diagnostic_is_per_language_not_per_file() {
    let root = synthetic_go_root_two_files();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the two-file Go project should succeed");
    };

    let report = common::phase_report(&b, "CfAst").expect("expected a CfAst phase report");
    let no_parser: Vec<&_> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "NoParserForLanguage")
        .collect();
    assert_eq!(
        no_parser.len(),
        1,
        "the diagnostic must be aggregated per-language, not per file (2 .go files should still yield 1), got: {}",
        no_parser.len()
    );
    assert!(
        no_parser[0].message.contains("go"),
        "the diagnostic should name the language, got: {}",
        no_parser[0].message
    );
    assert_eq!(no_parser[0].severity, Severity::Warning);

    // The structural layer is still built for *both* files (degrade, don't drop).
    let files = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("File".to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("query");
    let go_files: Vec<&str> = files
        .iter()
        .filter(|f| f.name.ends_with(".go"))
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        go_files.len() >= 2,
        "both .go files should be built as File nodes, got: {:?}",
        go_files
    );

    let _ = std::fs::remove_dir_all(&root);
}
