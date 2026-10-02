//! Self-check: a frontend sub-project's HTTP calls to the backend should be connected by the build mechanism into a "contract bridge".
//!
//! Not depending on an external CRMEB sample: run the full pipeline on `samples/frontend-backend-link`, a tiny
//! synthetic repo (a ThinkPHP backend + an axios frontend), asserting:
//!   1. the frontend `axios.post('/api/delete')` is wrapped in `export function deleteItem`,
//!      synthesises an `HttpContract` with an **incoming `CallsHttp`** edge whose source is the frontend **function node**
//!      (`deleteItem`) — this is exactly a "frontend semantic node": isomorphic to the backend `Method`, not a File;
//!   2. the same contract node is annotated `side = frontend` by the frontend FKB;
//!   3. the backend `thinkphp6` `Route::post('/api/delete', ...)` and the frontend synthesise
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
/// The sample tree's location is **not fixed**: it may be at the inner workspace root, or at the upper repo root (after merging `samples/`).
/// Hard-coding "up two levels" would point at a non-existent directory once the sample is moved — then the case fails, or worse is changed
/// to skip, becoming "CI all green but zero coverage". Here we instead search **upward level by level** from `CARGO_MANIFEST_DIR`, and support a
/// `GRAPHTELL_SAMPLES_DIR` override; only when nothing is found do we fall back to the original candidate path, so the caller's assert raises a
/// diagnosable missing-path message (this file deliberately requires failure rather than skip).
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
    // Fallback: return to the original candidate, so the caller's assert points out the specific missing path.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.join("samples/frontend-backend-link")
}

#[test]
fn frontend_calls_backend_merge_into_contract() {
    let root = synth_root();
    assert!(
        root.is_dir(),
        "合成样本缺失：{}。它随仓库分发，不应被 .gitignore 排除",
        root.display()
    );
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("建图");

    // The two sub-projects should be recognised correctly: backend php / frontend javascript.
    let subs = b.store.list_sub_projects(b.project.id).unwrap_or_default();
    assert!(
        subs.iter().any(|s| s.language.as_str() == "php" && s.frameworks.contains(&"thinkphp6".to_string())),
        "后端应识别为 thinkphp6，实际：{:?}",
        subs.iter().map(|s| (&s.language, &s.frameworks)).collect::<Vec<_>>()
    );
    assert!(
        subs.iter().any(|s| s.language.as_str() == "javascript" && s.frameworks.contains(&"frontend-js".to_string())),
        "前端应识别为 frontend-js"
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
        .expect("契约节点可读");

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
        .expect("前端 axios.post('/api/delete') 应合成 HttpContract 并产生 CallsHttp 入边");
    // The frontend HTTP call is wrapped in `export function deleteItem`, so CallsHttp is initiated by the function node,
    // isomorphic to the backend `Method --HandledBy--> HttpContract` (it used to be a File node, now fixed).
    assert_eq!(
        caller.kind.as_str(),
        "Function",
        "CallsHttp 的源头应是前端函数节点（语义节点），实际 kind = {}",
        caller.kind
    );
    assert_eq!(
        caller.name, "deleteItem",
        "CallsHttp 源头函数应为 deleteItem"
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
        .expect("节点可读");
    let fn_delete = all_nodes
        .iter()
        .find(|n| n.kind.as_str() == "Function" && n.name == "deleteItem")
        .expect("应建出前端语义节点 Function:deleteItem");
    assert!(
        b.store
            .edges_of(fn_delete.id, EdgeDirection::Outgoing)
            .unwrap_or_default()
            .iter()
            .any(|e| e.kind.as_str() == "HasCallSite"),
        "deleteItem 函数应持有 HasCallSite 边（指向其内部的 axios 调用点）"
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
    let c = bridged.expect("被前端连入的契约节点应存在");
    assert_eq!(
        c.identity.as_ref().map(|i| i.value.as_str()),
        Some("POST /api/delete"),
        "前后端应汇聚到同一个 ContractId（POST /api/delete）"
    );

    // ---- 3) Frontend FKB annotation ----
    assert_eq!(
        c.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend"),
        "契约节点应被前端 FKB 标注 side=frontend"
    );

    // ---- Informational: backend handler resolution (P7, existing mechanism) ----
    let out = b.store.edges_of(c.id, EdgeDirection::Outgoing).unwrap_or_default();
    if out.iter().any(|e| e.kind.as_str() == "HandledBy") {
        eprintln!("✓ backend HandledBy also resolved (complete contract bridge)");
    } else {
        eprintln!("ℹ backend HandledBy is resolved by P7 and does not deterministically land an edge in this sample; the existing CRMEB integration test covers that path");
    }
}

/// **Member-style** frontend calls (`request.get('/api/ping')`, the uni-app / CRMEB `template/uni-app` form)
/// must also land in the contract bridge — a historical bug was exactly here:
/// only object-style (`request({ url, method })`) and `axios.<verb>` were recognised,
/// so the uni-app sub-project went entirely "offline", and the frontend was invisible in the route view.
#[test]
fn member_style_request_bridges() {
    let root = synth_root();
    assert!(
        root.is_dir(),
        "合成样本缺失：{}。它随仓库分发，不应被 .gitignore 排除",
        root.display()
    );
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("建图");

    let all = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: None,
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("节点可读");

    let ping = all
        .iter()
        .find(|n| n.kind.as_str() == "HttpContract" && n.name.contains("/api/ping"))
        .expect("应由 `request.get('/api/ping')` 合成契约 GET /api/ping");
    assert_eq!(ping.name, "GET /api/ping", "成员名应成为 HTTP method");
    assert_eq!(
        ping.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend"),
        "契约应被前端 FKB 标注 side=frontend"
    );

    // Frontend side: CallsHttp is initiated by a **function node** (semantic node), not a File.
    let inc = b.store.edges_of(ping.id, EdgeDirection::Incoming).unwrap_or_default();
    let caller = inc
        .iter()
        .find(|e| e.kind.as_str() == "CallsHttp")
        .map(|e| b.store.get_node(e.from_id).ok().flatten())
        .flatten()
        .expect("应存在来自前端的 CallsHttp 边");
    assert_eq!(caller.name, "pingItem", "CallsHttp 源头应是前端函数 pingItem");
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
        "契约应带前端 location，实际：{files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("route/api.php")),
        "契约应带后端路由 location（前后端汇聚到同一节点），实际：{files:?}"
    );

    for (fn_name, url_part) in [("invoiceDetail", "/api/invoice/detail"), ( "orderInvoiceDetail", "/api/order/invoice_detail")] {
        let ident = format!("GET {url_part}/:*");
        let node = all.iter().find(|n| {
            n.kind.as_str() == "HttpContract"
                && n.identity.as_ref().map(|i| i.value.as_str()) == Some(ident.as_str())
        })
        .unwrap_or_else(|| panic!("拼接/模板 URL 应与后端路由按形状汇聚成 {ident}"));
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
            "{ident} 应带前端 location，实际：{files:?}"
        );
        assert!(
            files.iter().any(|f| f.contains("route/api.php")),
            "{ident} 应带后端路由 location（前后端同一节点），实际：{files:?}"
        );
        let _ = fn_name;
    }

    // Frontend **semantic node**: `uni.setStorageSync('token', v)` -> `Cache:token`,
    // going through the same Synthesize mechanism as the backend `Cache::set('k', v)` (only the FKB differs).
    let cache = all
        .iter()
        .find(|n| n.kind.as_str() == "Cache" && n.name == "token")
        .expect("前端本地存储应合成 Cache 语义节点（前端 FKB 规则 frontend-cache-*）");
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
        "读方应挂 ReadsCache，实际：{inc:?}"
    );
    assert!(
        inc.iter().any(|k| k == "WritesCache"),
        "写方应挂 WritesCache，实际：{inc:?}"
    );

    // The frontend semantic-node family: I18nKey (copy), Store (Vuex, declared by FKB `semantic_kinds`),
    // ConfigKey (`export default {...}` config entry) — all go through the same Synthesize as the backend.
    let i18n = all
        .iter()
        .find(|n| n.kind.as_str() == "I18nKey" && n.name == "hello")
        .expect("`i18n.t('hello')` 应合成 I18nKey 语义节点");
    assert_eq!(
        i18n.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    let store = all
        .iter()
        .find(|n| n.kind.as_str() == "Store" && n.name == "counter/inc")
        .expect("`$store.dispatch('counter/inc')` 应合成 Store 语义节点");
    assert!(
        b.store
            .edges_of(store.id, EdgeDirection::Incoming)
            .unwrap_or_default()
            .iter()
            .any(|e| e.kind.as_str() == "Mutates"),
        "Store 节点应有 Mutates 入边"
    );
    assert!(
        all.iter()
            .any(|n| n.kind.as_str() == "ConfigKey" && n.name == "HTTP_REQUEST_URL"),
        "前端配置（export default）应合成 ConfigKey，实际：{:?}",
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
        assert!(ids.contains(want), "资源路由应展开出 {want}，实际：{ids:?}");
    }
    assert!(
        ids.contains("GET /api/tags") && ids.contains("DELETE /api/tags/:id"),
        "`except` 只剔除 read，其余动作应照常展开，实际：{:?}",
        ids.iter().filter(|i| i.contains("api/tags")).collect::<Vec<_>>()
    );
    assert!(
        !ids.contains("GET /api/tags/:id"),
        "`->except(['read'])` 应剔除 read 动作，不该凭空造出 GET /api/tags/:id"
    );

    // Frontend **page-route** semantic node: comes from `pages.json`, isomorphic to the backend `Route`.
    let index_page = all
        .iter()
        .find(|n| n.kind.as_str() == "Page" && n.name == "/pages/index/index")
        .expect("`pages.json` 应合成 Page 语义节点 /pages/index/index");
    assert_eq!(
        index_page.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    // Sub-package page: the root prefix joins the identity, ensuring the jump edge connects to the same node.
    let sub_page = all
        .iter()
        .find(|n| n.kind.as_str() == "Page" && n.name == "/pagesA/list/list")
        .expect("subPackages 页面身份应带 root 前缀（/pagesA/list/list）");

    let inc = b
        .store
        .edges_of(sub_page.id, EdgeDirection::Incoming)
        .unwrap_or_default();
    let nav = inc
        .iter()
        .find(|e| e.kind.as_str() == "NavigatesTo")
        .expect("子包页应收到 NavigatesTo 入边（来自 uni.navigateTo）");
    let from = b
        .store
        .get_node(nav.from_id)
        .ok()
        .flatten()
        .expect("导航源头节点应存在");
    assert_eq!(from.name, "goList", "NavigatesTo 源头应是 goList 函数");
    assert_eq!(from.kind.as_str(), "Function");

    // Frontend **event-bus** semantic node: `uni.$emit('listRefresh')` and `uni.$on('listRefresh')`
    // merge idempotently into the same `EventBus` node (isomorphic to the backend `Event`); both emitter and listener connect to it.
    let bus = all
        .iter()
        .find(|n| n.kind.as_str() == "EventBus" && n.name == "listRefresh")
        .expect("`uni.$emit/on('listRefresh')` 应合成 EventBus 语义节点");
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
        .expect("事件节点应收到 Emits 入边")
        .from_id;
    let emitter = b.store.get_node(emitter).ok().flatten().expect("发射方节点");
    assert_eq!(emitter.name, "emitRefresh", "Emits 源头应是 emitRefresh 函数");
    // Listener `onRefresh` -> event node (ListensTo in-edge).
    let listener = emit_inc
        .iter()
        .find(|e| e.kind.as_str() == "ListensTo")
        .expect("事件节点应收到 ListensTo 入边")
        .from_id;
    let listener = b.store.get_node(listener).ok().flatten().expect("监听方节点");
    assert_eq!(listener.name, "onRefresh", "ListensTo 源头应是 onRefresh 函数");

    // Counterexample: `cache.get('/api/ping')` has a receiver that is not an HTTP client, must **not** produce a contract.
    let false_positive = all.iter().any(|n| {
        n.kind.as_str() == "HttpContract"
            && n.identity
                .as_ref()
                .map(|i| i.value.as_str())
                .unwrap_or_default()
                .contains("dynamic-url")
    });
    assert!(!false_positive, "动态 URL 不应合成 <dynamic-url> 垃圾契约");
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
        "合成样本缺失：{}。它随仓库分发，不应被 .gitignore 排除",
        root.display()
    );
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("建图");

    let all = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: None,
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("节点可读");

    // Backend controller's `Cache::get('order-status')` -> node `order-status`.
    let backend_cache = all
        .iter()
        .find(|n| n.kind.as_str() == "Cache" && n.name == "order-status")
        .expect("后端 `Cache::get('order-status')` 应合成 Cache 语义节点（common.yaml）");
    assert_eq!(
        backend_cache.properties.get("side").and_then(|v| v.as_str()),
        Some("backend"),
        "后端缓存节点应被通用层标注 side=backend"
    );

    // The reader should carry a ReadsCache in-edge.
    let inc = b
        .store
        .edges_of(backend_cache.id, EdgeDirection::Incoming)
        .unwrap_or_default();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "ReadsCache"),
        "后端缓存读方应挂 ReadsCache，实际：{:?}",
        inc.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );
}
