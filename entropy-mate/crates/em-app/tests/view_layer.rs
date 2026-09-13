//! em-app 视角层集成测试（组装根 → 建图 → 视角切片）。
//!
//! 以真实的 `samples/CRMEB-master` 为原料，经由 `Container` 装配全部适配器，
//! 跑一遍完整建图，再用 `ViewService` 验证一/二级筛选器与各视角切片。
//! 样本缺失时整组跳过（可用 `ENTROPY_MATE_SAMPLE_DIR` 指定）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use em_app::{AppConfig, Container};
use em_application::{PipelineService, ProjectService, ViewService};
use em_domain::model::NewProject;
use em_domain::port::{NoopObserver, Persistence, SystemClock};

/// 在 `CARGO_MANIFEST_DIR` 向上查找 `samples/CRMEB-master`。
fn find_sample() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ENTROPY_MATE_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let cand = cur.join("samples/CRMEB-master");
        if cand.is_dir() {
            return Some(cand);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}

struct Built {
    container: Container,
    project_id: em_domain::model::ProjectId,
}

/// 跑一次完整建图并缓存（同一测试二进制内只跑一遍）。
fn built() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let sample = find_sample()?;
            let data_dir =
                std::env::temp_dir().join(format!("entropy-mate-viewtest-{}", std::process::id()));
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
                container.store.clone() as Arc<dyn Persistence>,
                Arc::new(SystemClock),
            );
            let pipeline = PipelineService::new(
                container.store.clone() as Arc<dyn Persistence>,
                Arc::clone(&container.deps),
            );

            let project = projects
                .create(NewProject {
                    name: "CRMEB".into(),
                    root_path: sample,
                    description: None,
                    config: None,
                })
                .expect("创建工程不应失败");

            pipeline
                .run(project.id, &NoopObserver)
                .expect("对 CRMEB 样本建图不应失败");

            Some(Arc::new(Built {
                container,
                project_id: project.id,
            }))
        })
        .clone()
}

fn view_svc(b: &Built) -> ViewService {
    ViewService::new(b.container.store.clone(), b.container.views())
}

fn skip() -> &'static str {
    "跳过：未找到 CRMEB 样本（可用 ENTROPY_MATE_SAMPLE_DIR 指定）"
}

/// 返回一个确有候选的对象类视角 (perspective_id, center_node_id)。
fn first_object_target(
    views: &ViewService,
    pid: em_domain::model::ProjectId,
) -> Option<(String, em_domain::model::NodeId)> {
    let list = views.perspectives(pid).ok()?;
    for p in &list {
        if p["mode"].as_str() != Some("object") {
            continue;
        }
        if p["available"].as_u64().unwrap_or(0) == 0 {
            continue;
        }
        let id = p["id"].as_str()?.to_string();
        if let Ok(cands) = views.candidates(pid, &id, 50, None) {
            if let Some(c) = cands.first() {
                return Some((id, c.id));
            }
        }
    }
    None
}

#[test]
fn container_assembles_adapters() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    assert!(b.container.framework_count() > 0, "应装载到框架知识");
    let views_provider = b.container.views();
    let registry = views_provider.registry();
    assert!(!registry.perspectives.is_empty(), "应装载到视角声明");
    let ids: Vec<&str> = registry.perspectives.iter().map(|p| p.id.as_str()).collect();
    assert!(
        ids.iter().any(|i| *i == "route" || *i == "table"),
        "视角应至少含 route/table"
    );
    // HTTP 路由能正常装配（冒烟，不启动服务）。
    let _router = b.container.router();
}

#[test]
fn perspectives_reported_with_counts() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let list = views.perspectives(b.project_id).expect("perspectives");
    assert!(!list.is_empty(), "视角列表不应为空");
    let any_available = list
        .iter()
        .any(|v| v["available"].as_u64().unwrap_or(0) > 0);
    assert!(any_available, "图里至少有一个有数据的视角");
}

#[test]
fn aggregate_deploy_unit_clusters() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let agg = views
        .aggregate_view(b.project_id, "deploy_unit", 12)
        .expect("aggregate");
    assert_eq!(agg.perspective, "deploy_unit");
    assert!(
        !agg.clusters.is_empty() || agg.notice.is_some(),
        "deploy_unit 要么有聚类框，要么给出诚实提示"
    );
    assert!(agg.clusters.iter().any(|c| c.count > 0));
}

#[test]
fn aggregate_platform_matrix() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let agg = views
        .aggregate_view(b.project_id, "platform", 12)
        .expect("aggregate");
    let m = agg.matrix.expect("platform 应为矩阵视角");
    assert!(!m.rows.is_empty(), "矩阵应有行");
    assert!(!m.cols.is_empty(), "矩阵应有列");
    assert_eq!(m.cells.len(), m.rows.len());
    assert_eq!(
        m.cells.first().map(|r| r.len()).unwrap_or(0),
        m.cols.len()
    );
    for (i, row) in m.cells.iter().enumerate() {
        assert_eq!(row.iter().sum::<u32>(), m.row_totals[i]);
    }
}

#[test]
fn object_view_chain_and_hidden() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let Some((pid, nid)) = first_object_target(&views, b.project_id) else {
        eprintln!("没有可用的对象类视角候选，跳过 object_view 断言");
        return;
    };
    let ov = views
        .object_view(b.project_id, &pid, nid, Some(2), None)
        .expect("object_view");
    assert_eq!(ov.perspective, pid);
    assert_eq!(ov.center.id, nid);
    assert!(!ov.center.name.is_empty());
    assert_eq!(ov.center.ring, 0, "中心节点应在 0 环");
    assert!(!ov.hidden.note.is_empty(), "必须给出省略说明（诚实性）");
    assert!(!ov.candidates.is_empty(), "应回带二级筛选候选");
}

#[test]
fn object_view_default_is_semantic_only() {
    // 折叠（默认）视图必须「只显示对人类有意义的语义节点 / 语义边」：
    // * 不出现 Method / CallSite / Class 等语法节点与 Calls / HasCallSite 等语法边；
    // * 不出现指向不可见节点的悬空边；
    // * 二级候选按"价值"降序（前端默认打开价值最高的那个）。
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let Some((pid, _)) = first_object_target(&views, b.project_id) else {
        eprintln!("没有可用的对象类视角候选，跳过");
        return;
    };
    // 取价值最高的候选（`candidates` 已按语义依赖价值降序）。
    let cands = views
        .candidates(b.project_id, &pid, 5, None)
        .expect("candidates");
    let Some(top) = cands.first() else {
        eprintln!("该视角没有候选，跳过");
        return;
    };
    assert!(
        top.badge.as_deref().unwrap_or("").contains("语义依赖"),
        "候选 badge 应带价值（语义依赖数），实际 {:?}",
        top.badge
    );

    let ov = views
        .object_view(b.project_id, &pid, top.id, Some(2), None)
        .expect("object_view");

    // 语义节点 = 第一类语义 kind，或带 `category`（外部系统子类型 Cache / Event / Queue…）。
    let is_semantic = |n: &em_domain::model::NodeView| {
        em_domain::model::NodeKind(n.kind.clone()).is_semantic() || n.category.is_some()
    };
    assert!(is_semantic(&ov.center), "中心应是语义节点，实际 {}", ov.center.kind);
    let mut visible = std::collections::HashSet::new();
    visible.insert(ov.center.id.get());
    for n in ov.rings.iter().flatten() {
        visible.insert(n.id.get());
        assert!(
            is_semantic(n),
            "默认视图不应出现语法节点：{} ({})",
            n.name,
            n.kind
        );
    }
    for e in &ov.edges {
        assert!(
            em_domain::model::EdgeKind(e.kind.clone()).is_semantic(),
            "默认视图不应出现语法边：{}",
            e.kind
        );
        assert!(
            visible.contains(&e.from.get()) && visible.contains(&e.to.get()),
            "不应有悬空边：{} {} -> {}",
            e.kind,
            e.from.get(),
            e.to.get()
        );
    }
}

#[test]
fn object_view_resource_center_shows_its_users() {
    // 资源类中心（Table / ConfigKey / Cache…）的关系方向是**反向**的：
    // 语义边由使用者指向资源，所以视图必须回答"谁在用它"，而不是给出一张空图。
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let cands = views
        .candidates(b.project_id, "table", 1, None)
        .expect("candidates");
    let Some(top) = cands.first() else {
        eprintln!("表视角没有候选，跳过");
        return;
    };
    let ov = views
        .object_view(b.project_id, "table", top.id, Some(2), None)
        .expect("object_view");

    assert_eq!(ov.center.kind, "Table", "表视角中心应是 Table");
    assert_eq!(
        ov.center.own_view.as_deref(),
        Some("table"),
        "中心应带回自己的视角 id（供「点击即切」使用）"
    );
    assert!(
        ov.rings.iter().flatten().count() > 0,
        "表视角应给出使用者（谁在读写这张表），而不是空图"
    );
    let mut visible = std::collections::HashSet::new();
    visible.insert(ov.center.id.get());
    for n in ov.rings.iter().flatten() {
        visible.insert(n.id.get());
        assert!(
            em_domain::model::NodeKind(n.kind.clone()).is_semantic() || n.category.is_some(),
            "默认视图不应出现语法节点：{} ({})",
            n.name,
            n.kind
        );
    }
    for e in &ov.edges {
        assert!(
            em_domain::model::EdgeKind(e.kind.clone()).is_semantic(),
            "默认视图不应出现语法边：{}",
            e.kind
        );
        assert!(
            visible.contains(&e.from.get()) && visible.contains(&e.to.get()),
            "不应有悬空边：{} {} -> {}",
            e.kind,
            e.from.get(),
            e.to.get()
        );
    }
}

#[test]
fn node_locations_returns_sources() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let Some(nid) = first_object_target(&views, b.project_id).map(|(_, n)| n) else {
        eprintln!("无对象节点，跳过 locations 断言");
        return;
    };
    let locs = views.node_locations(nid).expect("node_locations");
    assert_eq!(locs.id, nid);
    assert!(
        !locs.locations.is_empty(),
        "语法节点应至少有一条定义位置"
    );
}

#[test]
fn edge_evidence_verifies_chain() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let Some((pid, nid)) = first_object_target(&views, b.project_id) else {
        eprintln!("无对象节点，跳过 edge 断言");
        return;
    };
    // 折叠视图会把语义边"提拉"为合成边（无独立 id），而本测试验证的是**真实链路边**的证据，
    // 因此显式 expand=true 取非折叠视图。
    let ov = views
        .object_view(b.project_id, &pid, nid, Some(1), Some(true))
        .expect("object_view");
    let Some(e) = ov.edges.first() else {
        eprintln!("中心节点没有链路边，跳过 edge_evidence 断言");
        return;
    };
    let ev = views
        .edge_evidence(e.id)
        .expect("edge_evidence")
        .expect("边应存在");
    assert_eq!(ev.edge.id, e.id);
    assert!(
        !ev.locations.is_empty() || ev.reason.is_some(),
        "实边或虚线都应有证据位置或理由"
    );
}
