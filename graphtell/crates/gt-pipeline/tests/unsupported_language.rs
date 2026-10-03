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
