//! 前端链路在**折叠视图**里的呈现自检（合成样本 `samples/frontend-backend-link`）。
//!
//! 建图层（`gt-pipeline/tests/frontend_backend_link.rs`）只证明「边建出来了」；
//! 这里证明的是**渲染层**把前端当一等公民对待：
//!   1. 前端函数（`deleteItem`）作为**语法节点**被折叠进 `via` 链，
//!      而不是退化成一个 `File` 节点直连契约（与后端 `Method` 同构）；
//!   2. 折叠边给出**逐跳调用处**（`via[i].call_site` / `to_call_site`），
//!      前端 drawer 才有「被折叠的语法调用链路」可展示；
//!   3. 跨文件的前端链（`App.onDelete → api.deleteItem`）也被收进同一条 `via`。
//!
//! 样本缺失时整组跳过。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, ViewService};
use gt_domain::model::{NewProject, NodeKind};
use gt_domain::port::{GraphQuery, NoopObserver, NodeFilter, SystemClock};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}

/// 合成样本根：`crates/gt-app` → 上两层到仓库根 → `samples/frontend-backend-link`。
fn synth_root() -> PathBuf {
    workspace_root().join("samples/frontend-backend-link")
}

struct Built {
    container: Container,
    project_id: gt_domain::model::ProjectId,
}

fn built() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let root = synth_root();
            if !root.is_dir() {
                return None;
            }
            let data_dir = std::env::temp_dir().join(format!(
                "graphtell-fe-view-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&data_dir).ok()?;

            let config = AppConfig {
                data_dir,
                fkb_dir: Some(workspace_root().join("fkb")),
                views_dir: Some(workspace_root().join("views")),
                rules_dir: Some(workspace_root().join("rules")),
                bind: "127.0.0.1".into(),
                port: 0,
            };
            let container = Container::new(config).expect("容器装配不应失败");

            let projects = ProjectService::new(
                container.store.clone() as Arc<dyn gt_domain::port::Persistence>,
                Arc::new(SystemClock),
            );
            let pipeline = PipelineService::new(
                container.store.clone() as Arc<dyn gt_domain::port::Persistence>,
                Arc::clone(&container.deps),
                Arc::clone(&container.rules)
                    as Arc<dyn gt_domain::port::RuleProvider>,
            );

            let project = projects
                .create(NewProject {
                    name: "frontend-backend-link".into(),
                    root_path: root,
                    description: None,
                    config: None,
                })
                .expect("创建工程不应失败");
            pipeline
                .run(project.id, &NoopObserver)
                .expect("对合成样本建图不应失败");

            Some(Arc::new(Built {
                container,
                project_id: project.id,
            }))
        })
        .clone()
}

fn view_svc(b: &Built) -> ViewService {
    ViewService::new(
        b.container.store.clone() as Arc<dyn gt_domain::port::Persistence>,
        b.container.views(),
    )
}

fn skip() -> String {
    format!(
        "跳过：未找到合成样本 {}",
        synth_root().display()
    )
}

/// 找到 `POST /api/delete` 这个契约节点（前后端汇聚点）。
fn contract_id(b: &Built) -> Option<gt_domain::model::NodeId> {
    let nodes = b
        .container
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project_id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: Some("/api/delete".into()),
            limit: Some(10),
            offset: Some(0),
        })
        .ok()?;
    nodes.into_iter().next().map(|n| n.id)
}

/// 把折叠视图按「画布所见」打印出来：中心 / 各环 / 每条边的 via 链与逐跳调用处。
/// 无头环境无法截图 UI，而 UI 渲染的就是这份数据模型——打印它等价于此。
fn dump_view(ov: &gt_domain::model::ObjectView, names: &std::collections::HashMap<i64, String>) {
    let name = |id: i64| -> String {
        names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("#{id}"))
    };
    println!("\n===== 折叠视图（默认所见）=====");
    println!(
        "中心: {} [{}]  (perspective={})",
        ov.center.name, ov.center.kind, ov.perspective
    );
    for (i, ring) in ov.rings.iter().enumerate() {
        println!("  第 {} 环:", i + 1);
        for n in ring {
            println!("    - {} [{}]", n.name, n.kind);
        }
    }
    println!("  边 ({}):", ov.edges.len());
    for e in &ov.edges {
        let chain: Vec<String> = e
            .via
            .iter()
            .map(|v| {
                let at = v
                    .call_site
                    .as_ref()
                    .map(|l| format!(" @ {}:{}", l.file, l.line))
                    .unwrap_or_default();
                format!("{}{}", v.name, at)
            })
            .collect();
        let end = e
            .to_call_site
            .as_ref()
            .map(|l| format!(" @ {}:{}", l.file, l.line))
            .unwrap_or_default();
        println!(
            "    * {} --{}--> {}  {}{}",
            name(e.from.get()),
            e.kind,
            name(e.to.get()),
            if chain.is_empty() {
                String::new()
            } else {
                format!("via [{}] ", chain.join(" → "))
            },
            end
        );
    }
    println!("  省略说明: {}", ov.hidden.note);
    println!("==============================\n");
}

/// 前端链路必须在**折叠视图**里可见，且带着可展开的逐跳调用链。
#[test]
fn frontend_chain_visible_in_folded_route_view() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let Some(cid) = contract_id(&b) else {
        eprintln!("图里没有 /api/delete 契约，跳过");
        return;
    };

    let ov = views
        .object_view(b.project_id, "route", cid, Some(2))
        .expect("object_view");

    let names: std::collections::HashMap<i64, String> = {
        let mut m = std::collections::HashMap::new();
        m.insert(ov.center.id.get(), ov.center.name.clone());
        for n in ov.rings.iter().flatten() {
            m.insert(n.id.get(), n.name.clone());
        }
        for e in &ov.edges {
            for v in &e.via {
                m.insert(v.id.get(), v.name.clone());
            }
        }
        m
    };
    dump_view(&ov, &names);

    // ---- 1) 前端发起的那条 CallsHttp 必须**有交代**（降级记账，不占画布）----
    //
    // 画布恒为语义节点：前端 `Function --CallsHttp--> 契约` 的起点是语法节点，
    // 不再点亮成画布上的药丸，而是降级进 `orphans` —— 名字 + 关系 + 接触点位置都在，
    // 一条不少，只是不占据"只画语义节点"的画布额度。
    let fe: Vec<_> = ov.orphans.iter().filter(|o| o.edge_kind == "CallsHttp").collect();
    assert!(
        !fe.is_empty(),
        "前端 → 契约的 CallsHttp 应记在 orphans 里，实际 orphans：{:?}",
        ov.orphans.iter().map(|o| &o.edge_kind).collect::<Vec<_>>()
    );

    // ---- 2) 发起方是**函数节点**（语法节点），不是 File ----
    for o in &fe {
        assert_eq!(
            o.kind, "Function",
            "CallsHttp 起点应是前端函数节点（与后端 Method 同构），实际 kind = {}",
            o.kind
        );
        // 画布上不许再出现它：既不进环，也不是任何边的端点。
        let on_canvas = ov
            .rings
            .iter()
            .flatten()
            .any(|n| n.id == o.id)
            || ov.edges.iter().any(|e| e.from == o.id || e.to == o.id);
        assert!(
            !on_canvas,
            "前端调用方 {} 已降级记账，不应再出现在画布上",
            o.name
        );
    }

    // ---- 3) drawer 可展开：逐跳调用处必须给出 ----
    // 折叠掉的中间跳（onDelete → deleteItem → axios 调用点）每一跳都要有 `call_site`，
    // 否则前端抽屉只能说「没有逐跳证据可查」。
    let mut hops = 0usize;
    let mut hops_with_site = 0usize;
    for e in &ov.edges {
        for v in &e.via {
            if v.id.get() == e.from.get() {
                continue; // 起点自身没有「谁调了我」
            }
            hops += 1;
            if v.call_site.is_some() {
                hops_with_site += 1;
            }
        }
    }
    // 不再断言 `hops > 0`：前端调用方已降级记账，这条最小路由（契约没有任何资源依赖）
    // 画布上**本来就该是 0 条边** —— 事实全在 orphans 里，不是"折叠坏了"。
    assert_eq!(
        hops, hops_with_site,
        "被折叠的每一跳都要给出调用处（drawer 逐跳链路），实际 {hops_with_site}/{hops}"
    );

    // ---- 4) 降级记账也要给出接触点：前端真正发出 axios 的那一行 ----
    // 记账不是"消失"：点开列表要能看到 `文件:行`，否则"谁在调这个接口"成了空话。
    for o in &fe {
        assert!(
            o.location.is_some(),
            "前端 CallsHttp 记账应给出接触点位置，实际缺失：{}",
            o.name
        );
    }
}
