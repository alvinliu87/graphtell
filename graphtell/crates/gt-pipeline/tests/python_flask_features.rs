//! Flask（Python）路由契约的端到端自检。
//!
//! 目的之一是验证「同一语言的第 N 个框架几乎免费」：本用例不依赖任何新的解析器
//! 能力，只靠 `fkb/python/flask.yaml` + 已有的 Python 解析器（装饰器即调用点）。

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_flask_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-flask-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    // pyproject.toml 既是子工程标记（→ python），也是 detector 的 manifest；
    // 注意其中**不含** fastapi，避免 FastAPI 的 FKB 一起命中。
    std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"demo\"\ndependencies = [\"flask>=2.3\"]\n")
        .expect("write pyproject");
    std::fs::write(dir.join("requirements.txt"), "flask>=2.3\n").expect("write requirements");

    std::fs::write(
        dir.join("app.py"),
        r#"from flask import Flask

app = Flask(__name__)


@app.route("/")
def index():
    return "ok"


@app.route("/orders", methods=["POST"])
def create_order():
    return "ok"


@app.get("/health")
def health():
    return "ok"
"#,
    )
    .expect("write app.py");

    dir
}

fn nodes_of_kind(b: &common::Built, kind: &str) -> Vec<Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
}

/// （契约节点名 → 经 HandledBy 连到的处理函数名）
fn contract_targets(b: &common::Built) -> Vec<(String, String)> {
    nodes_of_kind(b, "HttpContract")
        .iter()
        .map(|n| {
            let target: String = b
                .store
                .edges_of(n.id, EdgeDirection::Outgoing)
                .expect("edges")
                .iter()
                .filter(|e| e.kind.as_str() == "HandledBy")
                .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
                .map(|t| t.name.clone())
                .next()
                .unwrap_or_default();
            (n.name.clone(), target)
        })
        .collect()
}

#[test]
fn flask_routes_produce_http_contracts_linked_to_handlers() {
    let root = synthetic_flask_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成 Flask 工程建图应成功");
    };
    let pairs = contract_targets(&b);
    assert!(
        pairs.len() >= 3,
        "三条路由都应落成 HttpContract，实际：{pairs:?}"
    );

    // `@app.route("/")` —— 未声明 methods：Flask 默认仅响应 GET，
    // 与引擎对 ContractId 的 GET 兜底一致
    assert!(
        pairs.iter().any(|(c, t)| c == "GET /" && t == "index"),
        "应产出 GET / → index，实际：{pairs:?}"
    );
    // `@app.route("/orders", methods=["POST"])` —— methods= 已解码，方法取首项
    assert!(
        pairs.iter().any(|(c, t)| c == "POST /orders" && t == "create_order"),
        "应产出 POST /orders → create_order，实际：{pairs:?}"
    );
    // `@app.get("/health")` —— 动词快捷方式，方法从装饰器名推导
    assert!(
        pairs.iter().any(|(c, t)| c == "GET /health" && t == "health"),
        "应产出 GET /health → health，实际：{pairs:?}"
    );
}
