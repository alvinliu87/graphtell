//! 用**真实开源样本**验证 Laravel / ThinkPHP 的建图效果。
//!
//! 样本从哪来：本目录 `samples/php-projects/laravel-starter`（官方 `laravel/laravel`
//! 骨架，`git` 忽略、不入库，由开发者下载）。样本缺失时测试**跳过**而非失败，以便 CI
//! 无样本也能跑。
//!
//! 重点验证：Laravel 11 中间件别名能静态还原成类并挂到契约上——
//! * 框架默认别名（`throttle` ……来自 `vendor/`，由 FKB `laravel-default-aliases` 声明）；
//! * 应用级自定义别名（`logreq` ……来自 `bootstrap/app.php` 的 `$middleware->alias([...])`）。

mod common;

use std::path::PathBuf;

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery};

/// 定位真实 Laravel 样本根目录：先试环境变量 `GRAPHTELL_PHP_SAMPLE_DIR`，
/// 再回退到仓库内相对路径 `samples/php-projects/laravel-starter`。
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

/// 某语义节点的某类**入边**是否存在（如 HttpContract ← Middleware 的 PassesThrough）。
fn has_incoming_edge(b: &common::Built, kind: &str, edge: &str) -> bool {
    nodes_of_kind(b, kind).iter().any(|n| {
        b.store
            .edges_of(n.id, EdgeDirection::Incoming)
            .expect("edges")
            .iter()
            .any(|e| e.kind.as_str() == edge)
    })
}

/// Laravel 11 中间件别名：`throttle`（框架默认）与 `logreq`（应用自定义）都应被还原成类，
/// 并让使用它们的路由经 `PassesThrough` 连到对应 `Middleware` 节点。
#[test]
fn laravel_real_sample_middleware_alias() {
    let Some(root) = php_sample() else {
        eprintln!("跳过：未找到 Laravel 样本（samples/php-projects/laravel-starter）");
        return;
    };
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("Laravel 样本建图应成功");
    };

    let mws: Vec<String> = nodes_of_kind(&b, "Middleware")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    eprintln!("Laravel 样本 Middleware 节点：{:?}", mws);

    // 框架默认别名：`throttle` → Illuminate\Routing\Middleware\ThrottleRequests
    assert!(
        mws.iter().any(|n| n.contains("ThrottleRequests")),
        "框架默认别名 `throttle` 应还原成 ThrottleRequests，实际：{mws:?}"
    );
    // 应用级自定义别名：`logreq` → App\Http\Middleware\LogRequest
    assert!(
        mws.iter().any(|n| n.contains("LogRequest")),
        "应用级别名 `logreq`（bootstrap/app.php）应还原成 LogRequest，实际：{mws:?}"
    );

    // 端到端：用别名的路由应经 PassesThrough 连到 Middleware 节点。
    assert!(
        has_incoming_edge(&b, "Middleware", "PassesThrough"),
        "至少一条契约应经 PassesThrough 连到 Middleware（别名已还原并挂载）"
    );

    let count = nodes_of_kind(&b, "Middleware")
        .iter()
        .flat_map(|n| b.store.edges_of(n.id, EdgeDirection::Incoming).expect("edges"))
        .filter(|e| e.kind.as_str() == "PassesThrough")
        .count();
    eprintln!("Laravel 样本守卫（PassesThrough）边数 = {count}");
}
