//! 以 `分析样本/CRMEB-master` 为材料的建图集成测试。
//!
//! 这些用例验证的是**端到端结论**，而不是某个函数的返回值：
//! 子工程是否被正确识别、依赖目录是否被排除、`AppRoot` 是否按 FKB 解析、
//! 语义节点是否幂等合并、路由链是否真的连上了控制器。

mod common;

use em_domain::model::{NodeKind, ProjectId, SubProjectId};
use em_domain::port::{DiagnosticSink, GraphQuery, NodeFilter, ProjectReader, SymbolTableReader};

use serde_json::Value;

fn built() -> Option<std::sync::Arc<common::Built>> {
    common::graph()
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
            kind: Some(NodeKind(NodeKind::EXTERNAL_SYSTEM.to_string())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("查询可读");
    assert!(!events.is_empty(), "应从 app/event.php 合成事件中介节点");
    assert!(
        events
            .iter()
            .any(|n| n.properties.get("subtype").and_then(|v| v.as_str()) == Some("Event")),
        "事件节点应带 subtype=Event"
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
fn resolve_creates_event_trigger_edges() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let events = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::EXTERNAL_SYSTEM.to_string())),
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
