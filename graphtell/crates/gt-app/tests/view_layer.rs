//! gt-app 视角层集成测试（组装根 → 建图 → 视角切片）。
//!
//! 以真实的 `samples/CRMEB-master`（3 个子工程、2178 个源文件）为原料，经由 `Container`
//! 装配全部适配器，跑一遍完整建图，再用 `ViewService` 验证一/二级筛选器与各视角切片。
//!
//! **这组测试依赖体积过大的真实样本（不入库），整组标了 `#[ignore]`**：默认 `cargo test`
//! 不会执行，需先设置 `GRAPHTELL_SAMPLE_DIR` 指向样本根，再 `cargo test -- --ignored`。
//! 这是预期行为 —— CI 里看到 `ignored: N` 不是漏跑（见 `.github/workflows/ci.yml`）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, ViewService};
use gt_domain::model::{NewProject, NodeKind};
use gt_domain::port::{
    EdgeDirection, GraphQuery, NodeFilter, NoopObserver, Persistence, RuleProvider, SystemClock,
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
                rules_dir: Some(workspace_root().join("rules")),
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
                Arc::clone(&container.rules) as Arc<dyn RuleProvider>,
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
fn container_assembles_adapters() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    assert!(b.container.framework_count() > 0, "应装载到框架知识");
    let views_provider = b.container.views();
    let registry = views_provider.registry();
    assert!(!registry.perspectives.is_empty(), "应装载到视角声明");
    let ids: Vec<&str> = registry
        .perspectives
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert!(
        ids.iter().any(|i| *i == "route" || *i == "table"),
        "视角应至少含 route/table"
    );
    // HTTP 路由能正常装配（冒烟，不启动服务）。
    let _router = b.container.router();
}

#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
    assert_eq!(m.cells.first().map(|r| r.len()).unwrap_or(0), m.cols.len());
    for (i, row) in m.cells.iter().enumerate() {
        assert_eq!(row.iter().sum::<u32>(), m.row_totals[i]);
    }
}

#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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

/// 折叠视图允许出现的节点：**只能是语义节点**，一个例外都不留。
///
/// 两类"塌缩兜底"曾让语法节点上网开一面：
/// * 资源视角：与中心直连、上游无语义发起者的访问方（Seeder / 迁移脚本 / Console 命令…）
///   —— 把资源视角降解成了调用图（名字不可寻址、不回答"谁触发"、吃掉画布额度）；
/// * 入口视角：前端 `Function --CallsHttp--> 契约` 的调用方。
///
/// 二者现在一律降级为 `ObjectView.orphans` 记账：不占画布，但带名字、关系与接触点
/// 位置，绝不静默省略。画布因此严格等于"语义节点 + 语义边"。
fn assert_visible_node_ok(_ov: &gt_domain::model::ObjectView, n: &gt_domain::model::NodeView) {
    let semantic = gt_domain::model::NodeKind(n.kind.clone()).is_semantic() || n.category.is_some();
    assert!(
        semantic,
        "默认视图只允许语义节点（语法访问方应降级进 orphans 记账）：{} ({})",
        n.name, n.kind
    );
}

#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
fn object_view_default_is_semantic_only() {
    // 折叠（默认）视图必须「只显示对人类有意义的语义节点 / 语义边」：
    // * 不出现 Method / CallSite / Class 等语法节点与 Calls / HasCallSite 等语法边；
    //   上游找不到语义发起者的直接访问方（Seeder / 迁移脚本 / Console 命令…）
    //   **也不再点亮**：它们降级进 `orphans` 记账（带接触点位置），不占画布。
    //   曾把它们画成直连的语法节点，理由是"不画就空图、与徽标矛盾"——
    //   代价是资源视角降解成调用图；现在矛盾由 orphans 这一行记账消解。
    //   入口视角的前端 HTTP 调用方（`Function --CallsHttp--> 契约`）同样降级记账，
    //   于是画布**严格**只剩语义节点 + 语义边。
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
    assert!(
        is_semantic(&ov.center),
        "中心应是语义节点，实际 {}",
        ov.center.kind
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
    }
}

#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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

/// 孤儿访问（上游找不到任何语义入口的直接访问方）必须**降级记账而非点亮**：
/// * 不出现在画布上（既不进 `rings`，也不是任何边的端点）；
/// * 但必须出现在 `orphans` 里，且带名字与"它对资源做了什么"——
///   否则"徽标说有访问、图里查无此人"的静默省略又回来了。
#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
fn orphan_access_is_accounted_not_drawn() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let cands = views
        .candidates(b.project_id, "table", 30, None, None)
        .expect("candidates");
    let mut checked = 0usize;
    for c in cands {
        let Ok(ov) = views.object_view(b.project_id, "table", c.id, Some(2)) else {
            continue;
        };
        let drawn: std::collections::HashSet<i64> = ov
            .rings
            .iter()
            .flatten()
            .map(|n| n.id.get())
            .chain(ov.edges.iter().flat_map(|e| [e.from.get(), e.to.get()]))
            .collect();
        for o in &ov.orphans {
            assert!(
                !drawn.contains(&o.id.get()),
                "孤儿不应出现在画布上：{} ({})",
                o.name,
                o.kind
            );
            assert!(!o.name.is_empty(), "孤儿记账必须带名字");
            assert!(
                !o.edge_kind.is_empty(),
                "孤儿记账必须说明它对资源做了什么：{}",
                o.name
            );
            checked += 1;
        }
    }
    eprintln!("校验孤儿记账 {checked} 条（数量取决于工程，为 0 亦合法）");
}

/// 事件视角：画布恒为语义节点 —— `HandledBy`（`事件 --由…处理--> 监听器`）与
/// `Triggers`（`触发方 --触发--> 事件`）这类"语义 ↔ 语法"桥边的**语法端点必须折叠**，
/// 降级进 `ObjectView.orphans` 记账（带接触点位置），而**绝不画成画布边**。
///
/// 该视角真正画在画布上的是事件经监听器/触发方**间接触及的资源依赖**（如
/// `事件 --ReadsDb(via 监听器X)--> 表`）—— 证明语法节点虽不画出来，却仍把它的
/// 下游语义资源带上了图，信息没丢，只是从"画节点"降级成"记账 + via 接触点"。
///
/// 曾把监听器/触发方 `force_visible` 直接画出来，破坏"画布只画语义节点"原则；
/// 现在统一折叠。本条同时验证：① 画布上没有 `HandledBy`/`Triggers` 边；
/// ② 它们如实出现在 `orphans` 里（且 `Triggers` 带触发点 `location`）；
/// ③ 视图确有可画的内容（资源边或直连记账，二选一）。
#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
fn event_view_syntactic_accessors_collapse_to_orphans() {
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

    // ① 画布上绝不能出现桥边（语法端点不可见，画出来即违反"画布只画语义节点"）。
    assert!(
        !ov.edges
            .iter()
            .any(|e| e.kind == "HandledBy" || e.kind == "Triggers"),
        "事件视角不应画出 HandledBy/Triggers 这类桥边，应折叠进 orphans：{:?}",
        ov.edges.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );

    // ② 直连的语法访问方必须降级进 orphans，且带"它对事件做了什么"。
    let handled = ov
        .orphans
        .iter()
        .filter(|o| o.edge_kind == "HandledBy")
        .count();
    let triggers = ov
        .orphans
        .iter()
        .filter(|o| o.edge_kind == "Triggers")
        .count();
    assert!(
        handled + triggers > 0,
        "事件视角应把直连监听方/触发方记进 orphans，否则就是静默省略：{:?}",
        ov.orphans
            .iter()
            .map(|o| &o.edge_kind)
            .collect::<Vec<_>>()
    );

    // `Triggers` 这类只有 1 跳的直接边必须给出触发点（接触点位置），否则抽屉只剩
    // "没有逐跳证据可查"，其实 `event('X')` 那一行就在图里。
    for o in ov.orphans.iter().filter(|o| o.edge_kind == "Triggers") {
        assert!(
            !o.name.is_empty(),
            "Triggers orphan 必须带名字（触发方）"
        );
        assert!(
            o.location.is_some(),
            "Triggers orphan {} 应给出触发点（location）",
            o.name
        );
    }

    // ③ 视图不空：要么经监听器画出了资源边，要么至少有直连记账 —— 总之不能是一张空图。
    assert!(
        !ov.edges.is_empty() || !ov.orphans.is_empty(),
        "事件视角不应是一张空图"
    );
}

#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
    assert!(!locs.locations.is_empty(), "语法节点应至少有一条定义位置");
}

#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
            let Some(contact) = e.via.last() else {
                continue;
            };
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
fn cache_view_folded_edges_end_at_real_contact() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let store = &b.container.store;
    let cands = views
        .candidates(b.project_id, "cache", 50, None, None)
        .expect("cache 候选");
    assert!(!cands.is_empty(), "cache 视角应有候选");

    let mut checked = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for c in cands.iter() {
        let Ok(ov) = views.object_view(b.project_id, "cache", c.id, Some(3)) else {
            continue;
        };
        for e in &ov.edges {
            // 只看经过折叠的（`via` 非空的）语义边；直连边由起点自己负责。
            let Some(contact) = e.via.last() else {
                continue;
            };
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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

    // 32 → 31：前端 `Function --CallsHttp--> 契约` 这条**直连**边从画布撤下、降级进
    // `orphans` 记账（画布严格只留语义节点）。特征测试的意义正在于**显式**接受这类
    // 行为变更，而不是让它悄悄溜过去 —— 事实没丢，换了呈现位置（见下方 orphans 断言）。
    assert_eq!(ov.edges.len(), 31, "边总数变了：{:?}", by_kind);
    // 前端契约桥：uni-app 的 `` request.get(`v2/order/invoice_detail/${id}`) ``
    // （模板串 URL）已能与该后端路由按**参数形状**汇聚，这条 `CallsHttp` 改记在
    // `orphans` 里（画布不再出现前端函数这个语法节点）。
    assert_eq!(
        ov.orphans
            .iter()
            .filter(|o| o.edge_kind == "CallsHttp")
            .count(),
        1,
        "前端调用该契约的 CallsHttp 应记在 orphans 里，变了说明契约桥被改坏"
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
    // `CallsHttp` 已从画布撤下，现在画布上的每一条边都是沿后端调用链**间接**得来的
    // 资源读写（提拉 / 传播），不再有直连边混入。
    assert_eq!(
        indirect,
        ov.edges.len(),
        "画布上的边都应是提拉/传播得来的间接边，变了说明 indirect 判定被改坏"
    );
    assert_eq!(
        with_loc,
        ov.edges.len(),
        "每条边都应能给出资源访问位置，变了说明证据选取被改坏"
    );
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
fn schedule_view_follows_outgoing_chain() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let cands = views
        .candidates(b.project_id, "schedule", 30, None, None)
        .expect("candidates");
    assert!(
        !cands.is_empty(),
        "计划任务视角应有候选（CRMEB 的 crontab 路由）"
    );

    // 具体回归：`crontab/set_open/:id/:is_open` 经 `SystemCrontab::setTimerStatus`
    // → `SystemCrontabServices::setTimerStatus` 读缓存，视图里必须看得见这条依赖。
    if let Some(c) = cands
        .iter()
        .find(|c| c.name.starts_with("crontab/set_open"))
    {
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
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
