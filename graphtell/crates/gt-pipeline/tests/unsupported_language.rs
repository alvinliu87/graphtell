//! 「被识别却没有解析器」的语言必须**可见**，而不是静默产出空图。
//!
//! `MARKERS` 里已有 `go.mod` / `Cargo.toml` 等子工程标记，扩展名兜底表也能把
//! `.go` / `.rs` 扫进文件列表；但 `gt-adapter-parser` 并没有对应解析器。于是这些
//! 工程会**被识别成子工程**，却在 P2 一个语法事实都产不出 —— 工程图静默为空，
//! 而流水线照常报成功。Python 在补上解析器之前正是这个状态，且无人察觉
//! （当时 `cf_ast` 里是一句光秃秃的 `continue`）。
//!
//! 本用例锁定：这类语言必须产出一条 `NoParserForLanguage` 诊断。

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
        panic!("合成 Go 工程建图应成功");
    };

    // 前提：子工程**确实被识别**（否则这个陷阱根本不成立）
    assert!(
        b.outcome.sub_projects.iter().any(|s| s.language.as_str() == "go"),
        "go 子工程应被 go.mod 识别，实际：{:?}",
        b.outcome
            .sub_projects
            .iter()
            .map(|s| (s.language.as_str(), s.detected_by.as_str()))
            .collect::<Vec<_>>()
    );

    // 关键：不再静默 —— CfAst 阶段必须产出一条点名该语言的诊断
    let report = common::phase_report(&b, "CfAst").expect("应有 CfAst 阶段报告");
    let diag = report
        .diagnostics
        .iter()
        .find(|d| d.code == "NoParserForLanguage")
        .unwrap_or_else(|| {
            panic!(
                "应报 NoParserForLanguage 诊断，实际诊断：{:?}",
                report
                    .diagnostics
                    .iter()
                    .map(|d| (d.code.as_str(), d.message.as_str()))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        diag.message.contains("go"),
        "诊断应点名语言，实际：{}",
        diag.message
    );
    assert_eq!(diag.severity, Severity::Warning);

    // 同时坐实「语义层为空」：子工程在，但一个类节点都没有
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
    assert!(classes.is_empty(), "无解析器时不应产出任何类节点");

    // 降级而非丢弃：**结构层仍要有** —— 至少能看见这个工程有哪些文件。
    // 否则用户面对空画布无从判断"工程没东西"还是"工具不支持"。
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
        "无解析器时仍应建出 File 节点（结构层），实际：{:?}",
        files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>()
    );
}
