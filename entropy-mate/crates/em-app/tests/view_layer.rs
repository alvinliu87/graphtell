//! em-app 视角层集成测试（组装根 → 建图 → 视角切片）。
//!
//! 以真实的 `samples/CRMEB-master` 为原料，经由 `Container` 装配全部适配器，
//! 跑一遍完整建图，再用 `ViewService` 验证一/二级筛选器与各视角切片。
//! 样本缺失时整组跳过（可用 `ENTROPY_MATE_SAMPLE_DIR` 指定）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use em_app::{AppConfig, Container};
use em_application::{PipelineService, ProjectService, ViewService};
use em_domain::model::{NewProject, NodeKind};
use em_domain::port::{
    EdgeDirection, GraphQuery, NoopObserver, NodeFilter, Persistence, SystemClock,
};

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
        .object_view(b.project_id, &pid, top.id, Some(2))
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
        .object_view(b.project_id, "table", top.id, Some(2))
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
        // **方向**必须保持「使用者 --语义边--> 资源」：`ReadsDb` / `ReadsCache` / `WritesDb`
        // 这些谓语的主语是使用者，`to` 才是资源。曾按"由内向外"把两端对调，
        // 于是每条边都成了「资源 → 使用者」（表现为 `表·cache --读库--> GET /verify_code`），
        // 箭头、hover 卡的 `from → to`、Inspector 的「起点 / 终点」全部与谓语语义相反。
        assert_eq!(
            e.to.get(),
            ov.center.id.get(),
            "资源视角的边必须指向中心（使用者 → 资源），实际 {} -> {}",
            e.from.get(),
            e.to.get()
        );
        assert_ne!(
            e.from.get(),
            ov.center.id.get(),
            "资源视角的边不应从中心出发（{e:?}）"
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
    let Some((_pid, nid)) = first_object_target(&views, b.project_id) else {
        eprintln!("无对象节点，跳过 edge 断言");
        return;
    };
    // `edge_evidence` 验证的是**真实链路边**的证据，所以要直接用库里的真实边 id：
    // 视图里的边在折叠后经过"提拉"（一条真实边可能对应多条视图边），
    // 反向视角（资源类中心）的边更是合成出来的（id 为负），都不能当证据 id 用。
    let store = &b.container.store;
    let edges = store
        .edges_of(nid, EdgeDirection::Both)
        .expect("edges_of 不应失败");
    let Some(e) = edges.into_iter().next() else {
        eprintln!("中心节点没有任何边，跳过 edge_evidence 断言");
        return;
    };
    let ev = views
        .edge_evidence(e.id.get())
        .expect("edge_evidence")
        .expect("边应存在");
    assert_eq!(ev.edge.id, e.id.get());
    assert!(
        !ev.locations.is_empty() || ev.reason.is_some(),
        "实边或虚线都应有证据位置或理由"
    );
}

/// 折叠视图里**经过折叠**的语义边，其 `via` 末端必须落在**真实接触点**上：
/// 该接触点自己持有一条指向该资源的**直接**语义边（P5 命中，带 `evidence`）。
///
/// 反例：P8 传播边只陈述"上游可达该资源"，它**不是路径**。若拿它当 `via` 末端，
/// 链路就断在发现深度上，画出"路由自己读了缓存"这种伪路径（真实接触点在几跳之外）。
/// 这条不变量与边的种类（读库 / 读缓存 / 投递…）无关，对任何仓库都应成立。
#[test]
fn folded_semantic_edges_end_at_real_contact() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let store = &b.container.store;
    let cands = match views.candidates(b.project_id, "route", 6, None) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("无 route 候选，跳过");
            return;
        }
    };
    assert!(!cands.is_empty(), "route 视角应有候选");

    let mut checked = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for c in cands.iter().take(6) {
        let Ok(ov) = views.object_view(b.project_id, "route", c.id, Some(2)) else {
            continue;
        };
        for e in &ov.edges {
            // 只看经过折叠的（`via` 非空的）语义边；直连边由起点自己负责。
            let Some(contact) = e.via.last() else { continue };
            checked += 1;
            let outs = store
                .edges_of(contact.id, EdgeDirection::Outgoing)
                .unwrap_or_default();
            let has_direct = outs.iter().any(|r| {
                r.kind.as_str() == e.kind
                    && r.to_id == e.to
                    && r.properties.get("evidence").is_some()
            });
            if !has_direct {
                violations.push(format!(
                    "路由 {} 的边 --{}--> #{} 末端接触点是 {:?}（#{}），但它没有带 evidence 的直接边",
                    c.name, e.kind, e.to.get(), contact.name, contact.id.get()
                ));
            }
        }
    }
    assert!(checked > 0, "应检查到折叠边，实际一条都没有");
    assert!(
        violations.is_empty(),
        "存在断在发现深度上的伪路径：\n{}",
        violations.join("\n")
    );
}

/// 具体回归：`GET /v2/order/invoice_detail/:uni` 到 `Cache` 曾画出 3 条 `ReadsCache`，
/// 且每条的 `via` 都是截断的（有一条甚至只有 `[detail]`，等于断言 `detail` 自己读缓存）。
///
/// 真实情况只有**两条完整到达路径**，且都汇到同一个接触点 `CacheService::remember`：
///   detail → tidyOrder → SystemConfigService::more → CacheService::remember
///   detail → getQRCodePath → UploadService::init → SystemConfigService::more → CacheService::remember
#[test]
fn invoice_detail_route_cache_edges_have_complete_paths() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let store = &b.container.store;
    let nodes = store
        .query_nodes(&NodeFilter {
            project_id: b.project_id,
            kind: Some(NodeKind("HttpContract".into())),
            name_contains: Some("order/invoice_detail".into()),
            limit: Some(5),
            offset: Some(0),
        })
        .expect("query_nodes");
    let Some(contract) = nodes.first() else {
        eprintln!("图里没有 invoice_detail 路由，跳过");
        return;
    };
    let ov = views
        .object_view(b.project_id, "route", contract.id, Some(2))
        .expect("object_view");

    let cache_edges: Vec<_> = ov.edges.iter().filter(|e| e.kind == "ReadsCache").collect();
    let desc: Vec<String> = cache_edges
        .iter()
        .map(|e| {
            let chain: Vec<String> = e.via.iter().map(|v| v.name.clone()).collect();
            format!("[{}]", chain.join(" → "))
        })
        .collect();
    assert_eq!(
        cache_edges.len(),
        2,
        "应只有两条完整到达路径（tidyOrder / getQRCodePath 两条分支），实际 {}: {:?}",
        cache_edges.len(),
        desc
    );
    for e in &cache_edges {
        let last = e
            .via
            .last()
            .unwrap_or_else(|| panic!("缓存边应经过折叠链，实际 via 为空：{desc:?}"));
        assert_eq!(
            last.name, "remember",
            "via 必须落在真正的接触点 CacheService::remember 上，实际末端是 {:?}（整链 {desc:?}）",
            last.name
        );
        assert!(e.indirect, "路由自身不读缓存，应标记为间接（虚线）");
        assert!(
            e.to_call_site.is_some(),
            "应给出本链路访问缓存的位置（CacheService.php 的 Cache::tag()->remember()）"
        );
    }
    // 曾经的伪路径形态：via 只有一跳，等于说 handler 自己读了缓存。
    assert!(
        !cache_edges.iter().any(|e| e.via.len() <= 1),
        "不应再出现单跳的断尾伪路径：{desc:?}"
    );
}

/// 回归：路由契约 → handler（`detail`）这一跳必须给出「调用处」（路由注册行），
/// 而不是在折叠链第一跳就显示「未解析到调用语句」。
///
/// 折叠链里每一跳的 `via[i].call_site` 是「上一跳调用本跳的语句」：
/// `tidyOrder` 的调用处是 `StoreOrderInvoiceController.php:120`（在 `detail` 体内），
/// 那么 `detail` 自己的调用处就应该是**路由注册处**（`Route::get('invoice_detail', …)`）。
/// `HttpContract` 是合成节点（无 `file_id`），`node_source_location` 返回 `None`，
/// 故 `call_site_between` 改用 `node_locations` 取它汇聚的路由文件+行号。
#[test]
fn invoice_detail_route_first_hop_has_call_site() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let store = &b.container.store;
    let nodes = store
        .query_nodes(&NodeFilter {
            project_id: b.project_id,
            kind: Some(NodeKind("HttpContract".into())),
            name_contains: Some("order/invoice_detail".into()),
            limit: Some(5),
            offset: Some(0),
        })
        .expect("query_nodes");
    let Some(contract) = nodes.first() else {
        eprintln!("图里没有 invoice_detail 路由，跳过");
        return;
    };
    let ov = views
        .object_view(b.project_id, "route", contract.id, Some(2))
        .expect("object_view");

    let mut checked = 0usize;
    for e in &ov.edges {
        let Some(first) = e.via.first() else { continue };
        if first.name != "detail" {
            continue;
        }
        checked += 1;
        assert!(
            first.call_site.is_some(),
            "路由 → handler 首跳（{name}）应给出调用处（路由注册行），实际为 None（会显示「未解析到调用语句」）",
            name = first.name
        );
    }
    assert!(checked > 0, "应检查到至少一条经由 detail 的折叠边");
}

/// **特征测试（characterization test）**：钉住 `object_view` 对一个固定路由的完整输出形状。
///
/// 存在的唯一目的：`object_view` 是个近千行的折叠流程，将来拆分 / 优化时，
/// 任何"顺手改坏"都必须在这里立刻暴露 —— 边数、按种类的分布、via 长度分布、
/// 间接边与证据覆盖率、可见环分布，任一项变了都说明行为变了。
///
/// 它不是"正确性"断言（正确性是下面两个用例的事），而是**行为不变**的护栏。
#[test]
fn object_view_characterization_invoice_detail() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let store = &b.container.store;
    let nodes = store
        .query_nodes(&NodeFilter {
            project_id: b.project_id,
            kind: Some(NodeKind("HttpContract".into())),
            name_contains: Some("order/invoice_detail".into()),
            limit: Some(5),
            offset: Some(0),
        })
        .expect("query_nodes");
    let Some(contract) = nodes.first() else {
        eprintln!("图里没有 invoice_detail 路由，跳过");
        return;
    };
    let ov = views
        .object_view(b.project_id, "route", contract.id, Some(2))
        .expect("object_view");

    let mut by_kind: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut via_len: Vec<usize> = Vec::new();
    let mut indirect = 0usize;
    let mut with_loc = 0usize;
    for e in &ov.edges {
        *by_kind.entry(e.kind.clone()).or_default() += 1;
        via_len.push(e.via.len());
        if e.indirect {
            indirect += 1;
        }
        if e.to_call_site.is_some() {
            with_loc += 1;
        }
    }
    via_len.sort_unstable();

    assert_eq!(ov.edges.len(), 29, "边总数变了：{:?}", by_kind);
    assert_eq!(
        by_kind.get("ReadsCache").copied().unwrap_or(0),
        2,
        "ReadsCache 边数变了"
    );
    assert_eq!(
        by_kind.get("ReadsConfig").copied().unwrap_or(0),
        27,
        "ReadsConfig 边数变了"
    );
    assert_eq!(indirect, ov.edges.len(), "全部都是提拉/传播得来的间接边，变了说明 indirect 判定被改坏");
    assert_eq!(with_loc, ov.edges.len(), "每条边都应能给出资源访问位置，变了说明证据选取被改坏");
    // 关键：**最长链必须到 5 跳**（detail → getQRCodePath → init → more → remember），
    // 若折叠/回溯被改坏，最长链会退回 2~3 跳。
    assert_eq!(
        via_len.last().copied().unwrap_or(0),
        5,
        "最长 via 链应为 5 跳，实际分布 {via_len:?}"
    );
    assert!(
        via_len.iter().filter(|&&l| l == 1).count() >= 6,
        "应有多条 1 跳的直接边（detail 自己读的配置），实际 {via_len:?}"
    );
}

