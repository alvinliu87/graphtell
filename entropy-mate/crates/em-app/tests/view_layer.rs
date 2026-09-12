//! em-app 视角层集成测试（组装根 → 建图 → 视角切片）。
//!
//! 以真实的 `分析样本/CRMEB-master` 为原料，经由 `Container` 装配全部适配器，
//! 跑一遍完整建图，再用 `ViewService` 验证一/二级筛选器与各视角切片。
//! 样本缺失时整组跳过（可用 `ENTROPY_MATE_SAMPLE_DIR` 指定）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use em_app::{AppConfig, Container};
use em_application::{PipelineService, ProjectService, ViewService};
use em_domain::model::NewProject;
use em_domain::port::{NoopObserver, Persistence, SystemClock};

/// 在 `CARGO_MANIFEST_DIR` 向上查找 `分析样本/CRMEB-master`。
fn find_sample() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ENTROPY_MATE_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let cand = cur.join("分析样本/CRMEB-master");
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
        if let Ok(cands) = views.candidates(pid, &id, 50) {
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
        .object_view(b.project_id, &pid, nid, Some(2))
        .expect("object_view");
    assert_eq!(ov.perspective, pid);
    assert_eq!(ov.center.id, nid);
    assert!(!ov.center.name.is_empty());
    assert_eq!(ov.center.ring, 0, "中心节点应在 0 环");
    assert!(!ov.hidden.note.is_empty(), "必须给出省略说明（诚实性）");
    assert!(!ov.candidates.is_empty(), "应回带二级筛选候选");
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
    let ov = views
        .object_view(b.project_id, &pid, nid, Some(1))
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
