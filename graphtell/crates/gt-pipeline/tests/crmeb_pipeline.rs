//! A graph-building integration test using `samples/thinkphp-projects/CRMEB-master` as material.
//!
//! These cases verify **end-to-end conclusions**, not the return value of some function:
//! whether sub-projects are recognised correctly, dependency directories are excluded, `AppRoot` is resolved by FKB,
//! semantic nodes merge idempotently, and the route chain really connects to the controller.

mod common;

use gt_domain::model::{NodeKind, ProjectId, SubProjectId};
use gt_domain::port::{
    DiagnosticSink, EdgeDirection, GraphQuery, NodeFilter, ParserRegistry, ProjectReader,
    SymbolTableReader,
};

use serde_json::Value;

fn built() -> Option<std::sync::Arc<common::Built>> {
    common::graph()
}

/// Semantic-node judgement (consistent with the view): the first-class semantic kind (`SYNTHESIZED` in `kinds.rs`).
fn is_semantic_node(n: &gt_domain::model::Node) -> bool {
    NodeKind(n.kind.to_string()).is_semantic()
}

// ---------------------------------------------------------------- P0 Ingest

#[test]
fn ingest_detects_three_sub_projects() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let subs = b.store.list_sub_projects(b.project.id).expect("the sub-projects must be readable");
    let names: Vec<&str> = subs.iter().map(|s| s.name.as_str()).collect();

    // CRMEB is a multi-stack repo: a ThinkPHP backend + two frontend projects
    assert!(
        names.iter().any(|n| n.contains("crmeb")),
        "a backend sub-project must be recognised, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("uni-app")),
        "the Uni-app frontend sub-project must be recognised, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("admin")),
        "the admin frontend sub-project must be recognised, got: {names:?}"
    );

    let backend = subs
        .iter()
        .find(|s| s.detected_by == "composer.json")
        .expect("the backend must be recognised from composer.json");
    assert_eq!(backend.language.as_str(), "php");
    assert!(
        backend.role.starts_with("backend"),
        "the backend role must start with backend, got: {}",
        backend.role
    );

    let frontend = subs
        .iter()
        .find(|s| s.detected_by == "package.json")
        .expect("the frontend must be recognised from package.json");
    assert!(
        frontend.role.starts_with("frontend"),
        "the frontend role must start with frontend, got: {}",
        frontend.role
    );
}

#[test]
fn ingest_excludes_dependency_and_asset_dirs() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let files = b.store.list_files(b.project.id, None).expect("the files must be readable");
    assert!(!files.is_empty(), "at least some source files must be scanned");

    for forbidden in ["vendor/", "node_modules/", "target/", ".git/"] {
        let leaked: Vec<&str> = files
            .iter()
            .filter(|f| f.path.contains(forbidden))
            .map(|f| f.path.as_str())
            .take(3)
            .collect();
        assert!(
            leaked.is_empty(),
            "{forbidden} is a dependency directory, so it must not enter the analysis set: {leaked:?}"
        );
    }

    // Static assets and binary artifacts must not appear either
    for ext in [".png", ".jpg", ".woff2", ".zip"] {
        let leaked: Vec<&str> = files
            .iter()
            .filter(|f| f.path.to_ascii_lowercase().ends_with(ext))
            .map(|f| f.path.as_str())
            .take(3)
            .collect();
        assert!(leaked.is_empty(), "the static asset {ext} must not enter the analysis set: {leaked:?}");
    }

    // But business source must be present
    assert!(
        files.iter().any(|f| f.path.ends_with("crmeb/app/event.php")),
        "the business file crmeb/app/event.php must be in the analysis set"
    );
}

/// What must not be scanned is **framework knowledge resolved against this project**, not a hard-coded
/// directory name: ThinkPHP writes its cache under `runtime/`, and P0 finds it (and records how) before
/// a single file is parsed.
#[test]
fn ingest_resolves_excludes_from_framework_knowledge() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let subs = b.store.list_sub_projects(b.project.id).expect("the sub-projects must be readable");
    let backend = subs
        .iter()
        .find(|s| s.detected_by == "composer.json")
        .expect("the backend sub-project");
    let excludes = backend
        .facts
        .get("excludes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !excludes.is_empty(),
        "the ThinkPHP knowledge base must resolve exclude rules, got facts={}",
        backend.facts
    );
    let globs: Vec<&str> = excludes
        .iter()
        .filter_map(|e| e.get("glob").and_then(|g| g.as_str()))
        .collect();
    assert!(
        globs.iter().any(|g| g.contains("runtime")),
        "the runtime cache directory must be resolved, got: {globs:?}"
    );
    assert!(
        globs.iter().any(|g| g.contains("public")),
        "the public entry directory must be resolved, got: {globs:?}"
    );

    let files = b.store.list_files(b.project.id, None).expect("the files must be readable");
    for forbidden in ["crmeb/runtime/", "crmeb/public/"] {
        let leaked: Vec<&str> = files
            .iter()
            .filter(|f| f.path.contains(forbidden))
            .map(|f| f.path.as_str())
            .take(3)
            .collect();
        assert!(leaked.is_empty(), "{forbidden} must not enter the analysis set: {leaked:?}");
    }
}

// ---------------------------------------------------------------- P2 CfAst

#[test]
fn cf_ast_builds_syntax_nodes_and_call_sites() {
    // Runs for real when the CRMEB sample is present (GRAPHTELL_SAMPLE_DIR); skips otherwise.
    // CfAst's always-on structural coverage now lives in `cf_ast_structure.rs`, so a missing
    // sample here is a plain skip, not a fake-green gap.
    let Some(b) = built() else {
        eprintln!("skip: CRMEB sample not found (point GRAPHTELL_SAMPLE_DIR at it)");
        return;
    };
    let stats = b.store.stats(b.project.id).expect("the statistics must be readable");
    for kind in [
        NodeKind::CLASS,
        NodeKind::METHOD,
        NodeKind::PROPERTY,
        NodeKind::NAMESPACE,
        NodeKind::CALL_SITE,
    ] {
        assert!(
            stats.by_kind.get(kind).copied().unwrap_or(0) > 0,
            "CfAst must produce {kind} nodes, got statistics: {:?}",
            stats.by_kind
        );
    }
    assert!(
        stats.by_kind.get(NodeKind::CALL_SITE).copied().unwrap_or(0) > 1000,
        "a call site must be refined into a CallSite node"
    );
}

#[test]
fn cf_ast_resolves_php_namespace_and_fqn() {
    // Runs for real when the CRMEB sample is present (GRAPHTELL_SAMPLE_DIR); skips otherwise.
    // CfAst's always-on structural coverage now lives in `cf_ast_structure.rs`, so a missing
    // sample here is a plain skip, not a fake-green gap.
    let Some(b) = built() else {
        eprintln!("skip: CRMEB sample not found (point GRAPHTELL_SAMPLE_DIR at it)");
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
        .expect("the query must be readable");
    let hit = nodes
        .iter()
        .find(|n| n.fqn.as_deref() == Some("app\\model\\order\\StoreOrder"));
    assert!(
        hit.is_some(),
        "the FQN app\\model\\order\\StoreOrder must be derived from namespace + class, got: {:?}",
        nodes.iter().map(|n| n.fqn.clone()).take(5).collect::<Vec<_>>()
    );
}

#[test]
fn cf_ast_follows_model_inheritance_chain() {
    // Runs for real when the CRMEB sample is present (GRAPHTELL_SAMPLE_DIR); skips otherwise.
    // CfAst's always-on structural coverage now lives in `cf_ast_structure.rs`, so a missing
    // sample here is a plain skip, not a fake-green gap.
    let Some(b) = built() else {
        eprintln!("skip: CRMEB sample not found (point GRAPHTELL_SAMPLE_DIR at it)");
        return;
    };
    // StoreOrder extends BaseModel extends think\Model — the leaf is in vendor (excluded),
    // but the inheritance relation itself must be recorded, otherwise table inference loses everything
    let store_order = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: Some("StoreOrder".into()),
            limit: Some(50),
            offset: Some(0),
        })
        .expect("the query must be readable")
        .into_iter()
        .find(|n| n.fqn.as_deref() == Some("app\\model\\order\\StoreOrder"))
        .expect("the StoreOrder class must exist");

    let edges = b
        .store
        .edges_of(store_order.id, gt_domain::port::EdgeDirection::Outgoing)
        .expect("the edges must be readable");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Extends" || e.kind.as_str() == "UsesTrait"),
        "inheritance / trait usage must be recorded, got out-edges: {:?}",
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
    let subs = b.store.list_sub_projects(b.project.id).expect("the sub-projects must be readable");
    let backend = subs
        .iter()
        .find(|s| s.language.as_str() == "php")
        .expect("a PHP sub-project must exist");
    let facts: Value = serde_json::from_value(backend.facts.clone()).unwrap_or(Value::Null);
    let app_root = facts.get("app_root").expect("the app_root fact must be resolved");

    assert_eq!(
        app_root.get("value").and_then(|v| v.as_str()),
        Some("app"),
        "AppRoot must come from autoload.psr-4 in composer.json"
    );
    assert_eq!(
        app_root.get("fallback_used").and_then(|v| v.as_bool()),
        Some(false),
        "since psr-4 resolution succeeded the fallback directory must not be used"
    );
    let source = app_root
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        source.contains("autoload.psr-4"),
        "the resolution evidence must be recorded in source, got: {source}"
    );
    assert!(
        backend.frameworks.contains(&"thinkphp".to_string()),
        "thinkphp must be recognised, got: {:?}",
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
        let rows = b.store.list_symbols(pid, table).expect("the symbol table must be readable");
        assert!(!rows.is_empty(), "the authoritative table {table} must not be empty");
    }
    // schema comes from the install SQL + table names in code
    let schema = b.store.list_symbols(pid, "schema").expect("the schema must be readable");
    assert!(schema.len() > 50, "the schema must load a large number of tables, got {}", schema.len());
    let user = b
        .store
        .get_symbol(pid, "schema", "user")
        .expect("it must be readable")
        .expect("the user table must be in the schema");
    let cols = user.get("columns").and_then(|c| c.as_array()).map(|a| a.len()).unwrap_or(0);
    assert!(cols > 5, "the user table must resolve to several columns, got {cols}");
}

/// `config_keys` must collect config keys only from declared accessors — not from a blanket
/// `::get` suffix, and not leak project helpers into the framework FKB.
#[test]
fn prepare_config_keys_come_from_declared_accessors_not_route_paths() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let rows = b
        .store
        .list_symbols(b.project.id, "config_keys")
        .expect("config_keys must be readable");
    assert!(!rows.is_empty(), "config keys must be collected");

    // 1) The old blanket `suffixes: ["::get"]` matched `Route::get('api/goods/detail')` — 791 route
    //    registrations against 141 real config reads on CRMEB. No key here may look like a URL path.
    let route_like: Vec<&str> = rows
        .iter()
        .map(|r| r.key.as_str())
        .filter(|k| k.contains('/'))
        .collect();
    assert!(
        route_like.is_empty(),
        "a route path must not be taken for a config key: {:?}",
        &route_like[..route_like.len().min(5)]
    );

    // 2) CRMEB's own helper `sys_config()` belongs to the **project** FKB, not ThinkPHP; its keys must
    //    still be collected (the framework FKB no longer lists it).
    let from_db = rows
        .iter()
        .filter(|r| r.value.get("storage").and_then(|v| v.as_str()) == Some("Database"))
        .count();
    assert!(
        from_db > 0,
        "DB-stored config keys such as sys_config must be collected by the project-level loader"
    );

    // 3) Framework accessors (`env` / `Env::get`) carry `storage: Env`, distinct from file config.
    let from_env = rows
        .iter()
        .filter(|r| r.value.get("storage").and_then(|v| v.as_str()) == Some("Env"))
        .count();
    assert!(from_env > 0, "an env config key must be marked as Env storage");
}

#[test]
fn prepare_parses_sql_columns_without_being_cut_by_parentheses() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    // The closing paren in `int(11)` once made column parsing end early, leaving only the `id` column
    let user = b
        .store
        .get_symbol(b.project.id, "schema", "user")
        .expect("it must be readable")
        .expect("the user table must exist");
    let cols: Vec<String> = user
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    assert!(cols.contains(&"phone".to_string()), "the user table must contain the phone column: {cols:?}");
    assert!(cols.contains(&"uid".to_string()), "the user table must contain the uid column: {cols:?}");
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
        .expect("the query must be readable");
    let hit = contracts
        .iter()
        .find(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some("POST /apple_login"));
    assert!(
        hit.is_some(),
        "the contract bridge must produce POST /apple_login, got: {:?}",
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
        .expect("the query must be readable");
    let same: Vec<_> = tables
        .iter()
        .filter(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some("user"))
        .collect();
    assert_eq!(
        same.len(),
        1,
        "one identity must map to exactly one Table node (idempotent merge), got {}",
        same.len()
    );

    // Multi-source convergence: both the Db::name call site and the Model-convention inference
    let props = &same[0].properties;
    let sources: Vec<&str> = props
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(
        !sources.is_empty(),
        "a Table node must record its source rule, got properties={props}"
    );
    // Item ids are namespaced at load time (`<fkb-id>-<local id>`), so what lands on the graph is
    // always qualified — a bare `db-name-table` would mean the prefix was lost somewhere.
    assert!(
        sources.iter().any(|s| s.starts_with("thinkphp-")),
        "a source rule id must carry the framework namespace, got {sources:?}"
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
        .expect("the query must be readable");
    let values: Vec<String> = all
        .iter()
        .filter_map(|n| n.identity.as_ref().map(|i| i.value.clone()))
        .collect();
    assert!(!values.is_empty(), "a Table node must be produced");
    for v in &values {
        assert!(
            !v.starts_with("eb_"),
            "the identity must drop the eb_ prefix, but it still has: {v}"
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
        .expect("the query must be readable");
    assert!(!events.is_empty(), "an event-mediating node must be synthesised from app/event.php");
    // The event node's kind is directly `Event`; `category` equals kind only.
    assert!(
        events.iter().all(|n| n.kind.as_str() == "Event"),
        "the kind of an event-mediating node must be `Event` (the subtype was promoted to a kind)"
    );
    assert!(
        events
            .iter()
            .all(|n| n.properties.get("category").and_then(|v| v.as_str()) == Some("Event")),
        "the category of an event node must equal its kind (Event); the ExternalSystem umbrella is gone"
    );
}

// ---------------------------------------------------------------- P6 AnnotatePost

#[test]
fn annotate_post_tags_pii_on_tables_with_phone_columns() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    // store_order is recognised via the user_phone variant column name — a hard match on "phone" alone would miss it
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
            .expect("the query must be readable");
        let hit = nodes
            .iter()
            .find(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some(table));
        let Some(hit) = hit else { continue };
        let anns = b.store.annotations_of(hit.id).expect("the annotations must be readable");
        assert!(
            anns.iter().any(|a| a.kind == "pii" && a.subkind.as_deref() == Some("phone")),
            "table {table} must be annotated pii.phone, annotations: {:?}",
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
        .expect("the query must be readable");
    assert!(!contracts.is_empty(), "a contract containing login must exist");
    let tagged = contracts.iter().any(|c| {
        b.store
            .annotations_of(c.id)
            .unwrap_or_default()
            .iter()
            .any(|a| a.kind == "entrypoint.login")
    });
    assert!(tagged, "an endpoint containing login must be marked entrypoint.login");
}

// ---------------------------------------------------------------- P9 Taint
// P9 Taint was recently made language-agnostic: its SQL-sink vocabulary and request sources come from the
// FKB `taint:` declaration (replacing a hard-coded `language == php` gate), and the variable marker comes
// from the parser's `variable_prefixes`. On a real sample this pins the whole chain — if the FKB vocabulary
// or the prefix plumbing regresses, the engine silently emits nothing and a synthetic test may still pass
// (it injects its own markers), so the regression only shows up here.
#[test]
fn taint_flags_sql_injection_on_real_sample() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let all = b
        .store
        .annotations_of_project(b.project.id)
        .expect("the annotations must be readable");
    let kinds: std::collections::HashSet<String> = all.values().flatten().map(|a| a.kind.clone()).collect();
    for k in [
        "tainted_raw",
        "tainted_where",
        "tainted_raw_unknown",
        "tainted_where_unknown",
    ] {
        if kinds.contains(k) {
            eprintln!("CRMEB taint annotation present: {k}");
        }
    }
    assert!(
        kinds.iter().any(|k| {
            k == "tainted_raw" || k == "tainted_where" || k == "tainted_raw_unknown" || k == "tainted_where_unknown"
        }),
        "CRMEB must have at least one SQL-injection annotation (tainted_raw / tainted_where / …), annotation kinds: {:?}",
        kinds
    );
}

// ---------------------------------------------------------------- P11 SignCheck
// P11 SignCheck (signature verification) was made language-agnostic via the FKB `sign_check:` declaration
// (replacing a hard-coded `language == php` gate): which calls compute a signature, which comparison is a
// "loose" signature comparison, and which hash algorithms are weak all come from FKB. This pins the whole
// chain on a real sample — a synthetic test injects its own vocabulary and may still pass after a regression,
// so the failure only surfaces here.
#[test]
fn sign_check_flags_weak_signature_on_real_sample() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let all = b
        .store
        .annotations_of_project(b.project.id)
        .expect("the annotations must be readable");
    let kinds: std::collections::HashSet<String> =
        all.values().flatten().map(|a| a.kind.clone()).collect();
    for k in ["weak_sign_compare", "weak_sign_hash"] {
        if kinds.contains(k) {
            eprintln!("CRMEB sign_check annotation present: {k}");
        }
    }
    assert!(
        kinds.iter().any(|k| k == "weak_sign_compare" || k == "weak_sign_hash"),
        "CRMEB must have at least one signature annotation (weak_sign_compare / weak_sign_hash), annotation kinds: {:?}",
        kinds
    );
}

// ---------------------------------------------------------------- P13 Tx
// P13 Tx (transaction-boundary) was made language-agnostic via the FKB `tx_calls:` declaration (replacing a
// hard-coded `language == php` gate): the method names that open a transaction come entirely from FKB. This
// pins the chain on a real sample — a synthetic test injects its own markers and may still pass after a
// regression, so the failure only surfaces here.
#[test]
fn tx_flags_multi_write_without_tx_on_real_sample() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let all = b
        .store
        .annotations_of_project(b.project.id)
        .expect("the annotations must be readable");
    let kinds: std::collections::HashSet<String> =
        all.values().flatten().map(|a| a.kind.clone()).collect();
    assert!(
        kinds.contains("multi-write-without-tx"),
        "CRMEB must have at least one multi-write-without-tx annotation, annotation kinds: {:?}",
        kinds
    );
    let subkinds: Vec<&str> = all
        .values()
        .flatten()
        .filter(|a| a.kind == "multi-write-without-tx")
        .filter_map(|a| a.subkind.as_deref())
        .collect();
    eprintln!("CRMEB multi-write-without-tx subkinds: {subkinds:?}");
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
        .expect("the query must be readable");
    let mut linked = 0;
    for c in contracts.iter().take(400) {
        let edges = b
            .store
            .edges_of(c.id, gt_domain::port::EdgeDirection::Outgoing)
            .unwrap_or_default();
        if edges.iter().any(|e| e.kind.as_str() == "HandledBy") {
            linked += 1;
        }
    }
    assert!(
        linked > 50,
        "route handlers must resolve in large numbers (otherwise the whole route chain is broken), got {linked}"
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
        "unresolved links must stay very low, got {unresolved}"
    );
}

#[test]
fn fkb_resolves_apple_login_chain_to_semantics() {
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
        .expect("the query must be readable");
    let Some(center) = contracts.first().cloned() else {
        eprintln!("apple_login contract not found, skipping");
        return;
    };

    use gt_domain::port::EdgeDirection;
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

    // Traversal rules align with the view: semantic nodes are the **endpoints**; method nodes additionally jump to the "declaring class",
    // so class-level semantic edges (Dao->Model, Model->Table) surface.
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
            continue; // Semantic nodes do not penetrate outward
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
    eprintln!("semantic endpoint of apple_login: {semantic:?}");

    assert!(
        semantic
            .iter()
            .any(|(k, _)| matches!(k.as_str(), "Cache" | "Event" | "Queue")),
        "it must reach an external system (of kind Cache / Event / Queue), got {semantic:?}"
    );
    assert!(
        semantic.iter().any(|(k, n)| k == "Cache" && n == "Cache"),
        "it must reach a node of kind `Cache` (CacheService -> think\\facade\\Cache), got {semantic:?}"
    );
    assert!(
        semantic
            .iter()
            .filter(|(k, _)| k == NodeKind::CONFIG_KEY)
            .count()
            >= 1,
        "it must reach at least one ConfigKey (sys_config and friends), got {semantic:?}"
    );
    assert!(
        semantic
            .iter()
            .any(|(k, n)| k == NodeKind::TABLE && n == "wechat_user"),
        "应到达业务表 wechat_user（链路 WechatUserDao --ResolvesTo--> WechatUser \
         --MapsTo--> wechat_user），实际 {semantic:?}"
    );

    // ① Typed parameter: the handler should have a Calls edge to `WechatServices::appAuth`.
    let handler = b
        .store
        .edges_of(center.id, EdgeDirection::Outgoing)
        .unwrap_or_default()
        .into_iter()
        .find(|e| e.kind.as_str() == "HandledBy")
        .map(|e| e.to_id)
        .expect("apple_login must have a HandledBy handler");
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
        "the handler must call WechatServices::appAuth (resolved from the typed parameter `WechatServices $services`)"
    );

    // ③ Dao -> Model: `WechatUserDao::setModel()` returns `WechatUser::class` -> ResolvesTo edge.
    let daos = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: Some("WechatUserDao".to_string()),
            limit: Some(5),
            offset: Some(0),
        })
        .expect("the query must be readable");
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
            "WechatUserDao must have a ResolvesTo edge to WechatUser (setModel returns User::class)"
        );
    } else {
        eprintln!("WechatUserDao class not found, skipping the Dao→Model assertion");
    }
}

/// Specifically verify this route in `samples/thinkphp-projects/CRMEB-master/crmeb/app/api/route/v1.php`:
///
/// ```php
/// Route::post('apple_login', 'v1.LoginController/appleLogin')->name('appleLogin');
/// ```
///
/// The chain should be:
/// `POST /apple_login` (HttpContract)
///   -HandledBy-> `LoginController::appleLogin`
///   -Calls-> `WechatServices::appAuth`            (typed parameter `WechatServices $services`)
///   -Calls-> `CacheService::get` -ReadsCache-> `Cache` (kind `Cache`)
///   -Calls-> `WechatUserDao` -ResolvesTo-> `WechatUser` -MapsTo-> `wechat_user` (Table)
/// and read `ConfigKey` (sys_config).
#[test]
fn apple_login_route_chain_from_v1_php() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    use gt_domain::port::EdgeDirection;

    let find_nodes = |kind: &str, name_contains: &str| -> Vec<gt_domain::model::Node> {
        b.store
            .query_nodes(&NodeFilter {
                project_id: b.project.id,
                kind: Some(NodeKind(kind.to_string())),
                name_contains: Some(name_contains.to_string()),
                limit: Some(50),
                offset: Some(0),
            })
            .expect("the query must be readable")
    };
    let node_of = |id: gt_domain::model::NodeId| -> Option<gt_domain::model::Node> {
        b.store.get_node(id).ok().flatten()
    };
    let out_edges = |id: gt_domain::model::NodeId| -> Vec<gt_domain::model::Edge> {
        b.store
            .edges_of(id, EdgeDirection::Outgoing)
            .unwrap_or_default()
    };

    // ---- 1) Contract node: must come from line 31 of route/v1.php ----
    let contracts = find_nodes(NodeKind::HTTP_CONTRACT, "apple_login");
    let contract = contracts
        .iter()
        .find(|n| n.name == "POST /apple_login")
        .expect("the `POST /apple_login` contract must exist");
    let props = &contract.properties;
    assert_eq!(
        props.get("handler").and_then(|v| v.as_str()),
        Some("v1.LoginController/appleLogin"),
        "the handler field must keep the spelling used in the route file"
    );
    assert_eq!(
        props.get("raw_path").and_then(|v| v.as_str()),
        Some("apple_login"),
        "raw_path must be the 'apple_login' from the route"
    );
    let loc = props
        .get("locations")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .expect("the contract must carry a source location");
    let loc_file = loc.get("file").and_then(|v| v.as_str()).unwrap_or("");
    let loc_line = loc.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(
        loc_file.ends_with("app/api/route/v1.php"),
        "the contract must come from app/api/route/v1.php, got {loc_file}"
    );
    assert_eq!(loc_line, 31, "the contract must be located at Route::post on line 31 of v1.php");

    // ---- 2) HandledBy → LoginController::appleLogin ----
    let handler = out_edges(contract.id)
        .into_iter()
        .find(|e| e.kind.as_str() == "HandledBy")
        .map(|e| e.to_id)
        .expect("the route must be handled by a handler (HandledBy)");
    let handler_node = node_of(handler).expect("the handler node must exist");
    assert_eq!(
        handler_node.fqn.as_deref(),
        Some("app\\api\\controller\\v1\\LoginController::appleLogin"),
        "it must resolve to v1\\LoginController::appleLogin"
    );

    // ---- 3) Typed parameter: handler -Calls-> WechatServices::appAuth ----
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
        "appleLogin must call WechatServices::appAuth (resolved from the type of the `WechatServices $services` parameter)"
    );

    // ---- 4) The forward chain reaches semantic nodes (semantic nodes are endpoints) ----
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
    eprintln!("semantic endpoint of the POST /apple_login chain: {semantic:?}");

    let hit = |kind: &str, name: &str| semantic.iter().any(|(k, n)| k == kind && n == name);
    assert!(
        hit("Cache", "Cache"),
        "the chain must reach a node of kind `Cache` (CacheService::get -> think\\facade\\Cache), got {semantic:?}"
    );
    assert!(
        hit(NodeKind::TABLE, "wechat_user"),
        "the chain must reach Table(wechat_user), got {semantic:?}"
    );
    assert!(
        semantic.iter().any(|(k, _)| k == NodeKind::CONFIG_KEY),
        "the chain must reach a ConfigKey (sys_config), got {semantic:?}"
    );

    // ---- 5) Key semantic edges: Dao -> Model -> Table ----
    let dao = find_nodes(NodeKind::CLASS, "WechatUserDao")
        .into_iter()
        .find(|n| n.name == "WechatUserDao")
        .expect("the WechatUserDao class must exist");
    let model = out_edges(dao.id)
        .into_iter()
        .filter(|e| e.kind.as_str() == "ResolvesTo")
        .filter_map(|e| node_of(e.to_id))
        .find(|n| n.name == "WechatUser")
        .expect("WechatUserDao --ResolvesTo--> WechatUser (setModel returns User::class)");
    out_edges(model.id)
        .into_iter()
        .filter(|e| e.kind.as_str() == "MapsTo")
        .filter_map(|e| node_of(e.to_id))
        .find(|n| n.name == "wechat_user" && n.kind.as_str() == NodeKind::TABLE)
        .expect("WechatUser --MapsTo--> wechat_user（Table）");

    // ---- 6) Key semantic edge: CacheService reads Cache (FKB facade rule) ----
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
        "some method of CacheService must have a ReadsCache edge to a node of kind `Cache`"
    );
}

#[test]
fn calls_edge_records_call_site_node() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let node_of = |id: gt_domain::model::NodeId| -> Option<gt_domain::model::Node> {
        b.store.get_node(id).ok().flatten()
    };
    let out_edges = |id: gt_domain::model::NodeId| -> Vec<gt_domain::model::Edge> {
        b.store
            .edges_of(id, gt_domain::port::EdgeDirection::Outgoing)
            .unwrap_or_default()
    };

    // Find a Calls edge carrying a call_site, verifying the "call site" is recorded precisely on the edge (not guessed by name)
    let mut found: Option<gt_domain::model::Edge> = None;
    for n in b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Method".to_string())),
            name_contains: None,
            limit: Some(2000),
            offset: Some(0),
        })
        .expect("the method query must be readable")
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
    let e = found.expect("at least one Calls edge recording call_site must exist");
    let cs_id = e
        .properties
        .get("call_site")
        .and_then(|v| v.as_i64())
        .expect("call_site must be an integer node id");
    let cs = node_of(gt_domain::model::NodeId(cs_id)).expect("the node call_site points at must exist");
    assert_eq!(
        cs.kind.as_str(),
        "CallSite",
        "call_site must point exactly at a CallSite node (100% precise, no heuristics)"
    );
    assert!(
        cs.file_id.is_some(),
        "a CallSite must carry a source location (file + line)"
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
        .expect("the query must be readable");
    let triggers = events.iter().any(|e| {
        b.store
            .edges_of(e.id, gt_domain::port::EdgeDirection::Incoming)
            .unwrap_or_default()
            .iter()
            .any(|x| x.kind.as_str() == "Triggers")
    });
    assert!(triggers, "event('x') must resolve into a Triggers edge");
}

// ---------------------------------------------------------------- Honesty

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
        .expect("the query must be readable");
    let hit = tables
        .iter()
        .find(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some("user"))
        .expect("the user table must exist");
    let locations = hit
        .properties
        .get("locations")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert!(
        locations >= 2,
        "a synthesised node must keep all its co-occurrence locations (no invented single location), got {locations}"
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
            "phase {phase} must have a run report"
        );
    }
    let cf = common::phase_report(&b, "CfAst").expect("the CfAst report");
    assert!(cf.nodes_created > 1000, "CfAst must produce a large number of nodes");
}

#[test]
fn project_and_sub_project_ids_are_consistent() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    assert_eq!(b.project.id, ProjectId(1));
    let subs = b.store.list_sub_projects(b.project.id).expect("it must be readable");
    assert!(subs.iter().all(|s| s.project_id == b.project.id));
    assert!(subs.iter().any(|s| s.id == SubProjectId(1)));
}

// ---------------------------------------------------------------- v1.php special

/// Whether v1.php's routes entered the graph: the file was scanned + its contributed HttpContract nodes exist.
#[test]
fn v1_php_routes_are_in_graph() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };

    // 1) Was v1.php scanned into the graph
    let files = b.store.list_files(b.project.id, None).expect("it must be readable");
    assert!(
        files
            .iter()
            .any(|f| f.path.ends_with("crmeb/app/api/route/v1.php")),
        "v1.php must be scanned into the graph"
    );

    // 2) Which HttpContract nodes come from v1.php
    let contracts = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: None,
            limit: Some(5000),
            offset: Some(0),
        })
        .expect("it must be readable");

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
    eprintln!("HttpContract count contributed by v1.php = {}", from_v1.len());
    for id in from_v1.iter().take(30) {
        eprintln!("  {id}");
    }
    assert!(!from_v1.is_empty(), "v1.php must contribute at least some route contracts");
    for must in [
        "POST /apple_login",
        "ANY /wechat/serve",
        "GET /admin/order/statistics",
        "POST /login",
        "GET /pay/config",
    ] {
        assert!(from_v1.iter().any(|x| x == must), "the expected route {must} is missing");
    }
}

/// Look directly at v1.php's parse product (raw call sites), without going through the whole graph.
/// Queue semantic nodes should be detected by the **framework-level** FKB (no project-level FKB needed).
///
/// CRMEB uses `think\facade\Queue` via `QueueTrait::dispatch` / `crmeb\utils\Queue`; the framework rule `thinkphp-queue-topic`
/// synthesises the queue topic through `arg:0` (the Job class) + `owner_class` fallback (the class that produces the call);
/// propagation then walks the `PublishesTo` edge up to each Service.
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
        .expect("the query must be readable");
    eprintln!("Queue nodes detected in CRMEB = {}", queues.len());
    assert!(
        !queues.is_empty(),
        "the framework-level FKB must detect queue nodes (CRMEB uses think\\facade\\Queue through facades / wrappers / traits)"
    );
    // Every Queue node should have at least one PublishesTo in-edge (who publishes).
    let with_publisher = queues
        .iter()
        .filter(|q| {
            b.store
                .edges_of(q.id, gt_domain::port::EdgeDirection::Incoming)
                .unwrap_or_default()
                .iter()
                .any(|e| e.kind.as_str() == "PublishesTo")
        })
        .count();
    assert!(
        with_publisher > 0,
        "at least some Queue nodes must have a PublishesTo in-edge, got {with_publisher}/{}",
        queues.len()
    );
    // A queue should also have a consumer (HandledBy): when `arg:0` cannot resolve, fall back to `receiver_class` (the delivered
    // Job class), whose `doJob`/`handle` is the consumer entry, so the "queue view" sees consumers like events do.
    let with_consumer = queues
        .iter()
        .filter(|q| {
            b.store
                .edges_of(q.id, gt_domain::port::EdgeDirection::Outgoing)
                .unwrap_or_default()
                .iter()
                .any(|e| e.kind.as_str() == "HandledBy")
        })
        .count();
    assert!(
        with_consumer > 0,
        "at least some Queue nodes must have a HandledBy out-edge (the consumer), got {with_consumer}/{}",
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
        .expect("the query must be readable");
    eprintln!("Schedule nodes detected in CRMEB = {}", schedules.len());
    assert!(
        !schedules.is_empty(),
        "the project-level FKB (crmeb.yaml) must synthesise crontab/* routes into Schedule nodes"
    );
    // Every Schedule node should HandledBy to its corresponding CrontabController method.
    let with_handler = schedules
        .iter()
        .filter(|s| {
            b.store
                .edges_of(s.id, gt_domain::port::EdgeDirection::Outgoing)
                .unwrap_or_default()
                .iter()
                .any(|e| e.kind.as_str() == "HandledBy")
        })
        .count();
    assert!(
        with_handler > 0,
        "at least some Schedule nodes must have a HandledBy out-edge (the handler), got {with_handler}/{}",
        schedules.len()
    );
}

/// Cache nodes must be distinguished **by literal key**, not collapsed into one global blob;
/// and variable / expression arguments must **never** be used as identity (otherwise junk nodes like `$name`, `self::X . $y` would be created).
///
/// Identity was once hard-coded as `literal: "Cache"`: 346 call edges converged on one node, and the `key` field was overwritten repeatedly leaving only the last value —
/// neither answering "who read/wrote the same cache key" nor keeping integrity clean.
#[test]
fn cache_nodes_split_by_literal_key() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let caches = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Cache".into())),
            name_contains: None,
            limit: Some(2000),
            offset: Some(0),
        })
        .expect("the query must be readable");
    eprintln!(
        "there are {} Cache nodes: {:?}",
        caches.len(),
        caches.iter().map(|n| n.name.as_str()).collect::<Vec<_>>()
    );

    // ① Dynamic keys (`Cache::get($name)` etc.) must fall back to the unified `Cache` node.
    assert!(
        caches.iter().any(|n| n.name == "Cache"),
        "there must be a dynamic-key fallback node `Cache`, got {:?}",
        caches.iter().map(|n| n.name.as_str()).collect::<Vec<_>>()
    );
    // ② At least one literal-key node (e.g. `crontabCache`) should appear.
    assert!(
        caches.iter().any(|n| n.name != "Cache"),
        "nodes must be split by their literal key (e.g. crontabCache), got {:?}",
        caches.iter().map(|n| n.name.as_str()).collect::<Vec<_>>()
    );
    // ③ A variable / expression text must never be used as identity (a valid literal key contains neither `$` nor `::`).
    for n in &caches {
        assert!(
            !n.name.starts_with('$'),
            "a variable name is taken as the cache identity: {}",
            n.name
        );
        assert!(
            !n.name.contains("::"),
            "an expression text is taken as the cache identity: {}",
            n.name
        );
    }
}

#[test]
fn v1_php_parse_result() {
    let Some(root) = common::sample_root() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    let path = root.join("crmeb/app/api/route/v1.php");
    let src = std::fs::read_to_string(&path).expect("reading v1.php");
    let reg = gt_adapter_parser::DefaultParserRegistry::new();
    let parser = reg
        .parser_for(&gt_domain::model::Language::new(
            gt_domain::model::Language::PHP,
        ))
        .expect("the php parser");
    let facts = parser.parse(path.to_str().unwrap(), &src).expect("parsing");

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
    eprintln!("Route:: call-site count = {}", route_calls.len());
    for c in route_calls.iter().take(30) {
        let args: Vec<String> = c
            .args
            .iter()
            .map(|a| match a {
                gt_domain::model::FactValue::String(s) => s.clone(),
                _ => "?".to_string(),
            })
            .collect();
        eprintln!(
            "  {}  args={:?}  @{}:{}",
            c.callee_text, args, c.owner_fqn, c.span.start_line
        );
    }
    assert!(!route_calls.is_empty(), "v1.php must yield Route call sites");
}

/// Facade short names must be resolved by **the file's own `use`**, not fall back to the global short-name index to guess.
///
/// `crmeb/crmeb/services/CacheService.php` writes `use think\facade\Cache;`, while the project happens to have a Model named
/// `app\model\other\Cache` (`protected $name = 'cache'`). With global short-name-index resolution, `Cache::tag($tag)->remember(...)`
/// would be treated as that Model, so every cache call would spuriously gain a `Calls` edge and drag the
/// `Model --MapsTo--> Table(cache)` class-level semantic edge onto every route along the call chain.
///
/// This is PHP's name-resolution rule (per file), framework-agnostic; `Request` / `Route` / `Response` etc. have same-named project
/// classes too, protected by the same rule.
#[test]
fn facade_short_name_resolves_per_file_import() {
    let Some(b) = built() else {
        eprintln!("{}", common::skip_reason());
        return;
    };
    // `name_contains` matches the node's `name`, so coarse-filter by short name first, then locate precisely by fqn.
    let by_fqn = |fqn: &str| -> Option<gt_domain::model::Node> {
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
        eprintln!("the graph has no app\\model\\other\\Cache, skipping");
        return;
    };
    let remember = by_fqn("crmeb\\services\\CacheService::remember");
    let Some(remember) = remember else {
        eprintln!("the graph has no CacheService::remember, skipping");
        return;
    };

    // `Cache::...` in this file is `think\facade\Cache` (outside the graph, vendor excluded),
    // so it should **not** resolve to the same-named project Model.
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
         `use think\\facade\\Cache;` 是图外的框架类，短名parsing不能退回全局同名类"
    );

    // That Model's own class-level semantic edges must be intact (the fix is "guessed the wrong call", not "deleted the class").
    let maps_to = b
        .store
        .edges_of(model.id, EdgeDirection::Outgoing)
        .unwrap_or_default()
        .into_iter()
        .any(|e| e.kind.as_str() == "MapsTo");
    assert!(maps_to, "app\\model\\other\\Cache must still keep the class-level Model --MapsTo--> Table edge");
}


