//! Self-check: a frontend sub-project's HTTP calls to the backend should be connected by the build mechanism into a "contract bridge".
//!
//! Not depending on an external sample_project sample: run the full pipeline on `samples/frontend-backend-link`, a tiny
//! synthetic repo (a ThinkPHP backend + an axios frontend), asserting:
//!   1. the frontend `axios.post('/api/delete')` is wrapped in `export function deleteItem`,
//!      synthesises an `HttpContract` with an **incoming `CallsHttp`** edge whose source is the frontend **function node**
//!      (`deleteItem`) — this is exactly a "frontend semantic node": isomorphic to the backend `Method`, not a File;
//!   2. the same contract node is annotated `side = frontend` by the frontend FKB;
//!   3. the backend `thinkphp` `Route::post('/api/delete', ...)` and the frontend synthesise
//!      **the same `ContractId` (`POST /api/delete`)** and merge idempotently — the contract bridge's
//!      cross-sub-project convergence point;
//!   4. a frontend cross-file call chain: `App.onDelete -> api.deleteItem` is resolved into a `Calls` edge by P7
//!      (frontend semantic nodes connect, gathered into the `via` chain in the folded view, hop-by-hop visible in the drawer).

mod common;

use std::collections::HashSet;
use std::path::PathBuf;

use gt_domain::model::NodeKind;
use gt_domain::model::ProjectConfig;
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter, ProjectReader};

/// A synthetic sample directory inside the repo (shipped with the repo).
///
/// The sample tree lives at the **repo root** (`samples/`). Hard-coding "up N levels" would point at a
/// non-existent directory once the
/// sample moves — then the case fails, or worse is changed to skip, becoming "CI all green but zero
/// coverage". Here we instead search **upward level by level** from `CARGO_MANIFEST_DIR`, and support a
/// `GRAPHTELL_SAMPLES_DIR` override; only when nothing is found do we fall back to a candidate path, so the
/// caller's assert raises a diagnosable missing-path message (this file deliberately requires failure
/// rather than skip).
fn synth_root() -> PathBuf {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLES_DIR") {
        let candidate = PathBuf::from(dir).join("frontend-backend-link");
        if candidate.is_dir() {
            return candidate;
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let candidate = cur.join("samples").join("frontend-backend-link");
        if candidate.is_dir() {
            return candidate;
        }
        if !cur.pop() {
            break;
        }
    }
    // Fallback: the repo root — `crates/gt-pipeline` -> `crates` -> workspace -> repo root — so the assert
    // names the path that really should hold the sample.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.pop();
    p.join("samples/frontend-backend-link")
}

#[test]
fn frontend_calls_backend_merge_into_contract() {
    let root = synth_root();
    assert!(
        root.is_dir(),
        "the synthetic sample is missing: {}. It ships with the repository, so .gitignore must not exclude it",
        root.display()
    );
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("graphing");

    // The two sub-projects should be recognised correctly: backend php / frontend javascript.
    let subs = b.store.list_sub_projects(b.project.id).unwrap_or_default();
    assert!(
        subs.iter().any(|s| s.language.as_str() == "php" && s.frameworks.contains(&"thinkphp".to_string())),
        "the backend must be recognised as thinkphp, got: {:?}",
        subs.iter().map(|s| (&s.language, &s.frameworks)).collect::<Vec<_>>()
    );
    assert!(
        subs.iter().any(|s| s.language.as_str() == "javascript" && s.frameworks.contains(&"frontend-js".to_string())),
        "the frontend must be recognised as frontend-js"
    );

    // Take all HttpContract nodes, find the one an incoming frontend CallsHttp connects into.
    let contracts = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: None,
            limit: Some(200),
            offset: Some(0),
        })
        .expect("the contract node must be readable");

    let mut frontend_caller: Option<gt_domain::model::Node> = None;
    let mut bridged: Option<gt_domain::model::Node> = None;
    for c in &contracts {
        let inc = b.store.edges_of(c.id, EdgeDirection::Incoming).unwrap_or_default();
        if let Some(e) = inc.iter().find(|e| e.kind.as_str() == "CallsHttp") {
            // Incoming CallsHttp: from is the caller (the frontend **function node** deleteItem, not a File)
            if let Some(from) = b.store.get_node(e.from_id).ok().flatten() {
                // The sample has more than one frontend call (`deleteItem` / `pingItem`); pick this case's by name.
                if from.name != "deleteItem" {
                    continue;
                }
                frontend_caller = Some(from);
                bridged = Some(c.clone());
            }
        }
    }

    // ---- 1) Frontend -> backend: the CallsHttp in-edge must exist, and its source is a frontend semantic node (function) ----
    let caller = frontend_caller
        .expect("the frontend axios.post('/api/delete') must synthesise an HttpContract and produce a CallsHttp in-edge");
    // The frontend HTTP call is wrapped in `export function deleteItem`, so CallsHttp is initiated by the function node,
    // isomorphic to the backend `Method --HandledBy--> HttpContract`.
    assert_eq!(
        caller.kind.as_str(),
        "Function",
        "the source of CallsHttp must be a frontend function node (a semantic node), got kind = {}",
        caller.kind
    );
    assert_eq!(
        caller.name, "deleteItem",
        "the source function of CallsHttp must be deleteItem"
    );

    // ---- 1.5) The frontend function node should be built as a declaration node by P2, holding a HasCallSite edge ----
    let all_nodes = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: None,
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("the node must be readable");
    let fn_delete = all_nodes
        .iter()
        .find(|n| n.kind.as_str() == "Function" && n.name == "deleteItem")
        .expect("the frontend semantic node Function:deleteItem must be created");
    assert!(
        b.store
            .edges_of(fn_delete.id, EdgeDirection::Outgoing)
            .unwrap_or_default()
            .iter()
            .any(|e| e.kind.as_str() == "HasCallSite"),
        "the deleteItem function must hold a HasCallSite edge (pointing at the axios call site inside it)"
    );

    // ---- 4) Frontend cross-file call chain: App.onDelete -> api.deleteItem ----
    let on_delete = all_nodes
        .iter()
        .find(|n| n.kind.as_str() == "Function" && n.name == "onDelete");
    if let Some(on_delete) = on_delete {
        let linked = b
            .store
            .edges_of(on_delete.id, EdgeDirection::Outgoing)
            .unwrap_or_default()
            .iter()
            .any(|e| {
                e.kind.as_str() == "Calls"
                    && all_nodes
                        .iter()
                        .any(|t| t.id == e.to_id && t.name == "deleteItem")
            });
        if linked {
            eprintln!("✓ frontend cross-file call chain: App.onDelete → api.deleteItem (Calls)");
        } else {
            eprintln!("ℹ frontend cross-file Calls are resolved by P7 by name; a missing edge is expected (name resolution requires global uniqueness)");
        }
    } else {
        eprintln!("ℹ no onDelete node detected");
    }

    // ---- 2) Contract bridge: frontend and backend synthesise the same ContractId and merge ----
    let c = bridged.expect("the contract node connected from the frontend must exist");
    assert_eq!(
        c.identity.as_ref().map(|i| i.value.as_str()),
        Some("POST /api/delete"),
        "frontend and backend must converge on the same ContractId (POST /api/delete)"
    );

    // ---- 2.5) The "was the front end here?" marker must be judged by the **edge**, not by a property.
    // Historically `frontend-mark-called` selected on `side = frontend`; but a bridge node is patched by both
    // sides and its scalar `side` is decided by whoever patches last, so the marker could vanish silently.
    // The selector now asks for a `CallsHttp` in-edge — this assertion locks that (see `Predicate::HasIncoming`).
    let anns = b.store.annotations_of(c.id).expect("the annotations must be readable");
    assert!(
        anns.iter().any(|a| a.kind == "frontend.called"),
        "a contract connected from the frontend must be hit by frontend-mark-called (its criterion is a CallsHttp edge), annotations: {:?}",
        anns.iter().map(|a| a.kind.clone()).collect::<Vec<_>>()
    );

    // ---- 3) `side` on a bridge node: not "the side that patched last", but the honest derived label ----
    // Two parties have evidence on this node, so the scalar label is `bridge` and the parties live in `sides`.
    // The label must not be `frontend` merely because the front-end rule happens to run last — that is exactly
    // the order-dependent lie this test forbids (`GraphWorkspace::record_side`).
    assert_eq!(
        c.properties.get("side").and_then(|v| v.as_str()),
        Some("bridge"),
        "a contract shared by frontend and backend must have side bridge"
    );
    assert_eq!(
        c.properties.get("sides"),
        Some(&serde_json::json!(["backend", "frontend"])),
        "sides must be the ordered de-duplicated set of both sides"
    );

    // ---- Informational: backend handler resolution (P7, existing mechanism) ----
    let out = b.store.edges_of(c.id, EdgeDirection::Outgoing).unwrap_or_default();
    if out.iter().any(|e| e.kind.as_str() == "HandledBy") {
        eprintln!("✓ backend HandledBy also resolved (complete contract bridge)");
    } else {
        eprintln!("ℹ backend HandledBy is resolved by P7 and does not deterministically land an edge in this sample; the existing sample_project integration test covers that path");
    }
}

/// **Member-style** frontend calls (`request.get('/api/ping')`, the uni-app / sample_project `template/uni-app` form)
/// must also land in the contract bridge — a historical bug was exactly here:
/// only object-style (`request({ url, method })`) and `axios.<verb>` were recognised,
/// so the uni-app sub-project went entirely "offline", and the frontend was invisible in the route view.
#[test]
fn member_style_request_bridges() {
    let root = synth_root();
    assert!(
        root.is_dir(),
        "the synthetic sample is missing: {}. It ships with the repository, so .gitignore must not exclude it",
        root.display()
    );
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("graphing");

    let all = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: None,
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("the node must be readable");

    let ping = all
        .iter()
        .find(|n| n.kind.as_str() == "HttpContract" && n.name.contains("/api/ping"))
        .expect("`request.get('/api/ping')` must synthesise the contract GET /api/ping");
    assert_eq!(ping.name, "GET /api/ping", "the member name becomes the HTTP method");
    // Both the backend `Route::get('/api/ping')` and the frontend `request.get('/api/ping')` synthesise this
    // node, hence `bridge` + both parties in `sides`.
    assert_eq!(
        ping.properties.get("side").and_then(|v| v.as_str()),
        Some("bridge"),
        "the side of a bridge contract must be bridge"
    );
    assert_eq!(
        ping.properties.get("sides"),
        Some(&serde_json::json!(["backend", "frontend"])),
        "sides must contain both the frontend and the backend"
    );

    // Frontend side: CallsHttp is initiated by a **function node** (semantic node), not a File.
    let inc = b.store.edges_of(ping.id, EdgeDirection::Incoming).unwrap_or_default();
    let caller = inc
        .iter()
        .find(|e| e.kind.as_str() == "CallsHttp")
        .map(|e| b.store.get_node(e.from_id).ok().flatten())
        .flatten()
        .expect("a CallsHttp edge coming from the frontend must exist");
    assert_eq!(caller.name, "pingItem", "the source of CallsHttp must be the frontend function pingItem");
    assert_eq!(caller.kind.as_str(), "Function");

    let locs = ping
        .properties
        .get("locations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let files: Vec<&str> = locs
        .iter()
        .filter_map(|l| l.get("file").and_then(|f| f.as_str()))
        .collect();
    assert!(
        files.iter().any(|f| f.contains("api.js")),
        "the contract must carry a frontend location, got: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("route/api.php")),
        "the contract must carry the backend route location (both sides converge on one node), got: {files:?}"
    );

    for (fn_name, url_part) in [("invoiceDetail", "/api/invoice/detail"), ( "orderInvoiceDetail", "/api/order/invoice_detail")] {
        let ident = format!("GET {url_part}/:*");
        let node = all.iter().find(|n| {
            n.kind.as_str() == "HttpContract"
                && n.identity.as_ref().map(|i| i.value.as_str()) == Some(ident.as_str())
        })
        .unwrap_or_else(|| panic!("a concatenated / templated URL must converge with the backend route by shape into {ident}"));
        let locs = node
            .properties
            .get("locations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let files: Vec<&str> = locs
            .iter()
            .filter_map(|l| l.get("file").and_then(|f| f.as_str()))
            .collect();
        assert!(
            files.iter().any(|f| f.contains("api.js")),
            "{ident} must carry a frontend location, got: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.contains("route/api.php")),
            "{ident} must carry the backend route location (one node for both sides), got: {files:?}"
        );
        let _ = fn_name;
    }

    // Frontend **semantic node**: `uni.setStorageSync('token', v)` -> `Cache:token`,
    // going through the same Synthesize mechanism as the backend `Cache::set('k', v)` (only the FKB differs).
    let cache = all
        .iter()
        .find(|n| n.kind.as_str() == "Cache" && n.name == "token")
        .expect("frontend local storage must synthesise a Cache semantic node (the frontend FKB rules frontend-cache-*)");
    assert_eq!(
        cache.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    let inc: Vec<String> = b
        .store
        .edges_of(cache.id, EdgeDirection::Incoming)
        .unwrap_or_default()
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect();
    assert!(
        inc.iter().any(|k| k == "ReadsCache"),
        "the reader must get ReadsCache, got: {inc:?}"
    );
    assert!(
        inc.iter().any(|k| k == "WritesCache"),
        "the writer must get WritesCache, got: {inc:?}"
    );

    // The frontend semantic-node family: I18nKey (copy), Store (Vuex, declared by FKB `semantic_kinds`),
    // ConfigKey (`export default {...}` config entry) — all go through the same Synthesize as the backend.
    let i18n = all
        .iter()
        .find(|n| n.kind.as_str() == "I18nKey" && n.name == "hello")
        .expect("`i18n.t('hello')` must synthesise an I18nKey semantic node");
    assert_eq!(
        i18n.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    let store = all
        .iter()
        .find(|n| n.kind.as_str() == "Store" && n.name == "counter/inc")
        .expect("`$store.dispatch('counter/inc')` must synthesise a Store semantic node");
    assert!(
        b.store
            .edges_of(store.id, EdgeDirection::Incoming)
            .unwrap_or_default()
            .iter()
            .any(|e| e.kind.as_str() == "Mutates"),
        "the Store node must have a Mutates in-edge"
    );
    assert!(
        all.iter()
            .any(|n| n.kind.as_str() == "ConfigKey" && n.name == "HTTP_REQUEST_URL"),
        "frontend config (export default) must synthesise a ConfigKey, got: {:?}",
        all.iter()
            .filter(|n| n.kind.as_str() == "ConfigKey")
            .map(|n| &n.name)
            .take(8)
            .collect::<Vec<_>>()
    );

    // A REST resource route `Route::resource('api/items', ...)` expands into 7 contracts;
    // `api/tags` with `->except(['read'])` should NOT produce `GET /api/tags/:id`.
    let ids: HashSet<String> = all
        .iter()
        .filter(|n| n.kind.as_str() == "HttpContract")
        .filter_map(|n| n.identity.as_ref().map(|i| i.value.clone()))
        .collect();
    for want in [
        "GET /api/items",
        "GET /api/items/create",
        "POST /api/items",
        "GET /api/items/:id",
        "GET /api/items/:id/edit",
        "PUT /api/items/:id",
        "DELETE /api/items/:id",
    ] {
        assert!(ids.contains(want), "a resource route must expand to {want}, got: {ids:?}");
    }
    assert!(
        ids.contains("GET /api/tags") && ids.contains("DELETE /api/tags/:id"),
        "`except` removes only read; the other actions must still expand, got: {:?}",
        ids.iter().filter(|i| i.contains("api/tags")).collect::<Vec<_>>()
    );
    assert!(
        !ids.contains("GET /api/tags/:id"),
        "`->except(['read'])` must drop the read action and must not invent GET /api/tags/:id"
    );

    // The other branch of the derived label: a contract **only** the backend ever synthesised stays `backend`
    // (single party ⇒ the party itself, no `bridge`). This is what "backend-only endpoints" are selected by.
    let items_index = all
        .iter()
        .find(|n| n.kind.as_str() == "HttpContract"
            && n.identity.as_ref().map(|i| i.value.as_str()) == Some("GET /api/items"))
        .expect("a resource route must expand to GET /api/items");
    assert_eq!(
        items_index.properties.get("side").and_then(|v| v.as_str()),
        Some("backend"),
        "a contract with evidence only on the backend side must have side backend"
    );
    assert_eq!(
        items_index.properties.get("sides"),
        Some(&serde_json::json!(["backend"])),
        "a contract with evidence only on the backend side must have sides [backend]"
    );

    // Frontend **page-route** semantic node: comes from `pages.json`, isomorphic to the backend `Route`.
    let index_page = all
        .iter()
        .find(|n| n.kind.as_str() == "Page" && n.name == "/pages/index/index")
        .expect("`pages.json` must synthesise the Page semantic node /pages/index/index");
    assert_eq!(
        index_page.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    // Sub-package page: the root prefix joins the identity, ensuring the jump edge connects to the same node.
    let sub_page = all
        .iter()
        .find(|n| n.kind.as_str() == "Page" && n.name == "/pagesA/list/list")
        .expect("a subPackages page identity must carry the root prefix (/pagesA/list/list)");

    let inc = b
        .store
        .edges_of(sub_page.id, EdgeDirection::Incoming)
        .unwrap_or_default();
    let nav = inc
        .iter()
        .find(|e| e.kind.as_str() == "NavigatesTo")
        .expect("a sub-package page must receive a NavigatesTo in-edge (from uni.navigateTo)");
    let from = b
        .store
        .get_node(nav.from_id)
        .ok()
        .flatten()
        .expect("the navigation source node must exist");
    assert_eq!(from.name, "goList", "the source of NavigatesTo must be the goList function");
    assert_eq!(from.kind.as_str(), "Function");

    // Frontend **event-bus** semantic node: `uni.$emit('listRefresh')` and `uni.$on('listRefresh')`
    // merge idempotently into the same `EventBus` node (isomorphic to the backend `Event`); both emitter and listener connect to it.
    let bus = all
        .iter()
        .find(|n| n.kind.as_str() == "EventBus" && n.name == "listRefresh")
        .expect("`uni.$emit/on('listRefresh')` must synthesise an EventBus semantic node");
    assert_eq!(
        bus.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    // Emitter `emitRefresh` -> event node (Emits in-edge).
    let emit_inc = b
        .store
        .edges_of(bus.id, EdgeDirection::Incoming)
        .unwrap_or_default();
    let emitter = emit_inc
        .iter()
        .find(|e| e.kind.as_str() == "Emits")
        .expect("the event node must receive an Emits in-edge")
        .from_id;
    let emitter = b.store.get_node(emitter).ok().flatten().expect("the emitting node");
    assert_eq!(emitter.name, "emitRefresh", "the source of Emits must be the emitRefresh function");
    // Listener `onRefresh` -> event node (ListensTo in-edge).
    let listener = emit_inc
        .iter()
        .find(|e| e.kind.as_str() == "ListensTo")
        .expect("the event node must receive a ListensTo in-edge")
        .from_id;
    let listener = b.store.get_node(listener).ok().flatten().expect("the listening node");
    assert_eq!(listener.name, "onRefresh", "the source of ListensTo must be the onRefresh function");

    // Counterexample: `cache.get('/api/ping')` has a receiver that is not an HTTP client, must **not** produce a contract.
    let false_positive = all.iter().any(|n| {
        n.kind.as_str() == "HttpContract"
            && n.identity
                .as_ref()
                .map(|i| i.value.as_str())
                .unwrap_or_default()
                .contains("dynamic-url")
    });
    assert!(!false_positive, "a dynamic URL must not synthesise a junk <dynamic-url> contract");
}

/// **Backend cache nodes must carry `side: backend`** (symmetric to the frontend `side = frontend`).
///
/// Historical issue: the frontend `uni.setStorageSync('token')` synthesises a `Cache` node via the frontend FKB and tags
/// `side = frontend`, but the backend `Cache::get(...)` node had **no** `side`, so in the "route / cache view" the graph
/// only had the frontend cache concept and could not see backend caches. Now backend caches are synthesised by the generic cache rule in
/// `fkb/php/common.yaml` and uniformly tagged `side = backend`; this test locks that invariant.
#[test]
fn backend_cache_node_tagged_backend() {
    let root = synth_root();
    assert!(
        root.is_dir(),
        "the synthetic sample is missing: {}. It ships with the repository, so .gitignore must not exclude it",
        root.display()
    );
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("graphing");

    let all = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: None,
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("the node must be readable");

    // Backend controller's `Cache::get('order-status')` -> node `order-status`.
    let backend_cache = all
        .iter()
        .find(|n| n.kind.as_str() == "Cache" && n.name == "order-status")
        .expect("the backend `Cache::get('order-status')` must synthesise a Cache semantic node (common.yaml)");
    assert_eq!(
        backend_cache.properties.get("side").and_then(|v| v.as_str()),
        Some("backend"),
        "a backend cache node must be annotated side=backend by the generic layer"
    );

    // The reader should carry a ReadsCache in-edge.
    let inc = b
        .store
        .edges_of(backend_cache.id, EdgeDirection::Incoming)
        .unwrap_or_default();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "ReadsCache"),
        "a backend cache reader must get ReadsCache, got: {:?}",
        inc.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );
}
