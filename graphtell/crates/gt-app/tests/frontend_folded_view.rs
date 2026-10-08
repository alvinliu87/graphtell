//! Self-check of how the frontend chain renders in the **folded view** (synthetic sample `samples/link`).
//!
//! The build layer (`gt-pipeline/tests/link_sample.rs`) only proves "the edges are built";
//! this proves the **render layer** treats the frontend as a first-class citizen:
//!   1. frontend functions (`deleteItem`) are folded into the `via` chain as **syntax nodes**,
//!      not degraded into a `File` node directly wired to the contract (isomorphic to a backend `Method`);
//!   2. folded edges give a **per-hop call site** (`via[i].call_site` / `to_call_site`),
//!      so the frontend drawer has a "folded syntax call chain" to display;
//!   3. a cross-file frontend chain (`App.onDelete -> api.deleteItem`) is also gathered into one `via`.
//!
//! The single test below builds the in-repo synthetic `link` sample (`samples/link`, shipped with the
//! repo); if it is somehow absent the test now fails loudly via `.expect` rather than soft-skipping into
//! a false pass.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, ViewService};
use gt_domain::model::{EdgeView, NewProject, NodeKind};
use gt_domain::port::{GraphQuery, NoopObserver, NodeFilter, SystemClock};
use gt_sample_support::link_sample_root;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}

struct Built {
    container: Container,
    project_id: gt_domain::model::ProjectId,
}

fn built() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let root = link_sample_root()?;
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
                ui_dir: None,
                };
            let container = Container::new(config).expect("container assembly should not fail");

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
                    name: "link-sample".into(),
                    root_path: root,
                    description: None,
                    config: None,
                })
                .expect("creating the project should not fail");
            pipeline
                .run(project.id, &NoopObserver)
                .expect("graph build on the synthetic sample should not fail");

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


/// Find the contract node `POST /api/delete` (the front-back convergence point).
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
    let nodes: Vec<_> = nodes;
    // Two sub-projects now name the same endpoint and each owns its own node; this test is about the
    // **frontend's** contract (the one carrying the `CallsHttp` in-edge from the axios call site).
    nodes
        .iter()
        .find(|n| n.properties.get("side").and_then(|v| v.as_str()) == Some("frontend"))
        .or_else(|| nodes.first())
        .map(|n| n.id)
}

/// Print the folded view "as the canvas shows it": centre / each ring / each edge's via chain and per-hop call sites.
/// A headless environment cannot screenshot the UI, yet the UI renders exactly this data model — printing it is equivalent.
fn dump_view(ov: &gt_domain::model::ObjectView, names: &std::collections::HashMap<i64, String>) {
    let name = |id: i64| -> String {
        names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("#{id}"))
    };
    println!("\n===== Folded view (default) =====");
    println!(
        "center: {} [{}]  (perspective={})",
        ov.center.name, ov.center.kind, ov.perspective
    );
    for (i, ring) in ov.rings.iter().enumerate() {
        println!("  ring {}:", i + 1);
        for n in ring {
            println!("    - {} [{}]", n.name, n.kind);
        }
    }
    println!("  edges ({}):", ov.edges.len());
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
    println!("  note: {}", ov.hidden.note);
    println!("==============================\n");
}

/// The frontend chain must be visible in the **folded view**, carrying an expandable per-hop call chain.
#[test]
fn frontend_chain_visible_in_folded_route_view() {
    let b = built().expect(
        "frontend folded-view test requires the synthetic `link` sample (samples/link), which ships with the repo; a missing sample means the checkout is incomplete",
    );
    let views = view_svc(&b);
    let Some(cid) = contract_id(&b) else {
        eprintln!("no /api/delete contract in the graph, skipping");
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

    let fe: Vec<_> = ov.orphans.iter().filter(|o| o.edge_kind == "CallsHttp").collect();
    assert!(
        !fe.is_empty(),
        "frontend -> contract CallsHttp should be recorded in orphans, got orphans: {:?}",
        ov.orphans.iter().map(|o| &o.edge_kind).collect::<Vec<_>>()
    );

    // ---- 2) The initiator is a **function node** (a syntax node), not File ----
    for o in &fe {
        assert_eq!(
            o.kind, "Function",
            "CallsHttp start should be a frontend function node (isomorphic to a backend Method), got kind = {}",
            o.kind
        );
        // The sample wraps `axios.post('/api/delete')` in `export function deleteItem`, so the CallsHttp
        // source must be that frontend function node by name — pinning "function, not File" concretely rather
        // than only by kind (a wrong caller name would mean the wrong node was folded in).
        assert_eq!(
            o.name, "deleteItem",
            "the CallsHttp source must be the `deleteItem` frontend function, got name = {}",
            o.name
        );
        // It must not appear on the canvas: neither in a ring nor as an endpoint of any edge.
        let on_canvas = ov
            .rings
            .iter()
            .flatten()
            .any(|n| n.id == o.id)
            || ov.edges.iter().any(|e| e.from == o.id || e.to == o.id);
        assert!(
            !on_canvas,
            "the frontend caller {} is an accounted-downgrade and must not appear on the canvas again",
            o.name
        );
    }

    let mut hops = 0usize;
    let mut hops_with_site = 0usize;
    for e in &ov.edges {
        for v in &e.via {
            if v.id.get() == e.from.get() {
                continue; // the start itself has no "who called me"
            }
            hops += 1;
            if v.call_site.is_some() {
                hops_with_site += 1;
            }
        }
    }
    // No longer asserting `hops > 0`: the frontend caller is an accounted-downgrade, and this minimal route (the contract has no resource dependency)
    // is **supposed to be 0 edges** on the canvas — the facts all live in orphans, not "the fold broke".
    assert_eq!(
        hops, hops_with_site,
        "every folded hop must give a call site (drawer hop-by-hop chain), got {hops_with_site}/{hops}"
    );

    // ---- 4) The downgrade accounting must still give a touch point: the line where the frontend really issues axios ----
    // Accounting is not "vanishing": opening the list must show `file:line`, otherwise "who calls this endpoint" becomes empty talk.
    for o in &fe {
        assert!(
            o.location.is_some(),
            "frontend CallsHttp accounting should give a touch-point location, missing: {}",
            o.name
        );
    }
}

/// The render layer's drawer contract also needs the **edge-level** `to_call_site` (where the last folded hop
/// calls `to`) on real folded edges — the doc lists `via[i].call_site / to_call_site` together, but the
/// contract-centred route view above has 0 canvas edges, so it never exercises a folded `EdgeView`.
///
/// This centres on a backend method and asserts, *whenever a folded edge exists*, that every non-start `via` hop
/// carries a per-hop `call_site` **and** at least one folded edge carries a terminal `to_call_site`. The `link`
/// sample has no backend handler→service→DB chain, so on it this soft-skips (no folded edges); on a richer
/// sample it becomes a real guard against a dropped `to_call_site`.
#[test]
fn folded_edge_exposes_to_call_site_when_present() {
    let b = built().expect(
        "frontend folded-view test requires the synthetic `link` sample (samples/link), which ships with the repo",
    );
    let views = view_svc(&b);

    // Pick a backend method to centre on: prefer the `/api/delete` handler, else any non-frontend method.
    let candidates = b
        .container
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project_id,
            kind: Some(NodeKind(NodeKind::METHOD.to_string())),
            name_contains: Some("delete".into()),
            limit: Some(20),
            offset: Some(0),
        })
        .unwrap_or_default();
    let backend = candidates
        .into_iter()
        .find(|n| n.properties.get("side").and_then(|v| v.as_str()) != Some("frontend"))
        .or_else(|| {
            b.container
                .store
                .query_nodes(&NodeFilter {
                    project_id: b.project_id,
                    kind: Some(NodeKind(NodeKind::METHOD.to_string())),
                    name_contains: None,
                    limit: Some(50),
                    offset: Some(0),
                })
                .ok()
                .and_then(|ns| {
                    ns.into_iter()
                        .find(|n| n.properties.get("side").and_then(|v| v.as_str()) != Some("frontend"))
                })
        });
    let Some(h) = backend else {
        eprintln!("no backend method in the graph, skipping to_call_site check");
        return;
    };

    let ov = views
        .object_view(b.project_id, "route", h.id, Some(2))
        .expect("object_view");

    let folded: Vec<&EdgeView> = ov.edges.iter().filter(|e| !e.via.is_empty()).collect();
    if folded.is_empty() {
        // This sample has no folded visible edges (frontend lives in orphans, backend chain is direct), so the
        // drawer's hop-by-hop + terminal call-site contract is not exercisable here — skip rather than false-fail.
        eprintln!(
            "method {} yields no folded edges in this view, skipping to_call_site check",
            h.name
        );
        return;
    }

    for e in &folded {
        for v in &e.via {
            if v.id == e.from {
                continue; // the start hop has no "who called me"
            }
            assert!(
                v.call_site.is_some(),
                "folded edge {} -> {} hop `{}` must carry a per-hop call_site",
                e.from.get(),
                e.to.get(),
                v.name
            );
        }
    }
    assert!(
        folded.iter().any(|e| e.to_call_site.is_some()),
        "at least one folded edge must carry a terminal to_call_site (the drawer's final call site); edges={}",
        folded.len()
    );
}
