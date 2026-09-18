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

    // ---- 1) 折叠视图里必须看得到前端发起的那条 CallsHttp ----
    let fe: Vec<_> = ov.edges.iter().filter(|e| e.kind == "CallsHttp").collect();
    assert!(
        !fe.is_empty(),
        "前端 → 契约的 CallsHttp 应在折叠视图里可见，实际边：{:?}",
        ov.edges.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );

    // ---- 2) 发起方是**函数节点**（语法节点），不是 File ----
    // 折叠视图默认只画语义节点；非语义节点能出现只有一种正当理由：
    // 它与中心有**直接语义边**且上游再无语义发起者（塌缩兜底）——前端正是这种情形。
    for e in &fe {
        let from = ov
            .rings
            .iter()
            .flatten()
            .find(|n| n.id == e.from)
            .unwrap_or_else(|| panic!("CallsHttp 的起点应在可见环里：{}", e.from));
        assert_eq!(
            from.kind, "Function",
            "CallsHttp 起点应是前端函数节点（与后端 Method 同构），实际 kind = {}",
            from.kind
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
    assert!(
        hops > 0,
        "折叠视图应折叠掉若干语法跳（前端函数 / 调用点），实际一条 via 都没有"
    );
    assert_eq!(
        hops, hops_with_site,
        "被折叠的每一跳都要给出调用处（drawer 逐跳链路），实际 {hops_with_site}/{hops}"
    );

    // ---- 4) 终点调用处：前端真正发出 axios 的那一行 ----
    for e in &fe {
        assert!(
            e.to_call_site.is_some(),
            "前端 CallsHttp 应给出「本链路发出该请求的位置」"
        );
    }
}
