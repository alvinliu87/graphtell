//! 以 `samples/CRMEB-master` 为材料的建图集成测试。
//!
//! 这些用例验证的是**端到端结论**，而不是某个函数的返回值：
//! 子工程是否被正确识别、依赖目录是否被排除、`AppRoot` 是否按 FKB 解析、
//! 语义节点是否幂等合并、路由链是否真的连上了控制器。

mod common;

use em_domain::model::{NodeKind, ProjectId, SubProjectId};
use em_domain::port::{
    DiagnosticSink, EdgeDirection, GraphQuery, NodeFilter, ParserRegistry, ProjectReader,
    SymbolTableReader,
};

use serde_json::Value;

fn built() -> Option<std::sync::Arc<common::Built>> {
    common::graph()
}

/// 语义节点判定（与视图一致）：第一类语义 kind（`kinds.rs` 的 `SYNTHESIZED`）。
fn is_semantic_node(n: &em_domain::model::Node) -> bool {
    NodeKind(n.kind.to_string()).is_semantic()
}

// ---------------------------------------------------------------- P0 Ingest

#[test]
fn ingest_detects_three_sub_projects() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let subs = b.store.list_sub_projects(b.project.id).expect("子工程可读");
    let names: Vec<&str> = subs.iter().map(|s| s.name.as_str()).collect();

    // CRMEB 是一个多技术栈仓库：ThinkPHP 后端 + 两个前端工程
    assert!(
        names.iter().any(|n| n.contains("crmeb")),
        "应识别出后端子工程，实际：{names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("uni-app")),
        "应识别出 Uni-app 前端子工程，实际：{names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("admin")),
        "应识别出管理端前端子工程，实际：{names:?}"
    );

    let backend = subs
        .iter()
        .find(|s| s.detected_by == "composer.json")
        .expect("后端应由 composer.json 识别");
    assert_eq!(backend.language.as_str(), "php");
    assert_eq!(backend.role, "backend");

    let frontend = subs
        .iter()
        .find(|s| s.detected_by == "package.json")
        .expect("前端应由 package.json 识别");
    assert_eq!(frontend.role, "frontend");
}

#[test]
fn ingest_excludes_dependency_and_asset_dirs() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let files = b.store.list_files(b.project.id, None).expect("文件可读");
    assert!(!files.is_empty(), "至少要扫描到一些源文件");

    for forbidden in ["vendor/", "node_modules/", "target/", ".git/"] {
        let leaked: Vec<&str> = files
            .iter()
            .filter(|f| f.path.contains(forbidden))
            .map(|f| f.path.as_str())
            .take(3)
            .collect();
        assert!(
            leaked.is_empty(),
            "{forbidden} 属于依赖目录，不应进入待分析集合：{leaked:?}"
        );
    }

    // 静态资源与二进制产物同样不该出现
    for ext in [".png", ".jpg", ".woff2", ".zip"] {
        let leaked: Vec<&str> = files
            .iter()
            .filter(|f| f.path.to_ascii_lowercase().ends_with(ext))
            .map(|f| f.path.as_str())
            .take(3)
            .collect();
        assert!(leaked.is_empty(), "静态资源 {ext} 不应进入待分析集合：{leaked:?}");
    }

    // 但业务源码必须在
    assert!(
        files.iter().any(|f| f.path.ends_with("crmeb/app/event.php")),
        "业务文件 crmeb/app/event.php 必须在待分析集合里"
    );
}

// ---------------------------------------------------------------- P2 CfAst

#[test]
fn cf_ast_builds_syntax_nodes_and_call_sites() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let stats = b.store.stats(b.project.id).expect("统计可读");
    for kind in [
        NodeKind::CLASS,
        NodeKind::METHOD,
        NodeKind::PROPERTY,
        NodeKind::NAMESPACE,
        NodeKind::CALL_SITE,
    ] {
        assert!(
            stats.by_kind.get(kind).copied().unwrap_or(0) > 0,
            "CfAst 应产出 {kind} 节点，实际统计：{:?}",
            stats.by_kind
        );
    }
    assert!(
        stats.by_kind.get(NodeKind::CALL_SITE).copied().unwrap_or(0) > 1000,
        "调用点应细化到 CallSite 节点"
    );
}

#[test]
fn cf_ast_resolves_php_namespace_and_fqn() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let nodes = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: Some("StoreOrder".into()),
            limit: Some(50),
            offset: Some(0),
        })
        .expect("查询可读");
    let hit = nodes
        .iter()
        .find(|n| n.fqn.as_deref() == Some("app\\model\\order\\StoreOrder"));
    assert!(
        hit.is_some(),
        "应能从 namespace + class 推出 FQN app\\model\\order\\StoreOrder，实际：{:?}",
        nodes.iter().map(|n| n.fqn.clone()).take(5).collect::<Vec<_>>()
    );
}

#[test]
fn cf_ast_follows_model_inheritance_chain() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    // StoreOrder extends BaseModel extends think\Model —— 末端在 vendor（被排除），
    // 但继承关系本身必须被记录，否则表推导会全丢
    let store_order = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: Some("StoreOrder".into()),
            limit: Some(50),
            offset: Some(0),
        })
        .expect("查询可读")
        .into_iter()
        .find(|n| n.fqn.as_deref() == Some("app\\model\\order\\StoreOrder"))
        .expect("StoreOrder 类应存在");

    let edges = b
        .store
        .edges_of(store_order.id, em_domain::port::EdgeDirection::Outgoing)
        .expect("边可读");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Extends" || e.kind.as_str() == "UsesTrait"),
        "应记录继承 / trait 使用关系，实际出边：{:?}",
        edges.iter().map(|e| e.kind.to_string()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------- P3 Prepare

#[test]
fn prepare_resolves_app_root_from_composer_psr4() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let subs = b.store.list_sub_projects(b.project.id).expect("子工程可读");
    let backend = subs
        .iter()
        .find(|s| s.language.as_str() == "php")
        .expect("存在 PHP 子工程");
    let facts: Value = serde_json::from_value(backend.facts.clone()).unwrap_or(Value::Null);
    let app_root = facts.get("app_root").expect("应解析出 app_root 事实");

    assert_eq!(
        app_root.get("value").and_then(|v| v.as_str()),
        Some("app"),
        "AppRoot 应来自 composer.json 的 autoload.psr-4"
    );
    assert_eq!(
        app_root.get("fallback_used").and_then(|v| v.as_bool()),
        Some(false),
        "既然 psr-4 解析成功，就不该使用兜底目录"
    );
    let source = app_root
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        source.contains("autoload.psr-4"),
        "解析依据必须记录在 source 里，实际：{source}"
    );
    assert!(
        backend.frameworks.contains(&"thinkphp6".to_string()),
        "应识别出 thinkphp6，实际：{:?}",
        backend.frameworks
    );
}

#[test]
fn prepare_loads_authoritative_symbol_tables() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let pid = b.project.id;
    for table in ["facade_map", "event_listeners", "container_bindings"] {
        let rows = b.store.list_symbols(pid, table).expect("符号表可读");
        assert!(!rows.is_empty(), "权威表 {table} 不应为空");
    }
    // schema 来自安装 SQL + 代码中的表名
    let schema = b.store.list_symbols(pid, "schema").expect("schema 可读");
    assert!(schema.len() > 50, "schema 应装载到大量表，实际 {}", schema.len());
    let user = b
        .store
        .get_symbol(pid, "schema", "user")
        .expect("可读")
        .expect("user 表应在 schema 中");
    let cols = user.get("columns").and_then(|c| c.as_array()).map(|a| a.len()).unwrap_or(0);
    assert!(cols > 5, "user 表应解析出多列，实际 {cols}");
}

#[test]
fn prepare_parses_sql_columns_without_being_cut_by_parentheses() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    // `int(11)` 里的右括号曾经让列解析提前结束，只剩 `id` 一列
    let user = b
        .store
        .get_symbol(b.project.id, "schema", "user")
        .expect("可读")
        .expect("user 表存在");
    let cols: Vec<String> = user
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    assert!(cols.contains(&"phone".to_string()), "user 表应含 phone 列：{cols:?}");
    assert!(cols.contains(&"uid".to_string()), "user 表应含 uid 列：{cols:?}");
}

// ---------------------------------------------------------------- P5 Synthesize

#[test]
fn synthesize_creates_http_contract_including_apple_login() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let contracts = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: Some("apple_login".into()),
            limit: Some(20),
            offset: Some(0),
        })
        .expect("查询可读");
    let hit = contracts
        .iter()
        .find(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some("POST /apple_login"));
    assert!(
        hit.is_some(),
        "契约桥应产出 POST /apple_login，实际：{:?}",
        contracts.iter().map(|n| n.name.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn synthesize_merges_table_identity_idempotently() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let tables = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::TABLE.to_string())),
            name_contains: Some("user".into()),
            limit: Some(100),
            offset: Some(0),
        })
        .expect("查询可读");
    let same: Vec<_> = tables
        .iter()
        .filter(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some("user"))
        .collect();
    assert_eq!(
        same.len(),
        1,
        "同一 identity 只能有一个 Table 节点（幂等合并），实际 {}",
        same.len()
    );

    // 多来源汇聚：既有 Db::name 调用处，也有 Model 约定推导
    let props = &same[0].properties;
    let sources = props
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert!(
        sources >= 1,
        "Table 节点应记录来源规则，实际 properties={props}"
    );
}

#[test]
fn synthesize_normalizes_table_prefix_and_plural() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let all = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::TABLE.to_string())),
            name_contains: None,
            limit: Some(500),
            offset: Some(0),
        })
        .expect("查询可读");
    let values: Vec<String> = all
        .iter()
        .filter_map(|n| n.identity.as_ref().map(|i| i.value.clone()))
        .collect();
    assert!(!values.is_empty(), "应产出 Table 节点");
    for v in &values {
        assert!(
            !v.starts_with("eb_"),
            "identity 应去掉 eb_ 前缀，实际存在：{v}"
        );
    }
}

#[test]
fn synthesize_creates_event_mediator_nodes() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let events = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::EVENT.to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("查询可读");
    assert!(!events.is_empty(), "应从 app/event.php 合成事件中介节点");
    // 事件节点的种类直接是 `Event`（不再笼统叫 `ExternalSystem`）；`category` 仅等于 kind。
    assert!(
        events.iter().all(|n| n.kind.as_str() == "Event"),
        "事件中介节点的种类应为 `Event`（子类型已提升为种类）"
    );
    assert!(
        events
            .iter()
            .all(|n| n.properties.get("category").and_then(|v| v.as_str()) == Some("Event")),
        "事件节点的 category 应等于其 kind（Event），不再有 ExternalSystem 伞"
    );
}

// ---------------------------------------------------------------- P6 AnnotatePost

#[test]
fn annotate_post_tags_pii_on_tables_with_phone_columns() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    // store_order 通过 user_phone 变体列名识别出来 —— 只靠 "phone" 硬匹配会漏
    for table in ["user", "store_order"] {
        let nodes = b
            .store
            .query_nodes(&NodeFilter {
                project_id: b.project.id,
                kind: Some(NodeKind(NodeKind::TABLE.to_string())),
                name_contains: Some(table.to_string()),
                limit: Some(20),
                offset: Some(0),
            })
            .expect("查询可读");
        let hit = nodes
            .iter()
            .find(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some(table));
        let Some(hit) = hit else { continue };
        let anns = b.store.annotations_of(hit.id).expect("标注可读");
        assert!(
            anns.iter().any(|a| a.kind == "pii" && a.subkind.as_deref() == Some("phone")),
            "{table} 表应被打上 pii.phone，实际标注：{:?}",
            anns.iter().map(|a| format!("{}:{:?}", a.kind, a.subkind)).collect::<Vec<_>>()
        );
    }
}

#[test]
fn annotate_post_marks_login_entrypoints() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let contracts = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: Some("login".into()),
            limit: Some(50),
            offset: Some(0),
        })
        .expect("查询可读");
    assert!(!contracts.is_empty(), "应存在含 login 的契约");
    let tagged = contracts.iter().any(|c| {
        b.store
            .annotations_of(c.id)
            .unwrap_or_default()
            .iter()
            .any(|a| a.kind == "entrypoint.login")
    });
    assert!(tagged, "含 login 的端点应被标记为 entrypoint.login");
}

// ---------------------------------------------------------------- P7 Resolve

#[test]
fn resolve_links_routes_to_controllers() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let contracts = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: None,
            limit: Some(400),
            offset: Some(0),
        })
        .expect("查询可读");
    let mut linked = 0;
    for c in contracts.iter().take(400) {
        let edges = b
            .store
            .edges_of(c.id, em_domain::port::EdgeDirection::Outgoing)
            .unwrap_or_default();
        if edges.iter().any(|e| e.kind.as_str() == "HandledBy") {
            linked += 1;
        }
    }
    assert!(
        linked > 50,
        "路由 handler 应大量解析成功（否则整张路由链是断的），实际 {linked}"
    );

    let unresolved = b
        .store
        .list_diagnostics(b.project.id, 500)
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.code == "UnresolvedLink")
        .count();
    assert!(
        unresolved < 100,
        "未解析链接应被控制在很小的数量，实际 {unresolved}"
    );
}

#[test]
fn fkb_resolves_apple_login_chain_to_semantics() {
    // 端到端验证「FKB 语义解析」把整条链路连起来：
    //   ① 类型化参数 → 实例方法调用（`$services->appAuth()`）
    //   ② 构造器注入的属性类型（`$this->dao`）
    //   ③ `Dao::setModel() → Model → Table`（`WechatUserDao → WechatUser → wechat_user`）
    // 断言：从 apple_login 正向能走到 Cache / Table(wechat_user) / ConfigKey。
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let contracts = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: Some("apple_login".to_string()),
            limit: Some(5),
            offset: Some(0),
        })
        .expect("查询可读");
    let Some(center) = contracts.first().cloned() else {
        eprintln!("未找到 apple_login 契约，跳过");
        return;
    };

    use em_domain::port::EdgeDirection;
    let chain = [
        "HandledBy",
        "Calls",
        "HasCallSite",
        "ReadsCache",
        "ReadsConfig",
        "ReadsDb",
        "WritesDb",
        "MapsTo",
        "Triggers",
        "PublishesTo",
        "CallsHttp",
        "ResolvesTo",
    ];

    // 遍历规则与视图一致：语义节点是**终点**；方法节点额外跳到"声明类"，
    // 让类级语义边（Dao→Model、Model→Table）浮现。
    let mut seen = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    seen.insert(center.id.get());
    queue.push_back(center.id);
    let mut semantic: Vec<(String, String)> = Vec::new();
    let mut sem_ids = std::collections::HashSet::new();
    while let Some(id) = queue.pop_front() {
        let Some(node) = b.store.get_node(id).ok().flatten() else {
            continue;
        };
        let is_semantic = is_semantic_node(&node);
        if id != center.id && is_semantic {
            continue; // 语义节点不再向外穿透
        }
        if node.kind.as_str() == NodeKind::METHOD {
            for e in b
                .store
                .edges_of(id, EdgeDirection::Incoming)
                .unwrap_or_default()
            {
                if e.kind.as_str() == "Declares" && seen.insert(e.from_id.get()) {
                    queue.push_back(e.from_id);
                }
            }
        }
        for e in b
            .store
            .edges_of(id, EdgeDirection::Outgoing)
            .unwrap_or_default()
        {
            if !chain.contains(&e.kind.as_str()) {
                continue;
            }
            if let Ok(Some(n)) = b.store.get_node(e.to_id) {
                if is_semantic_node(&n) && sem_ids.insert(n.id.get()) {
                    semantic.push((n.kind.to_string(), n.name.to_string()));
                }
            }
            if seen.insert(e.to_id.get()) {
                queue.push_back(e.to_id);
            }
        }
    }
    eprintln!("apple_login 的语义终点：{semantic:?}");

    assert!(
        semantic
            .iter()
            .any(|(k, _)| matches!(k.as_str(), "Cache" | "Event" | "Queue")),
        "应到达外部系统（其种类为 Cache / Event / Queue），实际 {semantic:?}"
    );
    assert!(
        semantic.iter().any(|(k, n)| k == "Cache" && n == "Cache"),
        "应到达种类为 `Cache` 的节点（CacheService → think\\facade\\Cache），实际 {semantic:?}"
    );
    assert!(
        semantic
            .iter()
            .filter(|(k, _)| k == NodeKind::CONFIG_KEY)
            .count()
            >= 1,
        "应到达至少一个 ConfigKey（sys_config 等），实际 {semantic:?}"
    );
    assert!(
        semantic
            .iter()
            .any(|(k, n)| k == NodeKind::TABLE && n == "wechat_user"),
        "应到达业务表 wechat_user（链路 WechatUserDao --ResolvesTo--> WechatUser \
         --MapsTo--> wechat_user），实际 {semantic:?}"
    );

    // ① 类型化参数：handler 应有一条 Calls 边指向 `WechatServices::appAuth`。
    let handler = b
        .store
        .edges_of(center.id, EdgeDirection::Outgoing)
        .unwrap_or_default()
        .into_iter()
        .find(|e| e.kind.as_str() == "HandledBy")
        .map(|e| e.to_id)
        .expect("apple_login 应有 HandledBy handler");
    let to_app_auth = b
        .store
        .edges_of(handler, EdgeDirection::Outgoing)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.kind.as_str() == "Calls")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .any(|n| n.fqn.as_deref().unwrap_or("").contains("WechatServices::appAuth"));
    assert!(
        to_app_auth,
        "handler 应调用 WechatServices::appAuth（类型化参数 `WechatServices $services` 解析）"
    );

    // ③ Dao → Model：`WechatUserDao::setModel()` 返回 `WechatUser::class` → ResolvesTo 边。
    let daos = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: Some("WechatUserDao".to_string()),
            limit: Some(5),
            offset: Some(0),
        })
        .expect("查询可读");
    if let Some(dao) = daos.iter().find(|n| n.name == "WechatUserDao") {
        let to_model = b
            .store
            .edges_of(dao.id, EdgeDirection::Outgoing)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.kind.as_str() == "ResolvesTo")
            .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
            .any(|n| n.name == "WechatUser");
        assert!(
            to_model,
            "WechatUserDao 应有一条 ResolvesTo 边指向 WechatUser（setModel 返回 User::class）"
        );
    } else {
        eprintln!("未找到 WechatUserDao 类，跳过 Dao→Model 断言");
    }
}

/// 专门验证 `samples/CRMEB-master/crmeb/app/api/route/v1.php` 里的这条路由：
///
/// ```php
/// Route::post('apple_login', 'v1.LoginController/appleLogin')->name('appleLogin');
/// ```
///
/// 链路应为：
/// `POST /apple_login`(HttpContract)
///   ─HandledBy→ `LoginController::appleLogin`
///   ─Calls→ `WechatServices::appAuth`            （类型化参数 `WechatServices $services`）
///   ─Calls→ `CacheService::get` ─ReadsCache→ `Cache`（种类 `Cache`）
///   ─Calls→ `WechatUserDao` ─ResolvesTo→ `WechatUser` ─MapsTo→ `wechat_user`(Table)
/// 并读到 `ConfigKey`（sys_config）。
#[test]
fn apple_login_route_chain_from_v1_php() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    use em_domain::port::EdgeDirection;

    let find_nodes = |kind: &str, name_contains: &str| -> Vec<em_domain::model::Node> {
        b.store
            .query_nodes(&NodeFilter {
                project_id: b.project.id,
                kind: Some(NodeKind(kind.to_string())),
                name_contains: Some(name_contains.to_string()),
                limit: Some(50),
                offset: Some(0),
            })
            .expect("查询可读")
    };
    let node_of = |id: em_domain::model::NodeId| -> Option<em_domain::model::Node> {
        b.store.get_node(id).ok().flatten()
    };
    let out_edges = |id: em_domain::model::NodeId| -> Vec<em_domain::model::Edge> {
        b.store
            .edges_of(id, EdgeDirection::Outgoing)
            .unwrap_or_default()
    };

    // ---- 1) 契约节点：必须来自 route/v1.php 的第 31 行 ----
    let contracts = find_nodes(NodeKind::HTTP_CONTRACT, "apple_login");
    let contract = contracts
        .iter()
        .find(|n| n.name == "POST /apple_login")
        .expect("应存在 `POST /apple_login` 契约");
    let props = &contract.properties;
    assert_eq!(
        props.get("handler").and_then(|v| v.as_str()),
        Some("v1.LoginController/appleLogin"),
        "handler 字段应保留路由文件里的原样写法"
    );
    assert_eq!(
        props.get("raw_path").and_then(|v| v.as_str()),
        Some("apple_login"),
        "raw_path 应为路由里的 'apple_login'"
    );
    let loc = props
        .get("locations")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .expect("契约应带来源位置");
    let loc_file = loc.get("file").and_then(|v| v.as_str()).unwrap_or("");
    let loc_line = loc.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(
        loc_file.ends_with("app/api/route/v1.php"),
        "契约应来自 app/api/route/v1.php，实际 {loc_file}"
    );
    assert_eq!(loc_line, 31, "契约应定位到 v1.php 第 31 行的 Route::post");

    // ---- 2) HandledBy → LoginController::appleLogin ----
    let handler = out_edges(contract.id)
        .into_iter()
        .find(|e| e.kind.as_str() == "HandledBy")
        .map(|e| e.to_id)
        .expect("路由应由 handler 处理（HandledBy）");
    let handler_node = node_of(handler).expect("handler 节点存在");
    assert_eq!(
        handler_node.fqn.as_deref(),
        Some("app\\api\\controller\\v1\\LoginController::appleLogin"),
        "应解析到 v1\\LoginController::appleLogin"
    );

    // ---- 3) 类型化参数：handler ─Calls→ WechatServices::appAuth ----
    let calls_app_auth = out_edges(handler)
        .into_iter()
        .filter(|e| e.kind.as_str() == "Calls")
        .filter_map(|e| node_of(e.to_id))
        .any(|n| {
            n.fqn
                .as_deref()
                .unwrap_or("")
                .contains("WechatServices::appAuth")
        });
    assert!(
        calls_app_auth,
        "appleLogin 应调用 WechatServices::appAuth（参数 `WechatServices $services` 的类型解析）"
    );

    // ---- 4) 正向链路到达语义节点（语义节点为终点）----
    let chain = [
        "HandledBy",
        "Calls",
        "HasCallSite",
        "ReadsCache",
        "ReadsConfig",
        "ReadsDb",
        "WritesDb",
        "MapsTo",
        "Triggers",
        "PublishesTo",
        "CallsHttp",
        "ResolvesTo",
    ];
    let mut seen = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    seen.insert(contract.id.get());
    queue.push_back(contract.id);
    let mut semantic: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    while let Some(id) = queue.pop_front() {
        let Some(node) = node_of(id) else { continue };
        if id != contract.id && NodeKind(node.kind.to_string()).is_semantic() {
            continue;
        }
        if node.kind.as_str() == NodeKind::METHOD {
            for e in b
                .store
                .edges_of(id, EdgeDirection::Incoming)
                .unwrap_or_default()
            {
                if e.kind.as_str() == "Declares" && seen.insert(e.from_id.get()) {
                    queue.push_back(e.from_id);
                }
            }
        }
        for e in out_edges(id) {
            if !chain.contains(&e.kind.as_str()) {
                continue;
            }
            if let Some(n) = node_of(e.to_id) {
                if is_semantic_node(&n) {
                    semantic.insert((n.kind.to_string(), n.name.to_string()));
                }
            }
            if seen.insert(e.to_id.get()) {
                queue.push_back(e.to_id);
            }
        }
    }
    eprintln!("POST /apple_login 链路语义终点：{semantic:?}");

    let hit = |kind: &str, name: &str| semantic.iter().any(|(k, n)| k == kind && n == name);
    assert!(
        hit("Cache", "Cache"),
        "链路应到达种类为 `Cache` 的节点（CacheService::get → think\\facade\\Cache），实际 {semantic:?}"
    );
    assert!(
        hit(NodeKind::TABLE, "wechat_user"),
        "链路应到达 Table(wechat_user)，实际 {semantic:?}"
    );
    assert!(
        semantic.iter().any(|(k, _)| k == NodeKind::CONFIG_KEY),
        "链路应到达 ConfigKey（sys_config），实际 {semantic:?}"
    );

    // ---- 5) 关键语义边：Dao → Model → Table ----
    let dao = find_nodes(NodeKind::CLASS, "WechatUserDao")
        .into_iter()
        .find(|n| n.name == "WechatUserDao")
        .expect("应存在 WechatUserDao 类");
    let model = out_edges(dao.id)
        .into_iter()
        .filter(|e| e.kind.as_str() == "ResolvesTo")
        .filter_map(|e| node_of(e.to_id))
        .find(|n| n.name == "WechatUser")
        .expect("WechatUserDao --ResolvesTo--> WechatUser（setModel 返回 User::class）");
    out_edges(model.id)
        .into_iter()
        .filter(|e| e.kind.as_str() == "MapsTo")
        .filter_map(|e| node_of(e.to_id))
        .find(|n| n.name == "wechat_user" && n.kind.as_str() == NodeKind::TABLE)
        .expect("WechatUser --MapsTo--> wechat_user（Table）");

    // ---- 6) 关键语义边：CacheService 读到 Cache（FKB facade 规则）----
    let cache_read = find_nodes(NodeKind::CLASS, "CacheService")
        .into_iter()
        .flat_map(|svc| {
            out_edges(svc.id)
                .into_iter()
                .filter(|e| e.kind.as_str() == "Declares")
                .map(|e| e.to_id)
                .collect::<Vec<_>>()
        })
        .any(|m| {
            out_edges(m).into_iter().any(|e| {
                e.kind.as_str() == "ReadsCache"
                    && node_of(e.to_id)
                        .map(|n| n.kind.as_str() == "Cache")
                        .unwrap_or(false)
            })
        });
    assert!(
        cache_read,
        "CacheService 的某个方法应有一条 ReadsCache 边指向种类为 `Cache` 的节点"
    );
}

#[test]
fn calls_edge_records_call_site_node() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let node_of = |id: em_domain::model::NodeId| -> Option<em_domain::model::Node> {
        b.store.get_node(id).ok().flatten()
    };
    let out_edges = |id: em_domain::model::NodeId| -> Vec<em_domain::model::Edge> {
        b.store
            .edges_of(id, em_domain::port::EdgeDirection::Outgoing)
            .unwrap_or_default()
    };

    // 找一条带 call_site 的 Calls 边，验证"调用处"被精确记录在边上（而非靠名字猜）
    let mut found: Option<em_domain::model::Edge> = None;
    for n in b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Method".to_string())),
            name_contains: None,
            limit: Some(2000),
            offset: Some(0),
        })
        .expect("方法查询可读")
    {
        for e in out_edges(n.id) {
            if e.kind.as_str() == "Calls" && e.properties.get("call_site").is_some() {
                found = Some(e);
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let e = found.expect("应至少存在一条记录 call_site 的 Calls 边");
    let cs_id = e
        .properties
        .get("call_site")
        .and_then(|v| v.as_i64())
        .expect("call_site 应为整数节点 id");
    let cs = node_of(em_domain::model::NodeId(cs_id)).expect("call_site 指向的节点应存在");
    assert_eq!(
        cs.kind.as_str(),
        "CallSite",
        "call_site 应精确指向 CallSite 节点（100% 精确，无需启发式）"
    );
    assert!(
        cs.file_id.is_some(),
        "CallSite 应带有源文件位置（文件 + 行）"
    );
}

#[test]
fn resolve_creates_event_trigger_edges() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let events = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::EVENT.to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("查询可读");
    let triggers = events.iter().any(|e| {
        b.store
            .edges_of(e.id, em_domain::port::EdgeDirection::Incoming)
            .unwrap_or_default()
            .iter()
            .any(|x| x.kind.as_str() == "Triggers")
    });
    assert!(triggers, "event('x') 应解析出 Triggers 边");
}

// ---------------------------------------------------------------- 诚实性

#[test]
fn synthesized_nodes_keep_multiple_source_locations() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let tables = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::TABLE.to_string())),
            name_contains: Some("user".into()),
            limit: Some(50),
            offset: Some(0),
        })
        .expect("查询可读");
    let hit = tables
        .iter()
        .find(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some("user"))
        .expect("user 表应存在");
    let locations = hit
        .properties
        .get("locations")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert!(
        locations >= 2,
        "合成节点必须保留多处共现位置（不能编造单一位置），实际 {locations}"
    );
}

#[test]
fn every_phase_reports_and_persists() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    for phase in [
        "Ingest",
        "CfAst",
        "Prepare",
        "AnnotatePre",
        "Synthesize",
        "AnnotatePost",
        "Resolve",
    ] {
        assert!(
            common::phase_report(&b, phase).is_some(),
            "阶段 {phase} 应有运行报告"
        );
    }
    let cf = common::phase_report(&b, "CfAst").expect("CfAst 报告");
    assert!(cf.nodes_created > 1000, "CfAst 应产出大量节点");
}

#[test]
fn project_and_sub_project_ids_are_consistent() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    assert_eq!(b.project.id, ProjectId(1));
    let subs = b.store.list_sub_projects(b.project.id).expect("可读");
    assert!(subs.iter().all(|s| s.project_id == b.project.id));
    assert!(subs.iter().any(|s| s.id == SubProjectId(1)));
}

// ---------------------------------------------------------------- v1.php 专项

/// v1.php 的路由是否进了图：文件被扫描 + 其贡献的 HttpContract 节点存在。
#[test]
fn v1_php_routes_are_in_graph() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };

    // 1) v1.php 是否被扫描进图
    let files = b.store.list_files(b.project.id, None).expect("可读");
    assert!(
        files
            .iter()
            .any(|f| f.path.ends_with("crmeb/app/api/route/v1.php")),
        "v1.php 应被扫描进图"
    );

    // 2) 哪些 HttpContract 节点来自 v1.php
    let contracts = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("可读");

    let mut from_v1: Vec<String> = contracts
        .iter()
        .filter(|c| {
            c.properties
                .get("locations")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|l| {
                        l.get("file")
                            .and_then(|f| f.as_str())
                            .unwrap_or("")
                            .ends_with("route/v1.php")
                    })
                })
                .unwrap_or(false)
        })
        .filter_map(|c| c.identity.as_ref().map(|i| i.value.clone()))
        .collect();
    from_v1.sort();
    eprintln!("v1.php 贡献的 HttpContract 数量 = {}", from_v1.len());
    for id in from_v1.iter().take(30) {
        eprintln!("  {id}");
    }
    assert!(!from_v1.is_empty(), "v1.php 应至少贡献若干路由契约");
    for must in [
        "POST /apple_login",
        "ANY /wechat/serve",
        "GET /admin/order/statistics",
        "POST /login",
        "GET /pay/config",
    ] {
        assert!(from_v1.iter().any(|x| x == must), "缺少预期路由 {must}");
    }
}

/// 直接看 v1.php 的解析产物（原始调用点），不经过整张图。
/// 队列语义节点应被**框架级** FKB 探测出来（无需任何项目级 FKB）。
///
/// CRMEB 经 `QueueTrait::dispatch` / `crmeb\utils\Queue` 包装使用 `think\facade\Queue`，
/// 框架规则 `tp6-queue-topic` 通过 `arg:0`（Job 类）+ `owner_class` 兜底（产生调用的类）
/// 把队列 topic 合成出来；再由传播把 `PublishesTo` 边上溯到各 Service。
#[test]
fn synthesize_detects_queues_from_framework_fkb() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let queues = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::QUEUE.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("查询可读");
    eprintln!("CRMEB 探测到的 Queue 节点数 = {}", queues.len());
    assert!(
        !queues.is_empty(),
        "框架级 FKB 应探测出队列节点（CRMEB 经门面/包装/trait 使用 think\\facade\\Queue）"
    );
    // 每个 Queue 节点都应有至少一条 PublishesTo 入边（谁投递）。
    let with_publisher = queues
        .iter()
        .filter(|q| {
            b.store
                .edges_of(q.id, em_domain::port::EdgeDirection::Incoming)
                .unwrap_or_default()
                .iter()
                .any(|e| e.kind.as_str() == "PublishesTo")
        })
        .count();
    assert!(
        with_publisher > 0,
        "至少部分 Queue 节点应有 PublishesTo 入边，实际 {with_publisher}/{}",
        queues.len()
    );
    // 队列还应有消费方（HandledBy）：`arg:0` 解析不出时退回 `receiver_class`（被投递的
    // Job 类），其 `doJob`/`handle` 即为消费入口，使「队列视角」与事件一样看得到消费者。
    let with_consumer = queues
        .iter()
        .filter(|q| {
            b.store
                .edges_of(q.id, em_domain::port::EdgeDirection::Outgoing)
                .unwrap_or_default()
                .iter()
                .any(|e| e.kind.as_str() == "HandledBy")
        })
        .count();
    assert!(
        with_consumer > 0,
        "至少部分 Queue 节点应有 HandledBy 出边（消费方），实际 {with_consumer}/{}",
        queues.len()
    );
}

#[test]
fn synthesize_detects_schedules_from_project_fkb() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let schedules = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::SCHEDULE.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("查询可读");
    eprintln!("CRMEB 探测到的 Schedule 节点数 = {}", schedules.len());
    assert!(
        !schedules.is_empty(),
        "项目级 FKB（crmeb.yaml）应把 crontab/* 路由合成为 Schedule 节点"
    );
    // 每个 Schedule 节点都应 HandledBy 到对应 CrontabController 方法。
    let with_handler = schedules
        .iter()
        .filter(|s| {
            b.store
                .edges_of(s.id, em_domain::port::EdgeDirection::Outgoing)
                .unwrap_or_default()
                .iter()
                .any(|e| e.kind.as_str() == "HandledBy")
        })
        .count();
    assert!(
        with_handler > 0,
        "至少部分 Schedule 节点应有 HandledBy 出边（处理方），实际 {with_handler}/{}",
        schedules.len()
    );
}

#[test]
fn v1_php_parse_result() {
    let Some(root) = common::sample_root() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let path = root.join("crmeb/app/api/route/v1.php");
    let src = std::fs::read_to_string(&path).expect("读 v1.php");
    let reg = em_adapter_parser::DefaultParserRegistry::new();
    let parser = reg
        .parser_for(&em_domain::model::Language::new(
            em_domain::model::Language::PHP,
        ))
        .expect("php 解析器");
    let facts = parser.parse(path.to_str().unwrap(), &src).expect("解析");

    eprintln!(
        "declarations={} imports={} call_sites={} inheritances={} config_entries={}",
        facts.declarations.len(),
        facts.imports.len(),
        facts.call_sites.len(),
        facts.inheritances.len(),
        facts.config_entries.len()
    );

    let route_calls: Vec<_> = facts
        .call_sites
        .iter()
        .filter(|c| c.receiver.as_deref() == Some("Route"))
        .collect();
    eprintln!("Route:: 调用点数量 = {}", route_calls.len());
    for c in route_calls.iter().take(30) {
        let args: Vec<String> = c
            .args
            .iter()
            .map(|a| match a {
                em_domain::model::FactValue::String(s) => s.clone(),
                _ => "?".to_string(),
            })
            .collect();
        eprintln!(
            "  {}  args={:?}  @{}:{}",
            c.callee_text, args, c.owner_fqn, c.span.start_line
        );
    }
    assert!(!route_calls.is_empty(), "v1.php 应解析出 Route 调用点");
}

/// 门面短名必须按**文件自己的 `use`** 解析，不能退回全局短名索引去猜。
///
/// `crmeb/crmeb/services/CacheService.php` 里写的是 `use think\facade\Cache;`，
/// 而项目里恰好有一个 Model 叫 `app\model\other\Cache`（`protected $name = 'cache'`）。
/// 若用全局短名索引解析，`Cache::tag($tag)->remember(...)` 就会被当成那个 Model，
/// 于是每一次缓存调用都凭空多出一条 `Calls` 边，并把
/// `Model --MapsTo--> Table(cache)` 这条**类级**语义边沿调用链拖到每个路由上。
///
/// 这是 PHP 的名字解析规则（按文件），与框架无关；`Request` / `Route` / `Response`
/// 等同样有同名项目类，受同一条规则保护。
#[test]
fn facade_short_name_resolves_per_file_import() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    // `name_contains` 匹配的是节点的 `name`，所以先按短名粗筛、再按 fqn 精确定位。
    let by_fqn = |fqn: &str| -> Option<em_domain::model::Node> {
        let short = fqn.rsplit(['\\', ':']).next().unwrap_or(fqn);
        b.store
            .query_nodes(&NodeFilter {
                project_id: b.project.id,
                kind: None,
                name_contains: Some(short.to_string()),
                limit: Some(2000),
                offset: Some(0),
            })
            .unwrap_or_default()
            .into_iter()
            .find(|n| n.fqn.as_deref() == Some(fqn))
    };
    let cache_model = by_fqn("app\\model\\other\\Cache");
    let Some(model) = cache_model else {
        eprintln!("图里没有 app\\model\\other\\Cache，跳过");
        return;
    };
    let remember = by_fqn("crmeb\\services\\CacheService::remember");
    let Some(remember) = remember else {
        eprintln!("图里没有 CacheService::remember，跳过");
        return;
    };

    // `Cache::...` 在这个文件里是 `think\facade\Cache`（图外，vendor 已排除），
    // 因此**不该**解析到项目里的同名 Model。
    let bogus = b
        .store
        .edges_of(remember.id, EdgeDirection::Outgoing)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.kind.as_str() == "Calls" && e.to_id == model.id)
        .count();
    assert_eq!(
        bogus, 0,
        "CacheService::remember 不应有指向 app\\model\\other\\Cache 的 Calls 边：\
         `use think\\facade\\Cache;` 是图外的框架类，短名解析不能退回全局同名类"
    );

    // 该 Model 自身的类级语义边必须完好（修的是"猜错调用"，不是"删掉这个类"）。
    let maps_to = b
        .store
        .edges_of(model.id, EdgeDirection::Outgoing)
        .unwrap_or_default()
        .into_iter()
        .any(|e| e.kind.as_str() == "MapsTo");
    assert!(maps_to, "app\\model\\other\\Cache 仍应保留 Model --MapsTo--> Table 的类级边");
}


