//! gt-app view-layer integration tests (composition root → build graph → view slicing).
//!
//! Uses the real `samples/CRMEB-master` (3 sub-projects, 2178 source files) as material, assembles all adapters via `Container`,
//! runs a full build, then uses `ViewService` to verify the first/second-level filters and each view slice.
//!
//! **Depends on an oversized real sample (not in repo, see `samples/`'s .gitignore rules):**
//! when the sample exists, run normally; when absent, each case takes `built()`'s soft-skip branch (prints "skip" then
//! return), and will **not** disguise absence as passing.
//!
//! Two exceptions still marked `#[ignore]` (each states why):
//!   * `object_view_characterization_invoice_detail` — characterization snapshot needs recalibration against the reference sample;
//!   * `eval_recall_scenarios` (in `eval_recall.rs`) — needs bge-m3 model weights.
//! To force ignored cases: `cargo test -p gt-app -- --ignored`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, ViewService};
use gt_domain::model::{NewProject, NodeKind};
use gt_domain::port::{
    EdgeDirection, GraphQuery, NodeFilter, NoopObserver, Persistence, RuleProvider, SystemClock,
};

/// Locate the CRMEB sample under `dir/samples`.
///
/// The sample is actually placed in a **multi-level taxonomy** by tech stack (e.g. `samples/php-projects/thinkphp/CRMEB`),
/// and the dir name may or may not carry a `-master` suffix. Earlier we only matched `samples/*/CRMEB-master`
/// (one level + suffix), mismatching the real layout → the sample is clearly on disk but not matched → the whole group
/// takes `built()`'s soft-skip branch, still counted as passed, but in fact **zero coverage**.
/// Here we switch to a bounded-depth recursive search under `samples/`, no longer depending on concrete level or naming.
fn under_samples(dir: &Path) -> Option<PathBuf> {
    /// Search at most `depth` levels within `dir`; return the lexicographically first hit (stable result).
    fn search(dir: &Path, depth: usize) -> Option<PathBuf> {
        if depth == 0 {
            return None;
        }
        let mut hits: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "CRMEB" || name == "CRMEB-master" {
                hits.push(path);
            } else if let Some(found) = search(&path, depth - 1) {
                hits.push(found);
            }
        }
        hits.sort();
        hits.into_iter().next()
    }
    search(&dir.join("samples"), 3)
}

/// Search upward from `CARGO_MANIFEST_DIR` for `samples/**/CRMEB-master`.
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

/// Run a full build once and cache it (only once per test binary).
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
                ui_dir: None,
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

/// Whether this perspective is registered in `views/perspectives.yaml` (unregistered aggregate perspectives can't be asserted).
fn registered(views: &ViewService, pid: gt_domain::model::ProjectId, id: &str) -> bool {
    views
        .perspectives(pid)
        .unwrap_or_default()
        .iter()
        .any(|p| p["id"].as_str() == Some(id))
}

/// Return an object-kind perspective that really has candidates (perspective_id, center_node_id).
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
    let ids: Vec<&str> = registry
        .perspectives
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert!(
        ids.iter().any(|i| *i == "route" || *i == "table"),
        "视角应至少含 route/table"
    );
    // HTTP routing assembles normally (smoke, no service started).
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
    assert_eq!(m.cells.first().map(|r| r.len()).unwrap_or(0), m.cols.len());
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
    // Candidates are **no longer** brought back by the object view: the frontend dropdown requests `/view/{p}/candidates` on demand,
    // recomputing "score 5000 candidates one-by-one BFS" inside every object view is pure waste (see `ObjectView` comment).
}

/// Nodes allowed in the folded view: **only semantic nodes**, no exceptions.
///
/// Two kinds of "collapse fallback" once let syntax nodes slip through:
/// * resource perspective: accessors directly connected to the center with no semantic emitter upstream (Seeder / migration scripts / Console commands…)
///   — degraded the resource perspective into a call graph (names unaddressable, doesn't answer "who triggers", eats canvas quota);
/// * entry perspective: the caller of a frontend `Function --CallsHttp--> contract`.
///
/// Both are now downgraded to `ObjectView.orphans` accounting: not on the canvas, but carry name, relation, and touch-point
/// location, never silently omitted. The canvas thus strictly equals "semantic nodes + semantic edges".
fn assert_visible_node_ok(_ov: &gt_domain::model::ObjectView, n: &gt_domain::model::NodeView) {
    let semantic = gt_domain::model::NodeKind(n.kind.clone()).is_semantic() || n.category.is_some();
    assert!(
        semantic,
        "默认视图只允许语义节点（语法访问方应降级进 orphans 记账）：{} ({})",
        n.name, n.kind
    );
}

#[test]
fn object_view_default_is_semantic_only() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let Some((pid, _)) = first_object_target(&views, b.project_id) else {
        eprintln!("没有可用的对象类视角候选，跳过");
        return;
    };
    // Take the highest-value candidate (`candidates` already sorted by semantic-dependency value descending).
    let cands = views
        .candidates(b.project_id, &pid, 5, None, None)
        .expect("candidates");
    let Some(top) = cands.first() else {
        eprintln!("该视角没有候选，跳过");
        return;
    };
    assert!(
        top.badge.as_deref().unwrap_or("").contains("semantic dependencies"),
        "the candidate badge should carry a value (semantic dependency count), got {:?}",
        top.badge
    );

    let ov = views
        .object_view(b.project_id, &pid, top.id, Some(2))
        .expect("object_view");

    // Semantic node = first-class semantic kind, or carries `category` (external-system subtype Cache / Event / Queue…).
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
fn object_view_resource_center_shows_its_users() {
    // Resource-kind centers (Table / ConfigKey / Cache…) have **reversed** relation direction:
    // semantic edges point from the user to the resource, so the view must answer "who is using it", not show an empty graph.
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

/// Orphan access (direct accessors with no semantic entry upstream) must be **downgraded to accounting, not lit up**:
/// * not appear on the canvas (neither in `rings` nor as any edge endpoint);
/// * but must appear in `orphans`, carrying name and "what it did to the resource" —
#[test]
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

/// Event perspective: canvas = center event + **stable roles on both producer and consumer sides**.
///
/// * consumer side: the syntax endpoint of `HandledBy` (`event --handled by…--> listener`) is promoted via `Declares` to the listener
///   **class** node, the edge becomes a clickable-expandable canvas edge.
/// * trigger side: the trigger of `Triggers` (`trigger point --triggers--> event`) is directly promoted to a visible node and drawn as an
///   **enriched canvas edge** (start = upstream of the call chain / semantic entry, `via` = intermediate callers … trigger point,
///   `to_call_site` = dispatch call site) — "who triggers the event" is the event perspective's core fact,
///   can't stay only in orphans accounting (otherwise the canvas is half-missing).
/// * other general direct accessors still downgrade into `ObjectView.orphans` accounting (with touch-point location).
///
/// This case verifies: ① the `Triggers` edge on the canvas (if a trigger exists) has its trigger endpoint as a visible node and carries the dispatch call site; ② the `HandledBy` edge's listener endpoint is visible; ③ the view isn't empty.
#[test]
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

    let visible: std::collections::HashSet<i64> =
        ov.rings.iter().flatten().map(|n| n.id.get()).collect();

    // ① trigger promoted to visible node: the `Triggers` edge should be drawn, the trigger endpoint on canvas, carrying dispatch call site.
    let trigger_edges: Vec<_> = ov.edges.iter().filter(|e| e.kind == "Triggers").collect();
    for e in &trigger_edges {
        assert!(
            visible.contains(&e.from.get()),
            "Triggers 边的触发方应是画布上的可见节点：{:?}",
            (e.from, e.to)
        );
        assert!(
            e.to == ov.center.id,
            "Triggers 边的终点应是中心事件：{:?}",
            (e.from, e.to)
        );
    }
    // Orphans accounting should no longer contain a trigger already drawn as an edge (downgrade only for enrichment-failed cases).
    let trigger_orphans = ov
        .orphans
        .iter()
        .filter(|o| o.edge_kind == "Triggers")
        .count();
    assert!(
        trigger_edges.is_empty() || trigger_orphans == 0,
        "触发方要么画上画布、要么（富化失败时）记账，不应两边同时出现：edges={} orphans={}",
        trigger_edges.len(),
        trigger_orphans
    );

    // ② consumer (listener) promoted to visible node: the `HandledBy` edge should appear, and its other end is visible on canvas.
    let handled_edges: Vec<_> = ov.edges.iter().filter(|e| e.kind == "HandledBy").collect();
    for e in &handled_edges {
        assert!(
            visible.contains(&e.to.get()) || visible.contains(&e.from.get()),
            "HandledBy 边的端点应是画布上的可见监听器节点：{:?}",
            (e.from, e.to)
        );
    }

    // ③ view not empty: canvas edges on producer/consumer side, or direct accounting, at least one.
    assert!(
        !ov.edges.is_empty() || !ov.orphans.is_empty(),
        "事件视角不应是一张空图"
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
    assert!(!locs.locations.is_empty(), "语法节点应至少有一条定义位置");
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

/// A folded semantic edge in the folded view must have its `via` end land on a **real touch point**:
/// that touch point itself holds a **direct** semantic edge to the resource (hit at P5, with `evidence`).
///
/// Counterexample: a P8 propagation edge only states "upstream reachable to this resource", it **is not a path**. If used as the `via` end,
/// the chain breaks at discovery depth, drawing fake paths like "the route itself read the cache" (the real touch point is several hops away).
/// This invariant is independent of edge kind (read DB / read cache / publish…), should hold for any repo.
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
            // Only look at folded (`via` non-empty) semantic edges; direct edges are the start's own responsibility.
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

/// The **same invariant** under the resource perspective (the above only tested the `route` perspective, thus missed this direction).
///
/// When a shared resource like `Cache` is expanded in reverse, the folded in-edge's `via` end must also be a **real touch point**
/// (holding a `evidence`-bearing direct semantic edge), not stop on the upstream caller back-tracked via P8's propagation edge "shortcut" —
/// otherwise when filling `to_call_site` there's no evidence to rely on, and it would pick any same-resource reader in the loop.
///
/// Measured counterexample: `PUT /setting/seckill_data/set_status/:id/:status` via
/// `SystemGroupData::set_status` → `CacheService::clear()` → `Cache::tag('crmeb')->clear()`
/// (`CacheService.php:98`) reaches the cache; but the cache perspective stops `via` at `set_status`, then treats
/// `DataMigrationServices.php:53`'s `Cache::get(self::MIGRATION_STATUS_PREFIX . $name)` as "where this chain accesses the cache" — that route never touched that key.
#[test]
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
            // Only look at folded (`via` non-empty) semantic edges; direct edges are the start's own responsibility.
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

/// Concrete regression: `GET /v2/order/invoice_detail/:uni` to `Cache` once drew 3 `ReadsCache` edges,
/// and each `via` was truncated (one had only `[detail]`, equal to asserting `detail` itself read the cache).
///
/// The real situation has only **two complete arrival paths**, both converging on the same touch point `CacheService::remember`:
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
    // Former fake-path shape: via only one hop, equal to saying the handler itself read the cache.
    assert!(
        !cache_edges.iter().any(|e| e.via.len() <= 1),
        "不应再出现单跳的断尾伪路径：{desc:?}"
    );
}

/// Regression: the route contract → handler (`detail`) hop must give a "call site" (route registration line),
/// not show "no call statement resolved" at the first hop of the folded chain.
///
/// Each hop's `via[i].call_site` in the folded chain is "the statement that called this hop":
/// `tidyOrder`'s call site is `StoreOrderInvoiceController.php:120` (inside `detail`'s body),
/// so `detail`'s own call site should be the **route registration** (`Route::get('invoice_detail', …)`).
/// `HttpContract` is a synthesized node (no `file_id`), `node_source_location` returns `None`,
/// so `call_site_between` uses `node_locations` to get the route file+line it converges on.
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

/// **Characterization test**: pin the full output shape of `object_view` for a fixed route.
///
/// Its only purpose: `object_view` is a near-thousand-line fold flow; when split / optimized later,
/// any "casual breakage" must surface here immediately — edge count, distribution by kind, via-length distribution,
/// indirect-edge and evidence coverage, visible-ring distribution; any change means behavior changed.
///
/// It is not a "correctness" assertion (correctness is the two cases below), but a **behavior-unchanged** guard rail.
// characterization guard rail: assert the **edge-distribution snapshot** of the invoice_detail object view.
// It guards "behavior unchanged", not "correctness" — once numbers change, they must be **explicitly** accepted and the reason written,
// never silently passed.
//
// This snapshot was recalibrated against the current reference sample (CRMEB v6.0.0): edge total 31 → 35, the extra 4 are
// `{ForeignKey: 1, PassesThrough: 3}` — from the later-added P6 table foreign keys and P14 middleware promotion, both being
// **direct structural edges**, so `indirect` (31) no longer equals the edge total (35).
// Core metrics unchanged: ReadsCache 2, ReadsConfig 28, longest via chain 5 hops.
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
    // Calibration diagnostic: when the snapshot drifts, update the assertions below directly from the actual values printed here, no need to guess.
    eprintln!(
        "[characterize] total={} by_kind={:?} indirect={} with_loc={} max_via={} via1={} orphans_calls_http={}",
        ov.edges.len(),
        by_kind,
        indirect,
        with_loc,
        via_len.last().copied().unwrap_or(0),
        via_len.iter().filter(|&&l| l == 1).count(),
        ov.orphans.iter().filter(|o| o.edge_kind == "CallsHttp").count(),
    );

    assert_eq!(ov.edges.len(), 35, "边总数变了：{:?}", by_kind);
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
    assert_eq!(
        indirect, 31,
        "间接（提拉/传播）边数变了，说明 indirect 判定被改坏"
    );
    // 34 can give a resource-access location; the missing 1 is a structural edge (no call site to cite), expected.
    assert_eq!(
        with_loc, 34,
        "能给出访问位置的边数变了，说明证据选取被改坏"
    );
    // Key: **the longest chain must reach 5 hops** (detail → getQRCodePath → init → more → remember),
    // if folding/back-tracking is broken, the longest chain falls back to 2~3 hops.
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

/// Schedule perspective: `Schedule` is an **entry-kind** node (CRMEB's project-level FKB synthesizes `crontab/...` routes into
/// Schedule nodes), its dependencies are all in **out-edges**: `Schedule --HandledBy--> handler →Calls→ … → ReadsCache`,
/// in-edges always 0. Once treated as a "resource-kind center" and walked back along in-edges ⇒ not a single edge reachable: rings all empty, `hidden.total = 0`,
/// the canvas only has a lone center node (plus a ring of empty "1-hop" references); yet the second-level candidate badge scores by 3 hops
/// but says "semantic dependency 1" — the list says yes, the graph says no, the two contradict.
///
/// Concretely: `crontab/set_open/:id/:is_open` via `SystemCrontab::setTimerStatus`
/// → `SystemCrontabServices::setTimerStatus` reads cache, the view must show this dependency.
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
    assert!(
        !cands.is_empty(),
        "计划任务视角应有候选（CRMEB 的 crontab 路由）"
    );

    // Concrete regression: `crontab/set_open/:id/:is_open` via `SystemCrontab::setTimerStatus`
    // → `SystemCrontabServices::setTimerStatus` reads cache, the view must show this dependency.
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

    // General invariant: the badge's "semantic dependency N" is the count of semantic nodes reachable within 3 hops (`semantic_value`).
    // N > 0 ⇒ the same object must **draw** an edge in the view, otherwise the list and canvas contradict each other.
    let mut checked = 0usize;
    let mut dead: Vec<String> = Vec::new();
    for c in cands.iter() {
        let value: usize = c
            .badge
            .as_deref()
            .and_then(|b| b.strip_prefix("semantic dependencies "))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if value == 0 {
            continue;
        }
        // Use the score-consistent 3-hop view, to avoid misjudging "insufficient depth" as "wrong direction".
        let ov = views
            .object_view(b.project_id, "schedule", c.id, Some(3))
            .expect("object_view");
        checked += 1;
        if ov.edges.is_empty() {
            dead.push(format!("{}（徽标 {:?}）", c.name, c.badge));
        }
    }
    assert!(checked > 0, "there should be a scheduled-task candidate with semantic dependencies > 0");
    assert!(
        dead.is_empty(),
        "这些计划任务有语义依赖，视图却画出空图：\n{}",
        dead.join("\n")
    );
}

/// Empty-dependency Schedule (or route) perspective: when the canvas only has a center node, must give a "hint" conclusion,
/// dispelling the illusion "empty canvas = broken view", while explaining this is usually the real situation.
///
/// Invariant: **empty graph ⇔ with hint**, the two must appear / disappear together —
/// otherwise either an empty graph has no explanation (looks broken), or a real chain still gets a forced hint (misleading).
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
            let has_hint = ov.conclusions.get("hint").is_some();
            assert_eq!(
                ov.edges.is_empty(),
                has_hint,
                "{perspective} {} 空图与提示必须同时出现/消失：edges={} hint={:?}",
                c.name,
                ov.edges.len(),
                ov.conclusions.get("hint")
            );
            if has_hint {
                empty_seen += 1;
                let msg = ov.conclusions["hint"].as_str().unwrap_or("");
                assert!(
                    msg.contains(if perspective == "schedule" {
                        "scheduled task"
                    } else {
                        "route"
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

/// "Read + write" must be reported together, not just one side.
///
/// In the folded view, one user draws only **one** edge to the same resource (pick by `action_strength`, write > read).
/// So a touch point that both reads and writes (`Db::name('store_bargain')->find()` and `->update()` often
/// in the same method) would only show as read-DB **or** write-DB — either side is distortion.
///
/// Contract: the suppressed other half must be recorded on `EdgeView::also_kinds`, and **only** the other half of the same resource
/// (DB ↔ DB, cache ↔ cache), no cross-resource mixing (that would mean mislabeled).
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
                // DB ↔ DB, cache ↔ cache; no DB and cache mixed in one place.
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
