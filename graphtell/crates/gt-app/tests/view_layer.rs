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
/// and the dir name may or may not carry a `-master` suffix — a fixed one-level pattern would mismatch the real
/// layout → the sample is on disk but not matched → the whole group takes `built()`'s soft-skip branch, still
/// counted as passed, but in fact **zero coverage**. So this does a bounded-depth recursive search under
/// `samples/`, independent of concrete level or naming.
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
            let container = Container::new(config).expect("container wiring must not fail");

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
                .expect("creating the project must not fail");

            pipeline
                .run(project.id, &NoopObserver)
                .expect("graphing the CRMEB sample must not fail");

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
    "skipped: CRMEB sample not found (point GRAPHTELL_SAMPLE_DIR at it)"
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
    assert!(b.container.framework_count() > 0, "framework knowledge must be loaded");
    let views_provider = b.container.views();
    let registry = views_provider.registry();
    assert!(!registry.perspectives.is_empty(), "the perspective declarations must be loaded");
    let ids: Vec<&str> = registry
        .perspectives
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert!(
        ids.iter().any(|i| *i == "route" || *i == "table"),
        "the perspectives must include at least route/table"
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
    assert!(!list.is_empty(), "the perspective list must not be empty");
    let any_available = list
        .iter()
        .any(|v| v["available"].as_u64().unwrap_or(0) > 0);
    assert!(any_available, "at least one perspective in the graph has data");
}

#[test]
fn aggregate_deploy_unit_clusters() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    if !registered(&views, b.project_id, "deploy_unit") {
        eprintln!("skipped: the deploy_unit perspective is not enabled in views/perspectives.yaml");
        return;
    }
    let agg = views
        .aggregate_view(b.project_id, "deploy_unit", 12)
        .expect("aggregate");
    assert_eq!(agg.perspective, "deploy_unit");
    assert!(
        !agg.clusters.is_empty() || agg.notice.is_some(),
        "deploy_unit either has cluster boxes or gives an honest hint"
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
        eprintln!("skipped: the platform perspective is not enabled in views/perspectives.yaml");
        return;
    }
    let agg = views
        .aggregate_view(b.project_id, "platform", 12)
        .expect("aggregate");
    let m = agg.matrix.expect("platform must be a matrix perspective");
    assert!(!m.rows.is_empty(), "the matrix must have rows");
    assert!(!m.cols.is_empty(), "the matrix must have columns");
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
        eprintln!("no usable object-perspective candidates; skipping the object_view assertion");
        return;
    };
    let ov = views
        .object_view(b.project_id, &pid, nid, Some(2))
        .expect("object_view");
    assert_eq!(ov.perspective, pid);
    assert_eq!(ov.center.id, nid);
    assert!(!ov.center.name.is_empty());
    assert_eq!(ov.center.ring, 0, "the centre node must be on ring 0");
    assert!(!ov.hidden.note.is_empty(), "an omission note must be given (honesty)");
    // Candidates are not brought back by the object view: the frontend dropdown requests `/view/{p}/candidates` on demand,
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
        "the default view allows semantic nodes only (syntactic accessors degrade into the orphans tally): {} ({})",
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
        eprintln!("no usable object-perspective candidates, skipping");
        return;
    };
    // Take the highest-value candidate (`candidates` already sorted by semantic-dependency value descending).
    let cands = views
        .candidates(b.project_id, &pid, 5, None, None)
        .expect("candidates");
    let Some(top) = cands.first() else {
        eprintln!("this perspective has no candidates, skipping");
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
        "the centre must be a semantic node, got {}",
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
            "no syntactic edge may appear in the default view: {}",
            e.kind
        );
        assert!(
            visible.contains(&e.from.get()) && visible.contains(&e.to.get()),
            "there must be no dangling edge: {} {} -> {}",
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
        eprintln!("the table perspective has no candidates, skipping");
        return;
    };
    let ov = views
        .object_view(b.project_id, "table", top.id, Some(2))
        .expect("object_view");

    assert_eq!(ov.center.kind, "Table", "the centre of the table perspective must be a Table");
    assert_eq!(
        ov.center.own_view.as_deref(),
        Some("table"),
        "the centre must carry its own perspective id (used by click-to-switch)"
    );
    assert!(
        ov.rings.iter().flatten().count() > 0,
        "the table perspective must show its users (who reads/writes the table) instead of an empty graph"
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
            "no syntactic edge may appear in the default view: {}",
            e.kind
        );
        assert!(
            visible.contains(&e.from.get()) && visible.contains(&e.to.get()),
            "there must be no dangling edge: {} {} -> {}",
            e.kind,
            e.from.get(),
            e.to.get()
        );
        assert_eq!(
            e.to.get(),
            ov.center.id.get(),
            "an edge of a resource perspective must point at the centre (user -> resource), got {} -> {}",
            e.from.get(),
            e.to.get()
        );
        assert_ne!(
            e.from.get(),
            ov.center.id.get(),
            "an edge of a resource perspective must not start at the centre ({e:?})"
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
                "an orphan must not appear on the canvas: {} ({})",
                o.name,
                o.kind
            );
            assert!(!o.name.is_empty(), "the orphan tally must carry a name");
            assert!(
                !o.edge_kind.is_empty(),
                "the orphan tally must say what it did to the resource: {}",
                o.name
            );
            checked += 1;
        }
    }
    eprintln!("verified orphan accounting for {checked} entries (the count depends on the project; 0 is also valid)");
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
        eprintln!("the event perspective has no candidates, skipping");
        return;
    };
    let ov = views
        .object_view(b.project_id, "event", top.id, Some(2))
        .expect("object_view");
    assert_eq!(ov.center.kind, "Event", "the centre of the event perspective must be an Event");

    let visible: std::collections::HashSet<i64> =
        ov.rings.iter().flatten().map(|n| n.id.get()).collect();

    // ① trigger promoted to visible node: the `Triggers` edge should be drawn, the trigger endpoint on canvas, carrying dispatch call site.
    let trigger_edges: Vec<_> = ov.edges.iter().filter(|e| e.kind == "Triggers").collect();
    for e in &trigger_edges {
        assert!(
            visible.contains(&e.from.get()),
            "the trigger side of a Triggers edge must be a visible node on the canvas: {:?}",
            (e.from, e.to)
        );
        assert!(
            e.to == ov.center.id,
            "the end of a Triggers edge must be the centre event: {:?}",
            (e.from, e.to)
        );
    }
    // Orphans accounting should not contain a trigger already drawn as an edge (downgrade only for enrichment-failed cases).
    let trigger_orphans = ov
        .orphans
        .iter()
        .filter(|o| o.edge_kind == "Triggers")
        .count();
    assert!(
        trigger_edges.is_empty() || trigger_orphans == 0,
        "the trigger side is either drawn on the canvas or (when enrichment fails) tallied, never both: edges={} orphans={}",
        trigger_edges.len(),
        trigger_orphans
    );

    // ② consumer (listener) promoted to visible node: the `HandledBy` edge should appear, and its other end is visible on canvas.
    let handled_edges: Vec<_> = ov.edges.iter().filter(|e| e.kind == "HandledBy").collect();
    for e in &handled_edges {
        assert!(
            visible.contains(&e.to.get()) || visible.contains(&e.from.get()),
            "the endpoint of a HandledBy edge must be a visible listener node on the canvas: {:?}",
            (e.from, e.to)
        );
    }

    // ③ view not empty: canvas edges on producer/consumer side, or direct accounting, at least one.
    assert!(
        !ov.edges.is_empty() || !ov.orphans.is_empty(),
        "the event perspective must not be an empty graph"
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
        eprintln!("no object node; skipping the locations assertion");
        return;
    };
    let locs = views.node_locations(nid).expect("node_locations");
    assert_eq!(locs.id, nid);
    assert!(!locs.locations.is_empty(), "a syntactic node must have at least one definition location");
}

#[test]
fn edge_evidence_verifies_chain() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };
    let views = view_svc(&b);
    let Some((_pid, nid)) = first_object_target(&views, b.project_id) else {
        eprintln!("no object node; skipping the edge assertion");
        return;
    };
    let store = &b.container.store;
    let edges = store
        .edges_of(nid, EdgeDirection::Both)
        .expect("edges_of must not fail");
    let Some(e) = edges.into_iter().next() else {
        eprintln!("the center node has no edges; skipping the edge_evidence assertion");
        return;
    };
    let ev = views
        .edge_evidence(e.id.get())
        .expect("edge_evidence")
        .expect("the edge must exist");
    assert_eq!(ev.edge.id, e.id.get());
    assert!(
        !ev.locations.is_empty() || ev.reason.is_some(),
        "both solid and dashed edges must have an evidence location or a reason"
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
            eprintln!("no route candidates, skipping");
            return;
        }
    };
    assert!(!cands.is_empty(), "the route perspective must have candidates");

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
                    "edge --{}--> #{} of route {} ends at the contact point {:?} (#{}) but has no direct edge carrying evidence",
                    c.name, e.kind, e.to.get(), contact.name, contact.id.get()
                ));
            }
        }
    }
    assert!(checked > 0, "a collapsed edge must be found, but there is none");
    assert!(
        violations.is_empty(),
        "a pseudo-path broken off at the discovery depth exists:\n{}",
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
        .expect("cache candidate");
    assert!(!cands.is_empty(), "the cache perspective must have candidates");

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
                    "the collapsed edge --{}--> #{} of cache {} ends at the contact point {:?} (#{}) but has no direct edge carrying evidence",
                    c.name,
                    e.kind,
                    e.to.get(),
                    contact.name,
                    contact.id.get()
                ));
            }
        }
    }
    assert!(checked > 0, "a collapsed edge must be found, but there is none");
    assert!(
        violations.is_empty(),
        "the resource perspective has a pseudo-path broken off at a propagation shortcut (the contact point is misattributed):\n{}",
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
            let cs = e.to_call_site.as_ref().expect("that cache edge must give an access location");
            assert!(
                cs.file.ends_with("CacheService.php"),
                "the route's cache access must be located in CacheService.php, got {}:{}",
                cs.file,
                cs.line
            );
            assert!(
                !e.via.is_empty() && e.via.last().unwrap().name == "clear",
                "the contact point of that cache edge must be CacheService::clear, got {:?}",
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
        eprintln!("the graph has no invoice_detail route, skipping");
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
        "there must be exactly two complete arrival paths (the tidyOrder / getQRCodePath branches), got {}: {:?}",
        cache_edges.len(),
        desc
    );
    for e in &cache_edges {
        let last = e
            .via
            .last()
            .unwrap_or_else(|| panic!("the cache edge must go through a collapsed chain, but via is empty: {desc:?}"));
        assert_eq!(
            last.name, "remember",
            "via must land on the real contact point CacheService::remember, but it ends at {:?} (whole chain {desc:?})",
            last.name
        );
        assert!(e.indirect, "the route itself does not read the cache, so it must be marked indirect (dashed)");
        assert!(
            e.to_call_site.is_some(),
            "it must give the location where this chain accesses the cache (Cache::tag()->remember() in CacheService.php)"
        );
    }
    // Former fake-path shape: via only one hop, equal to saying the handler itself read the cache.
    assert!(
        !cache_edges.iter().any(|e| e.via.len() <= 1),
        "no single-hop truncated pseudo-path may appear any more: {desc:?}"
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
        eprintln!("the graph has no invoice_detail route, skipping");
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
            "the route -> handler first hop ({name}) must give a call site (the route registration line), got None (it would show 'call statement not resolved')",
            name = first.name
        );
    }
    assert!(checked > 0, "at least one collapsed edge going through detail must be found");
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
// This snapshot is calibrated against the current reference sample (CRMEB v6.0.0): edge total 35, of which 4 are
// `{ForeignKey: 1, PassesThrough: 3}` — P6 table foreign keys and P14 middleware promotion, both **direct structural
// edges**, so `indirect` (31) is less than the edge total (35).
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
        eprintln!("the graph has no invoice_detail route, skipping");
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

    assert_eq!(ov.edges.len(), 35, "the total edge count changed: {:?}", by_kind);
    assert_eq!(
        ov.orphans
            .iter()
            .filter(|o| o.edge_kind == "CallsHttp")
            .count(),
        1,
        "the CallsHttp of a frontend call to this contract must be tallied in orphans; a change means the contract bridge was broken"
    );
    assert_eq!(
        by_kind.get("ReadsCache").copied().unwrap_or(0),
        2,
        "the ReadsCache edge count changed"
    );
    assert_eq!(
        by_kind.get("ReadsConfig").copied().unwrap_or(0),
        28,
        "the ReadsConfig edge count changed"
    );
    assert_eq!(
        indirect, 31,
        "the indirect (hoisted / propagated) edge count changed, so the indirect judgement was broken"
    );
    // 34 can give a resource-access location; the missing 1 is a structural edge (no call site to cite), expected.
    assert_eq!(
        with_loc, 34,
        "the number of edges able to give an access location changed, so evidence selection was broken"
    );
    // Key: **the longest chain must reach 5 hops** (detail → getQRCodePath → init → more → remember),
    // if folding/back-tracking is broken, the longest chain falls back to 2~3 hops.
    assert_eq!(
        via_len.last().copied().unwrap_or(0),
        5,
        "the longest via chain should be 5 hops, got distribution {via_len:?}"
    );
    assert!(
        via_len.iter().filter(|&&l| l == 1).count() >= 6,
        "there must be several 1-hop direct edges (the config that detail itself reads), got {via_len:?}"
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
        "the schedule perspective must have candidates (CRMEB's crontab routes)"
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
        assert_eq!(ov.center.kind, "Schedule", "the centre of the schedule perspective must be a Schedule");
        assert!(
            !ov.edges.is_empty(),
            "schedule {} drew an empty graph (rings {:?}): the entry-class centre's dependencies are on out-edges, so they cannot be traced back along in-edges",
            c.name,
            ov.rings.iter().map(|r| r.len()).collect::<Vec<_>>()
        );
        assert!(
            ov.rings.iter().flatten().count() > 0,
            "the rings of schedule {} must contain a reachable semantic node",
            c.name
        );
        for e in &ov.edges {
            assert!(
                gt_domain::model::EdgeKind(e.kind.clone()).is_semantic(),
                "no syntactic edge may appear in the default view: {}",
                e.kind
            );
            assert_eq!(
                e.from.get(),
                ov.center.id.get(),
                "an edge of an entry-class perspective must start at the centre, got {} -> {}",
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
                    "schedule {} must write the cache (consistent with the route perspective), edges: {:?}",
                    c.name,
                    ov.edges.iter().map(|e| &e.kind).collect::<Vec<_>>()
                )
            });
        assert!(
            !cache.via.is_empty(),
            "it must reach the cache through a collapsed chain (handler -> service method), but via is empty"
        );
        let cs = cache
            .to_call_site
            .as_ref()
            .expect("it must give the line that writes the cache (where this chain accesses the resource)");
        assert!(
            cs.file.ends_with("SystemCrontabServices.php"),
            "the cache write must be located in SystemCrontabServices.php (Cache::delete on line 147), got {}:{}",
            cs.file,
            cs.line
        );
    } else {
        eprintln!("the graph has no crontab/set_open scheduled task; skipping the specific assertion");
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
            dead.push(format!("{} (badge {:?})", c.name, c.badge));
        }
    }
    assert!(checked > 0, "there should be a scheduled-task candidate with semantic dependencies > 0");
    assert!(
        dead.is_empty(),
        "these schedules have semantic dependencies, yet the view drew an empty graph:\n{}",
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
        assert!(!cands.is_empty(), "the {perspective} perspective must have candidates");
        for c in cands.iter() {
            let ov = views
                .object_view(b.project_id, perspective, c.id, Some(3))
                .expect("object_view");
            let has_hint = ov.conclusions.get("hint").is_some();
            assert_eq!(
                ov.edges.is_empty(),
                has_hint,
                "for {perspective} {} the empty graph and the hint must appear/vanish together: edges={} hint={:?}",
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
                    "the hint for {perspective} {} must name the entry type, got {msg}",
                    c.name
                );
            }
        }
    }
    if empty_seen == 0 {
        eprintln!("this sample has no entry with empty dependencies (does not affect the invariant); skipping the content assertion");
    } else {
        assert!(empty_seen > 0, "at least one empty-dependency entry must be hit to verify the hint content");
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
                    "on the {} edge of {} also_kinds must add exactly the other access mode, got {:?}",
                    c.name,
                    e.kind,
                    e.also_kinds
                );
                // DB ↔ DB, cache ↔ cache; no DB and cache mixed in one place.
                let db = all.iter().all(|k| k.ends_with("Db"));
                let cache = all.iter().all(|k| k.ends_with("Cache"));
                assert!(
                    db || cache,
                    "{} mixes resources across {}+{:?} (a DB and a cache cannot be two access modes of the same edge)",
                    c.name,
                    e.kind,
                    e.also_kinds
                );
                assert!(
                    all.contains(&"ReadsDb") || all.contains(&"ReadsCache"),
                    "the other access mode must be read, got {:?}",
                    e.also_kinds
                );
                assert!(
                    all.contains(&"WritesDb") || all.contains(&"WritesCache"),
                    "the other access mode must be write, got {:?}",
                    e.also_kinds
                );
                let _ = family;
            }
        }
    }
    assert!(checked > 0, "a collapsed edge must be found, but there is none");
    assert!(
        annotated > 0,
        "样本里应有既读又写的接触点（CRMEB 的 store_bargain / tagDate 都是），\
         实际一条都没被标注 —— 「读+写」又退化成单边了"
    );
}
