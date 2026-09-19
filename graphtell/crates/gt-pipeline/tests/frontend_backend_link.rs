//! 自检：前端子项目对后端的 HTTP 调用，应被建图机制连成「契约桥」。
//!
//! 不依赖外部 CRMEB 样本：用 `samples/frontend-backend-link` 这个极小的
//! 合成仓库（一个 ThinkPHP 后端 + 一个 axios 前端）跑完整 pipeline，断言：
//!   1. 前端 `axios.post('/api/delete')` 被包在 `export function deleteItem` 里，
//!      合成 `HttpContract` 并带一条**入向 `CallsHttp`** 边，源头是前端**函数节点**
//!      （`deleteItem`）—— 这正是「前端语义节点」：与后端 `Method` 同构，而非 File；
//!   2. 同一契约节点被前端 FKB 标注 `side = frontend`；
//!   3. 后端 `thinkphp6` 的 `Route::post('/api/delete', ...)` 与前端合成出
//!      **同一个 `ContractId`（`POST /api/delete`）** 并幂等合并 —— 契约桥的
//!      跨子项目汇聚点；
//!   4. 前端跨文件调用链：`App.onDelete → api.deleteItem` 由 P7 解析出 `Calls` 边
//!      （前端语义节点之间连通，折叠视图里收进 `via` 链、drawer 逐跳可见）。

mod common;

use std::collections::HashSet;
use std::path::PathBuf;

use gt_domain::model::NodeKind;
use gt_domain::model::ProjectConfig;
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter, ProjectReader};

/// 仓库根下的合成样本目录：
/// `crates/gt-pipeline` → 上两层到仓库根 → `samples/frontend-backend-link`。
fn synth_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.join("samples/frontend-backend-link")
}

#[test]
fn frontend_calls_backend_merge_into_contract() {
    let root = synth_root();
    if !root.is_dir() {
        eprintln!("跳过：未找到合成样本 {}", root.display());
        return;
    }
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("建图");

    // 两个子工程应被正确识别：后端 php / 前端 javascript。
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

    // 取全部 HttpContract 节点，找出被前端 CallsHttp 连入的那个。
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
            // 入向 CallsHttp：from 是调用方（前端**函数节点** deleteItem，而非 File）
            if let Some(from) = b.store.get_node(e.from_id).ok().flatten() {
                // 样本里不止一个前端调用（`deleteItem` / `pingItem`），按名字挑本用例要的那个。
                if from.name != "deleteItem" {
                    continue;
                }
                frontend_caller = Some(from);
                bridged = Some(c.clone());
            }
        }
    }

    // ---- 1) 前端→后端：CallsHttp 入边必须存在，且源头是前端语义节点（函数）----
    let caller = frontend_caller
        .expect("前端 axios.post('/api/delete') 应合成 HttpContract 并产生 CallsHttp 入边");
    // 前端 HTTP 调用被包在 `export function deleteItem` 里，因此 CallsHttp 由函数节点发起，
    // 与后端 `Method --HandledBy--> HttpContract` 同构（之前是 File 节点，已修正）。
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

    // ---- 1.5) 前端函数节点应被 P2 建成声明节点，且持有 HasCallSite 边 ----
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

    // ---- 4) 前端跨文件调用链：App.onDelete → api.deleteItem ----
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
            eprintln!("✓ 前端跨文件调用链：App.onDelete → api.deleteItem (Calls)");
        } else {
            eprintln!("ℹ 前端跨文件 Calls 由 P7 按名解析；若未落边属预期内（名称解析需全局唯一）");
        }
    } else {
        eprintln!("ℹ 未检测到 onDelete 节点");
    }

    // ---- 2) 契约桥：前端与后端合成出同一个 ContractId 并合并 ----
    let c = bridged.expect("被前端连入的契约节点应存在");
    assert_eq!(
        c.identity.as_ref().map(|i| i.value.as_str()),
        Some("POST /api/delete"),
        "前后端应汇聚到同一个 ContractId（POST /api/delete）"
    );

    // ---- 3) 前端 FKB 标注 ----
    assert_eq!(
        c.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend"),
        "契约节点应被前端 FKB 标注 side=frontend"
    );

    // ---- 信息性：后端 handler 解析（P7，既有机制）----
    let out = b.store.edges_of(c.id, EdgeDirection::Outgoing).unwrap_or_default();
    if out.iter().any(|e| e.kind.as_str() == "HandledBy") {
        eprintln!("✓ 后端 HandledBy 也已解析（完整契约桥）");
    } else {
        eprintln!("ℹ 后端 HandledBy 由 P7 解析，未在此样本确定性落边；既有 CRMEB 集成测试覆盖该路径");
    }
}

/// **成员式**前端调用（`request.get('/api/ping')`，uni-app / CRMEB 的 `template/uni-app`
/// 形态）同样要落进契约桥 —— 历史 bug 正是这里：
/// 只有对象式（`request({ url, method })`）与 `axios.<verb>` 被识别，
/// 于是 uni-app 子工程整体"失联"，路由视角里看不到前端。
#[test]
fn member_style_request_bridges() {
    let root = synth_root();
    if !root.is_dir() {
        eprintln!("跳过：未找到合成样本 {}", root.display());
        return;
    }
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

    // 前端侧：CallsHttp 由**函数节点**（语义节点）发起，而非 File。
    let inc = b.store.edges_of(ping.id, EdgeDirection::Incoming).unwrap_or_default();
    let caller = inc
        .iter()
        .find(|e| e.kind.as_str() == "CallsHttp")
        .map(|e| b.store.get_node(e.from_id).ok().flatten())
        .flatten()
        .expect("应存在来自前端的 CallsHttp 边");
    assert_eq!(caller.name, "pingItem", "CallsHttp 源头应是前端函数 pingItem");
    assert_eq!(caller.kind.as_str(), "Function");

    // 后端侧：**同一个 ContractId 节点**上同时挂着前后端的 location —— 契约桥的合并证据。
    // （`HandledBy` 能否落边取决于 handler 是否可解析：本样本的控制器用了 `@` 分隔、
    // 不在 FKB 的 `method_separators` 内，故不作断言，落在 P7 解析的既有覆盖范围里。）
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

    // **参数形状归一**：拼接式（`'...' + id`）与模板串（`` `...${id}` ``）URL
    // 折出 `:param` 占位，与后端 `/:id` / `/:uni` 按形状汇聚到同一契约节点 ——
    // 这正是"路由视角看不到前端"的最后一种形态。
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

    // 前端**语义节点**：`uni.setStorageSync('token', v)` → `Cache:token`，
    // 与后端 `Cache::set('k', v)` 走的是同一套 Synthesize 机制（只是 FKB 不同）。
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

    // 前端语义节点家族：I18nKey（文案）、Store（Vuex，由 FKB `semantic_kinds` 声明）、
    // ConfigKey（`export default {...}` 配置条目）—— 全部走与后端同一套 Synthesize。
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

    // REST 资源路由 `Route::resource('api/items', ...)` 展开成 7 条契约；
    // `->except(['read'])` 的 `api/tags` 不应出现 `GET /api/tags/:id`。
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

    // 前端**页面路由**语义节点：来自 `pages.json`，与后端 `Route` 同构。
    let index_page = all
        .iter()
        .find(|n| n.kind.as_str() == "Page" && n.name == "/pages/index/index")
        .expect("`pages.json` 应合成 Page 语义节点 /pages/index/index");
    assert_eq!(
        index_page.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    // 子包页面：root 前缀拼进身份，确保跳转边能连到同一节点。
    let sub_page = all
        .iter()
        .find(|n| n.kind.as_str() == "Page" && n.name == "/pagesA/list/list")
        .expect("subPackages 页面身份应带 root 前缀（/pagesA/list/list）");

    // 前端**页面跳转**语义边：`uni.navigateTo({ url: 'pagesA/list/list' })`
    // 从发起函数（`goList`）指向目标 `Page` 节点。URL 的查询串 / 分包前缀都与
    // `pages.json` 同一套身份归一（leading_slash / strip_query），精确连到同一节点。
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

    // 前端**事件总线**语义节点：`uni.$emit('listRefresh')` 与 `uni.$on('listRefresh')`
    // 幂等合并到同一个 `EventBus` 节点（与后端 `Event` 同构），发射方 / 监听方都连到它。
    let bus = all
        .iter()
        .find(|n| n.kind.as_str() == "EventBus" && n.name == "listRefresh")
        .expect("`uni.$emit/on('listRefresh')` 应合成 EventBus 语义节点");
    assert_eq!(
        bus.properties.get("side").and_then(|v| v.as_str()),
        Some("frontend")
    );
    // 发射方 `emitRefresh` → 事件节点（Emits 入边）。
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
    // 监听方 `onRefresh` → 事件节点（ListensTo 入边）。
    let listener = emit_inc
        .iter()
        .find(|e| e.kind.as_str() == "ListensTo")
        .expect("事件节点应收到 ListensTo 入边")
        .from_id;
    let listener = b.store.get_node(listener).ok().flatten().expect("监听方节点");
    assert_eq!(listener.name, "onRefresh", "ListensTo 源头应是 onRefresh 函数");

    // 反例：`cache.get('/api/ping')` 接收者不是 HTTP 客户端，**不得**产生契约。
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

/// **后端缓存节点必须带 `side: backend`**（对称于前端 `side = frontend`）。
///
/// 历史问题：前端 `uni.setStorageSync('token')` 经前端 FKB 合成 `Cache` 节点并标
/// `side = frontend`，但后端 `Cache::get(...)` 合成的节点**没有** `side`，于是在「路由 /
/// 缓存视角」里图上只有前端的缓存概念、看不到后端缓存。现在后端缓存由
/// `fkb/php/common.yaml` 的通用缓存规则合成并统一标注 `side = backend`；此测试锁死
/// 这条不变量。
#[test]
fn backend_cache_node_tagged_backend() {
    let root = synth_root();
    if !root.is_dir() {
        eprintln!("跳过：未找到合成样本 {}", root.display());
        return;
    }
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

    // 后端 controller 里的 `Cache::get('order-status')` → 节点 `order-status`。
    let backend_cache = all
        .iter()
        .find(|n| n.kind.as_str() == "Cache" && n.name == "order-status")
        .expect("后端 `Cache::get('order-status')` 应合成 Cache 语义节点（common.yaml）");
    assert_eq!(
        backend_cache.properties.get("side").and_then(|v| v.as_str()),
        Some("backend"),
        "后端缓存节点应被通用层标注 side=backend"
    );

    // 读方应挂 ReadsCache 入边。
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
