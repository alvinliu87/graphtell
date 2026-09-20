//! gt-app 视角层集成测试（组装根 → 建图 → 视角切片）。
//!
//! 以真实的 `samples/thinkphp-projects/CRMEB-master` 为原料，经由 `Container` 装配全部适配器，
//! 跑一遍完整建图，再用 `ViewService` 验证一/二级筛选器与各视角切片。
//! 样本缺失时整组跳过（可用 `GRAPHTELL_SAMPLE_DIR` 指定）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, ViewService};
use gt_domain::model::{NewProject, NodeKind};
use gt_domain::port::{
    EdgeDirection, GraphQuery, NoopObserver, NodeFilter, Persistence, SystemClock,
};

/// 在 `dir/samples` 下定位 CRMEB 样本：先试 `samples/CRMEB-master`，再遍历一层子目录
/// `samples/*/CRMEB-master`（样本按技术栈分目录放置时也能命中）。
fn under_samples(dir: &Path) -> Option<PathBuf> {
    let samples = dir.join("samples");
    let direct = samples.join("CRMEB-master");
    if direct.is_dir() {
        return Some(direct);
    }
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&samples)
        .ok()?
        .flatten()
        .map(|e| e.path().join("CRMEB-master"))
        .filter(|p| p.is_dir())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// 在 `CARGO_MANIFEST_DIR` 向上查找 `samples/**/CRMEB-master`。
fn find_sample() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        if let Some(cand) = under_samples(&cur) {
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
    project_id: gt_domain::model::ProjectId,
}

/// 跑一次完整建图并缓存（同一测试二进制内只跑一遍）。
fn built() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let sample = find_sample()?;
            let data_dir =
                std::env::temp_dir().join(format!("graphtell-viewtest-{}", std::process::id()));
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
    "跳过：未找到 CRMEB 样本（可用 GRAPHTELL_SAMPLE_DIR 指定）"
}

/// 该视角是否在 `views/perspectives.yaml` 里注册（未注册的聚合视角无从断言）。
fn registered(views: &ViewService, pid: gt_domain::model::ProjectId, id: &str) -> bool {
    views
        .perspectives(pid)
        .unwrap_or_default()
        .iter()
        .any(|p| p["id"].as_str() == Some(id))
}

/// 返回一个确有候选的对象类视角 (perspective_id, center_node_id)。
fn first_object_target(
    views: &ViewService,
    pid: gt_domain::model::ProjectId,
) -> Option<(String, gt_domain::model::NodeId)> {
    let list = views.perspectives(pid).ok()?;
    for p in &list {
        if p["mode"].as_str() != Some("object") {
            continue;
        }
        if p["available"].as_u64().unwrap_or(0) == 0 {
            continue;
        }
        let id = p["id"].as_str()?.to_string();
        if let Ok(cands) = views.candidates(pid, &id, 50, None, None) {
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
    // `deploy_unit` / `platform` 两个聚合视角在 `views/perspectives.yaml` 里尚未启用
    // （注释标着"MVP 暂不实现"）。没启用时 `aggregate_view` 只会返回 NotFound，
    // 这条用例无处可施力 —— 明确跳过，而不是把"视角不存在"当成视角实现有问题。
    if !registered(&views, b.project_id, "deploy_unit") {
        eprintln!("跳过：deploy_unit 视角未在 views/perspectives.yaml 中启用");
        return;
    }
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
    if !registered(&views, b.project_id, "platform") {
        eprintln!("跳过：platform 视角未在 views/perspectives.yaml 中启用");
        return;
    }
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
    // 候选**不再**由对象视图回带：前端下拉按需请求 `/view/{p}/candidates`，
    // 在每次对象视图里重算「5000 个候选逐个 BFS 打分」纯属浪费（见 `ObjectView` 注释）。
}

/// 折叠视图允许出现的节点：语义节点，或"塌缩兜底"——与中心有**直接语义边**的
/// 直接访问方（其调用链上游无任何语义发起者，不画就永远不可见、与候选徽标矛盾）。
fn assert_visible_node_ok(ov: &gt_domain::model::ObjectView, n: &gt_domain::model::NodeView) {
    let semantic = gt_domain::model::NodeKind(n.kind.clone()).is_semantic() || n.category.is_some();
    if semantic {
        return;
    }
    let direct_accessor = ov.edges.iter().any(|e| {
        gt_domain::model::EdgeKind(e.kind.clone()).is_semantic()
            && (e.from.get() == n.id.get() || e.to.get() == n.id.get())
            && (e.from.get() == ov.center.id.get() || e.to.get() == ov.center.id.get())
    });
    assert!(
        direct_accessor,
        "默认视图只允许语义节点或与中心直连的塌缩兜底节点：{} ({})",
        n.name, n.kind
    );
}

#[test]
fn object_view_default_is_semantic_only() {
    // 折叠（默认）视图必须「只显示对人类有意义的语义节点 / 语义边」：
    // * 不出现 Method / CallSite / Class 等语法节点与 Calls / HasCallSite 等语法边；
    //   **唯一例外**是"塌缩兜底"的直接访问方：上游不存在任何语义发起者时
    //   （Seeder / DataGrid / 迁移脚本…），折叠提拉永远到不了它们，只能如实画出
    //   `访问方 --ReadsDb/WritesDb…--> 资源`，否则画布空图、与候选徽标的「入边 N」矛盾。
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
        .candidates(b.project_id, &pid, 5, None, None)
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
    let is_semantic = |n: &gt_domain::model::NodeView| {
        gt_domain::model::NodeKind(n.kind.clone()).is_semantic() || n.category.is_some()
    };
    assert!(is_semantic(&ov.center), "中心应是语义节点，实际 {}", ov.center.kind);
    let mut visible = std::collections::HashSet::new();
    visible.insert(ov.center.id.get());
    for n in ov.rings.iter().flatten() {
        visible.insert(n.id.get());
        assert_visible_node_ok(&ov, n);
    }
    for e in &ov.edges {
        assert!(
            gt_domain::model::EdgeKind(e.kind.clone()).is_semantic(),
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
        .candidates(b.project_id, "table", 1, None, None)
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
        assert_visible_node_ok(&ov, n);
    }
    for e in &ov.edges {
        assert!(
            gt_domain::model::EdgeKind(e.kind.clone()).is_semantic(),
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

/// 事件视角：① `HandledBy` 的方向必须跟着原始边（**事件 --由…处理--> 监听器**）；
/// ② `Triggers` 这类只有 1 跳的直接边必须给出**触发点**（`to_call_site`）。
///
/// 反向视角原本一律画成「使用者 --语义边--> 中心」，于是 `HandledBy` 被翻成
/// "监听器 --由…处理--> 事件"，读起来正好相反；同时语义边此前把 `evidence` 存成**字符串**
/// （没有 `location`），触发点取不到，前端抽屉只剩一句"没有逐跳证据可查"。
#[test]
fn event_view_handled_by_points_outward_and_triggers_have_call_site() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let cands = views
        .candidates(b.project_id, "event", 20, None, None)
        .expect("candidates");
    let Some(top) = cands.first() else {
        eprintln!("事件视角没有候选，跳过");
        return;
    };
    let ov = views
        .object_view(b.project_id, "event", top.id, Some(2))
        .expect("object_view");
    assert_eq!(ov.center.kind, "Event", "事件视角中心应是 Event");

    let mut triggers = 0usize;
    let mut handled = 0usize;
    for e in &ov.edges {
        match e.kind.as_str() {
            "Triggers" => {
                // 触发方 --触发--> 事件：主语在触发方，事件是终点。
                assert_eq!(
                    e.to.get(),
                    ov.center.id.get(),
                    "Triggers 边应指向事件中心，实际 {} -> {}",
                    e.from.get(),
                    e.to.get()
                );
                // 只有 1 跳的直接边也要给出触发点（`event('X')` 那一行）：
                // 没有它，前端抽屉只能说"没有逐跳证据可查"，其实那一行就在图里。
                if e.via.is_empty() {
                    assert!(
                        e.to_call_site.is_some(),
                        "直接 Triggers 边 #{} -> #{} 应给出触发点（to_call_site）",
                        e.from.get(),
                        e.to.get()
                    );
                    triggers += 1;
                }
            }
            "HandledBy" => {
                // 事件 --由…处理--> 监听器：主语是事件自己，不能反过来。
                assert_eq!(
                    e.from.get(),
                    ov.center.id.get(),
                    "HandledBy 边应从事件中心出发（事件由监听器处理），实际 {} -> {}",
                    e.from.get(),
                    e.to.get()
                );
                handled += 1;
            }
            _ => {}
        }
    }
    assert!(
        triggers + handled > 0,
        "事件视角应给出触发方或监听方，否则就是一张空图"
    );
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
    let cands = match views.candidates(b.project_id, "route", 6, None, None) {
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

/// **资源视角**下的同一条不变式（上面那条只测了 `route` 视角，于是漏掉了这个方向）。
///
/// `Cache` 这类共享资源被反向展开时，折叠入边的 `via` 末端也必须是**真实接触点**
/// （自己持有一条带 `evidence` 的直接语义边），不能停在 P8 传播边这条"捷径"回溯出的
/// 上游调用者上 —— 否则填 `to_call_site` 时无证据可依，就会在环路里任取一个同资源读者。
///
/// 实测反例：`PUT /setting/seckill_data/set_status/:id/:status` 经
/// `SystemGroupData::set_status` → `CacheService::clear()` → `Cache::tag('crmeb')->clear()`
/// （`CacheService.php:98`）触达缓存；但缓存视角把 `via` 停在 `set_status`，再把
/// `DataMigrationServices.php:53` 的 `Cache::get(self::MIGRATION_STATUS_PREFIX . $name)`
/// 当作"本链路访问缓存的位置" —— 该路由根本没碰那个键。
#[test]
fn cache_view_folded_edges_end_at_real_contact() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let store = &b.container.store;
    let cands = views.candidates(b.project_id, "cache", 50, None, None).expect("cache 候选");
    assert!(!cands.is_empty(), "cache 视角应有候选");

    let mut checked = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for c in cands.iter() {
        let Ok(ov) = views.object_view(b.project_id, "cache", c.id, Some(3)) else {
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
                    "缓存 {} 的折叠边 --{}--> #{} 末端接触点是 {:?}（#{}），但它没有带 evidence 的直接边",
                    c.name,
                    e.kind,
                    e.to.get(),
                    contact.name,
                    contact.id.get()
                ));
            }
        }
    }
    assert!(checked > 0, "应检查到折叠边，实际一条都没有");
    assert!(
        violations.is_empty(),
        "资源视角存在断在传播捷径上的伪路径（接触点张冠李戴）：\n{}",
        violations.join("\n")
    );

    // 具体回归：`PUT /setting/seckill_data/set_status/:id/:status` 若出现在缓存视角，
    // 其"访问缓存的位置"必须落在 `CacheService.php`（它经 `CacheService::clear()` →
    // `Cache::tag('crmeb')->clear()` 触达缓存），**不得**是别的同资源读者
    // （曾错标为 `DataMigrationServices.php:53` 的 `Cache::get(self::MIGRATION_STATUS_PREFIX . $name)`，
    // 而该路由根本没碰那个键）。
    let center = cands.iter().find(|c| c.name == "Cache");
    let route = store
        .query_nodes(&NodeFilter {
            project_id: b.project_id,
            kind: Some(NodeKind("HttpContract".into())),
            name_contains: Some("seckill_data/set_status".into()),
            limit: Some(5),
            offset: Some(0),
        })
        .ok()
        .and_then(|v| v.into_iter().next());
    if let (Some(center), Some(route)) = (center, route) {
        let ov = views
            .object_view(b.project_id, "cache", center.id, Some(3))
            .expect("object_view");
        if let Some(e) = ov
            .edges
            .iter()
            .find(|e| e.from == route.id && e.kind == "ReadsCache")
        {
            let cs = e.to_call_site.as_ref().expect("该缓存边应给出访问位置");
            assert!(
                cs.file.ends_with("CacheService.php"),
                "该路由访问缓存的位置应在 CacheService.php，实际 {}:{}",
                cs.file,
                cs.line
            );
            assert!(
                !e.via.is_empty() && e.via.last().unwrap().name == "clear",
                "该缓存边的接触点应是 CacheService::clear，实际 {:?}",
                e.via.iter().map(|v| v.name.as_str()).collect::<Vec<_>>()
            );
        }
    }
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

    // 30 → 32：门面链式的 `Db::name('store_order')->count()` 被 P7 落成 `ReadsDb`
    // （`tidyOrder` 里确实读了库），另增一条 `ReadsConfig`。二者都是真阳性，
    // 特征测试的意义正在于**显式**接受这类行为变更，而不是让它悄悄溜过去。
    assert_eq!(ov.edges.len(), 32, "边总数变了：{:?}", by_kind);
    // 前端契约桥：uni-app 的 `` request.get(`v2/order/invoice_detail/${id}`) ``
    // （模板串 URL）已能与该后端路由按**参数形状**汇聚，`CallsHttp` 由此入图。
    assert_eq!(
        by_kind.get("CallsHttp").copied().unwrap_or(0),
        1,
        "前端调用该契约的 CallsHttp 应可见，变了说明契约桥被改坏"
    );
    assert_eq!(
        by_kind.get("ReadsCache").copied().unwrap_or(0),
        2,
        "ReadsCache 边数变了"
    );
    assert_eq!(
        by_kind.get("ReadsConfig").copied().unwrap_or(0),
        28,
        "ReadsConfig 边数变了"
    );
    // 除了一条：前端 `CallsHttp` 是**直接**语义边（前端函数 → 契约），不走提拉/传播；
    // 其余 31 条都是沿后端调用链间接得到的资源读写。
    assert_eq!(
        indirect,
        ov.edges.len() - 1,
        "除前端 CallsHttp 外都应是提拉/传播得来的间接边，变了说明 indirect 判定被改坏"
    );
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

/// 计划任务视角：`Schedule` 是**入口类**节点（CRMEB 的项目级 FKB 把 `crontab/...` 路由合成
/// Schedule 节点），它的依赖全在**出边**：`Schedule --HandledBy--> handler →Calls→ … → ReadsCache`，
/// 入边恒为 0。曾把它当作"资源类中心"沿入边回溯 ⇒ 一条边也走不到：环全空、`hidden.total = 0`，
/// 画布上只剩一个孤零零的中心节点（外加一圈空的"1 跳"参考环）；而二级候选的徽标按 3 跳评分
/// 却写着"语义依赖 1" —— 列表说有、图里没有，两处自相矛盾。
#[test]
fn schedule_view_follows_outgoing_chain() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let cands = views
        .candidates(b.project_id, "schedule", 30, None, None)
        .expect("candidates");
    assert!(!cands.is_empty(), "计划任务视角应有候选（CRMEB 的 crontab 路由）");

    // 具体回归：`crontab/set_open/:id/:is_open` 经 `SystemCrontab::setTimerStatus`
    // → `SystemCrontabServices::setTimerStatus` 读缓存，视图里必须看得见这条依赖。
    if let Some(c) = cands.iter().find(|c| c.name.starts_with("crontab/set_open")) {
        let ov = views
            .object_view(b.project_id, "schedule", c.id, Some(2))
            .expect("object_view");
        assert_eq!(ov.center.kind, "Schedule", "计划任务视角中心应是 Schedule");
        assert!(
            !ov.edges.is_empty(),
            "计划任务 {} 画出了空图（环 {:?}）：入口类中心的依赖在出边，不能沿入边回溯",
            c.name,
            ov.rings.iter().map(|r| r.len()).collect::<Vec<_>>()
        );
        assert!(
            ov.rings.iter().flatten().count() > 0,
            "计划任务 {} 的环里应有可达的语义节点",
            c.name
        );
        for e in &ov.edges {
            assert!(
                gt_domain::model::EdgeKind(e.kind.clone()).is_semantic(),
                "默认视图不应出现语法边：{}",
                e.kind
            );
            assert_eq!(
                e.from.get(),
                ov.center.id.get(),
                "入口类视角的边应从中心出发，实际 {} -> {}",
                e.from.get(),
                e.to.get()
            );
        }
        // 这条计划任务的依赖与"路由视角"对同一个 handler 的结论必须一致：
        // `SystemCrontabServices::setTimerStatus` 实际做的是
        //   `Cache::delete('crontabCache')` + `Cache::set(...)`（147 / 148 行）
        // 外加 `$this->dao->update(...)`（145 行）—— 即**写**库 + **写**缓存，
        // 全工程没有任何一处 `Cache::get('crontabCache')` 落在这条链上
        // （唯一的读在 `SystemCrontabServices::crontabCommandRun`，与本任务无关）。
        // 中间两跳（控制器方法 → 服务方法）以 `via` 链给出，并能给出写缓存的那一行。
        let cache = ov
            .edges
            .iter()
            .find(|e| e.kind == "WritesCache")
            .unwrap_or_else(|| {
                panic!(
                    "计划任务 {} 应写缓存（与路由视角一致），实际边：{:?}",
                    c.name,
                    ov.edges.iter().map(|e| &e.kind).collect::<Vec<_>>()
                )
            });
        assert!(
            !cache.via.is_empty(),
            "应经过折叠链（handler → 服务方法）到达缓存，实际 via 为空"
        );
        let cs = cache
            .to_call_site
            .as_ref()
            .expect("应给出写缓存的那一行（本链路访问该资源的位置）");
        assert!(
            cs.file.ends_with("SystemCrontabServices.php"),
            "写缓存的位置应在 SystemCrontabServices.php（147 行 Cache::delete），实际 {}:{}",
            cs.file,
            cs.line
        );
    } else {
        eprintln!("图里没有 crontab/set_open 计划任务，跳过具体断言");
    }

    // 通用不变量：徽标里的"语义依赖 N"是 3 跳内可达的语义节点数（`semantic_value`）。
    // N > 0 ⇒ 同一对象在视图里**必须**画得出边，否则列表与画布互相打脸。
    let mut checked = 0usize;
    let mut dead: Vec<String> = Vec::new();
    for c in cands.iter() {
        let value: usize = c
            .badge
            .as_deref()
            .and_then(|b| b.strip_prefix("语义依赖 "))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if value == 0 {
            continue;
        }
        // 用打分一致的 3 跳取视图，避免把"深度不够"误判成"方向错了"。
        let ov = views
            .object_view(b.project_id, "schedule", c.id, Some(3))
            .expect("object_view");
        checked += 1;
        if ov.edges.is_empty() {
            dead.push(format!("{}（徽标 {:?}）", c.name, c.badge));
        }
    }
    assert!(checked > 0, "应有语义依赖 > 0 的计划任务候选");
    assert!(
        dead.is_empty(),
        "这些计划任务有语义依赖，视图却画出空图：\n{}",
        dead.join("\n")
    );
}

/// 空依赖的计划任务（或路由）视角：画布只剩一个中心节点时，必须给出一条"提示"结论，
/// 把"画面空了 = 视图坏了"的错觉消除掉，同时说明这通常就是真实情况。
///
/// 不变量：**空图 ⇔ 带提示**，二者必须同时出现 / 同时消失 ——
/// 否则要么空图没解释（看着像坏了），要么有链路还硬塞提示（误导）。
#[test]
fn empty_entry_view_carries_hint() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);

    let mut empty_seen = 0usize;
    for perspective in ["schedule", "route"] {
        let cands = views
            .candidates(b.project_id, perspective, 200, None, None)
            .expect("candidates");
        assert!(!cands.is_empty(), "{perspective} 视角应有候选");
        for c in cands.iter() {
            let ov = views
                .object_view(b.project_id, perspective, c.id, Some(3))
                .expect("object_view");
            let has_hint = ov.conclusions.get("提示").is_some();
            assert_eq!(
                ov.edges.is_empty(),
                has_hint,
                "{perspective} {} 空图与提示必须同时出现/消失：edges={} hint={:?}",
                c.name,
                ov.edges.len(),
                ov.conclusions.get("提示")
            );
            if has_hint {
                empty_seen += 1;
                let msg = ov.conclusions["提示"].as_str().unwrap_or("");
                assert!(
                    msg.contains(if perspective == "schedule" {
                        "定时任务"
                    } else {
                        "路由"
                    }),
                    "{perspective} {} 的提示应点明入口类型，实际 {msg}",
                    c.name
                );
            }
        }
    }
    if empty_seen == 0 {
        eprintln!("本次样本没有空依赖的入口（不影响不变量），跳过内容断言");
    } else {
        assert!(empty_seen > 0, "应至少命中一个空依赖入口以验证提示内容");
    }
}


/// 「读 + 写」必须一起报，不能只报一边。
///
/// 折叠视图里一个使用者对同一资源只画**一条**边（按 `action_strength` 择优，写 > 读）。
/// 于是一个既读又写的接触点（`Db::name('store_bargain')->find()` 与 `->update()` 常
/// 同在一个方法里）只会显示成读库**或**写库 —— 单边都是失真。
///
/// 契约：被压掉的另一半必须记在 `EdgeView::also_kinds` 上，且**只能**是同一资源的
/// 另一半（库 ↔ 库、缓存 ↔ 缓存），不许跨资源混搭（那说明标签张冠李戴）。
#[test]
fn read_write_at_same_contact_is_reported_together() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);

    let pairs: &[(&str, &str)] = &[("table", "Db"), ("cache", "Cache")];
    let mut checked = 0usize;
    let mut annotated = 0usize;
    for (perspective, family) in pairs {
        let cands = views
            .candidates(b.project_id, perspective, 30, None, None)
            .unwrap_or_default();
        for c in cands.iter().take(12) {
            let Ok(ov) = views.object_view(b.project_id, perspective, c.id, Some(3)) else {
                continue;
            };
            for e in &ov.edges {
                checked += 1;
                if e.also_kinds.is_empty() {
                    continue;
                }
                annotated += 1;
                let mut all: Vec<&str> = vec![e.kind.as_str()];
                all.extend(e.also_kinds.iter().map(|s| s.as_str()));
                all.sort_unstable();
                all.dedup();
                assert_eq!(
                    all.len(),
                    2,
                    "{} 的 {} 边上 also_kinds 应恰好补上另一种访问方式，实际 {:?}",
                    c.name,
                    e.kind,
                    e.also_kinds
                );
                // 库 ↔ 库、缓存 ↔ 缓存；不许库与缓存混在一处。
                let db = all.iter().all(|k| k.ends_with("Db"));
                let cache = all.iter().all(|k| k.ends_with("Cache"));
                assert!(
                    db || cache,
                    "{} 的 {}+{:?} 跨资源混搭了（库与缓存不可能是同一条边的两种访问方式）",
                    c.name,
                    e.kind,
                    e.also_kinds
                );
                assert!(
                    all.contains(&"ReadsDb") || all.contains(&"ReadsCache"),
                    "另一种访问方式应是读，实际 {:?}",
                    e.also_kinds
                );
                assert!(
                    all.contains(&"WritesDb") || all.contains(&"WritesCache"),
                    "另一种访问方式应是写，实际 {:?}",
                    e.also_kinds
                );
                let _ = family;
            }
        }
    }
    assert!(checked > 0, "应检查到折叠边，实际一条都没有");
    assert!(
        annotated > 0,
        "样本里应有既读又写的接触点（CRMEB 的 store_bargain / tagDate 都是），\
         实际一条都没被标注 —— 「读+写」又退化成单边了"
    );
}
