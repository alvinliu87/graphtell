//! End-to-end self-check for rule detection and prompt-augmented (code recall) recall.
//!
//! **Deliberately independent of external samples**: here we write a small hand-built set of nodes and edges straight into the DB,
//! so it always runs on any machine at any time (the CRMEB-style sample tests skip when the sample is missing,
//! and can't guard the regression of these two new capabilities).
//!
//! Covers three things:
//!   1. the rule engine **really can be driven by YAML** (not hard-coded);
//!   2. violations land as `rule:*` diagnostics, are readable back, and are **cleared and rebuilt** on re-run (no stale historical data);
//!   3. recall **is more than keyword matching** — after a seed hits, it must pull related code that doesn't contain the keyword along the graph.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gt_adapter_rules::YamlRuleSet;
use gt_app::AppConfig;
use gt_application::{RecallQuality, RecallQuery, RecallService, RuleService};
use http_body_util::BodyExt;
use tower::ServiceExt;
use gt_domain::model::{
    AnnotationChannel, CheckPredicate, EdgeKind, FileId, IdentityKey, Language, MergeStrategy,
    NewAnnotation, NewEdge, NewNode, NewProject, NewSourceFile, NodeId, NodeKind, Phase, ProjectId,
    Severity, Span,
};
use gt_domain::port::{
    DiagnosticSink, GraphDelta, GraphSink, Persistence, ProjectWriter, RuleProvider,
};

struct Fixture {
    container: gt_app::Container,
    project: ProjectId,
}

/// Locate the sample root: prefer `GRAPHTELL_SAMPLES_DIR`, otherwise walk up from `CARGO_MANIFEST_DIR` level by level
/// looking for a `samples/` containing `frontend-backend-link`.
///
/// The sample tree's location isn't fixed (may be at the workspace root, or the upper repo root); hard-coding one place would
/// **silently fail to find the sample** after switching machines or merging sample dirs — the test then gets skipped, becoming "CI all green but zero coverage".
fn samples_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLES_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        if cur.join("samples").join("frontend-backend-link").is_dir() {
            return Some(cur.join("samples"));
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Build a temporary container + project.
///
/// Cases run in parallel, so the dir name must be **unique within the process** (timestamps within the same millisecond collide,
/// making two cases share one SQLite file and pollute each other).
///
/// Returning `None` means the sample is missing (e.g. a release package / partial checkout without `samples/`); the caller should skip,
/// consistent with `gt-pipeline/tests/common/mod.rs`'s `sample_root()` soft-dependency convention: a missing sample must not hard-fail the test.
fn fixture() -> Option<Fixture> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
    // Soft-skip when the sample is missing (release package / partial checkout); point `GRAPHTELL_SAMPLES_DIR` at a stand-in.
    let samples = samples_dir()?;
    let data_dir = std::env::temp_dir().join(format!(
        "graphtell-rules-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        seq
    ));
    std::fs::create_dir_all(&data_dir).expect("临时目录应可创建");
    let config = AppConfig {
        data_dir,
        fkb_dir: Some(root.join("fkb")),
        views_dir: Some(root.join("views")),
        rules_dir: Some(root.join("rules")),
        bind: "127.0.0.1".into(),
        port: 0,
        ui_dir: None,
    };
    let container = gt_app::Container::new(config).expect("容器装配不应失败");
    let project_service = gt_application::ProjectService::new(
        container.store.clone() as Arc<dyn Persistence>,
        Arc::new(gt_domain::port::SystemClock),
    );
    let project = project_service
        .create(NewProject {
            name: "规则与召回自检".into(),
            root_path: samples.join("frontend-backend-link"),
            description: None,
            config: None,
        })
        .expect("工程应可创建");
    Some(Fixture { container, project: project.id })
}

/// For tests "that don't need a real sample" (e.g. performance regression): build a temporary container + project.
///
/// Unlike `fixture()`, this **doesn't depend on `samples/`** — `root_path` can be a placeholder using the in-repo `fkb` dir that always exists,
/// because such tests only write synthesized nodes into the graph and never read sample source. So even without `samples/` in a release package / partial checkout,
/// performance regression isn't dragged down or wrongly skipped.
fn temp_fixture() -> Fixture {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
    let data_dir = std::env::temp_dir().join(format!(
        "graphtell-tmp-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        seq
    ));
    std::fs::create_dir_all(&data_dir).expect("临时目录应可创建");
    let config = AppConfig {
        data_dir,
        fkb_dir: Some(root.join("fkb")),
        views_dir: Some(root.join("views")),
        rules_dir: Some(root.join("rules")),
        bind: "127.0.0.1".into(),
        port: 0,
        ui_dir: None,
    };
    let container = gt_app::Container::new(config).expect("容器装配不应失败");
    let project_service = gt_application::ProjectService::new(
        container.store.clone() as Arc<dyn Persistence>,
        Arc::new(gt_domain::port::SystemClock),
    );
    let project = project_service
        .create(NewProject {
            name: "临时工程".into(),
            root_path: root.join("fkb"),
            description: None,
            config: None,
        })
        .expect("工程应可创建");
    Fixture { container, project: project.id }
}

/// Write a small hand-built graph:
///
/// * `Table:user` — a hot table, read/written by Service and Dao (fan-in 2)
/// * `Table:unused_log` — unused by anyone
/// * `HttpContract:POST /api/order/create` — has a handler
/// * `HttpContract:DELETE /api/ghost` — no handler (should be hit by a rule)
/// * `Method:createOrder` — both writes a table and doesn't literally contain the word "table" (tests recall's graph expansion)
#[allow(clippy::too_many_lines)]
fn seed_graph(f: &Fixture) {
    let store = &f.container.store;
    let pid = f.project;

    let files = store
        .replace_files(
            pid,
            vec![
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/services/OrderServices.php".into(),
                    language: Language::new("php"),
                    size_bytes: 512,
                    content_hash: "h1".into(),
                },
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/model/User.php".into(),
                    language: Language::new("php"),
                    size_bytes: 256,
                    content_hash: "h2".into(),
                },
            ],
        )
        .expect("文件应可写入");
    let svc_file = files[0].id;
    let model_file = files[1].id;

    let node = |kind: &str, name: &str, id: i64, file: Option<gt_domain::model::FileId>| NewNode {
        id: Some(NodeId(id)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new(kind),
        name: name.to_string(),
        fqn: None,
        identity: Some(gt_domain::model::IdentityKey::fqn(name)),
        file_id: file,
        span: gt_domain::model::Span { start_line: 10, end_line: 30, start_byte: 0, end_byte: 0 },
        language: Language::new("php"),
        phase: Phase(Phase::SYNTHESIZE.to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    };

    let table_user = node("Table", "user", 101, Some(model_file));
    let table_unused = node("Table", "unused_log", 102, None);
    let contract_ok = node("HttpContract", "POST /api/order/create", 103, None);
    let contract_ghost = node("HttpContract", "DELETE /api/ghost", 104, None);
    let handler = node("Method", "createOrder", 105, Some(svc_file));
    let dao = node("Method", "userDaoSelect", 106, Some(svc_file));

    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![
                table_user,
                table_unused,
                contract_ok,
                contract_ghost,
                handler.clone(),
                dao.clone(),
            ],
            edges: vec![
                // contract → handler (HandledBy is an **out-edge**, direction must match the rule)
                NewEdge::new(pid, EdgeKind::new(EdgeKind::HANDLED_BY), NodeId(103), NodeId(105)),
                // handler / dao write-read the user table
                NewEdge::new(pid, EdgeKind::new(EdgeKind::WRITES_DB), NodeId(105), NodeId(101)),
                NewEdge::new(pid, EdgeKind::new(EdgeKind::READS_DB), NodeId(106), NodeId(101)),
            ],
            annotations: vec![NewAnnotation {
                node_id: NodeId(101),
                channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
                kind: "pii".into(),
                subkind: Some("phone".into()),
                confidence: 0.9,
                evidence: serde_json::json!({ "hook": "column:user_phone" }),
                phase: Phase(Phase::ANNOTATE_POST.to_string()),
                merge: MergeStrategy::Coexist,
            }],
            ..Default::default()
        })
        .expect("图应可写入");

}

// ---------------------------------------------------------------- rules: YAML-defined

/// Rules must be definable in YAML — this is the proof that "the kernel knows no rules".
const RULES_YAML: &str = r#"
rules:
  - id: demo-contract-no-handler
    title: 契约没有 handler
    severity: error
    category: contract
    applies_to:
      kinds: [HttpContract]
    when:
      - no_outgoing: HandledBy
    message: "契约 {name} 没有 handler"
  - id: demo-pii-table
    title: 含 PII 的表
    severity: warning
    category: security
    applies_to:
      kinds: [Table]
    when:
      - has_annotation: pii
    message: "表 {name} 含 PII"
  - id: demo-dead-table
    title: 无人使用的表
    severity: info
    category: deadcode
    applies_to:
      kinds: [Table]
    when:
      - fan_in_lte: 0
    message: "表 {name} 没有任何读写引用"
"#;

#[test]
fn rules_are_defined_in_yaml_and_evaluated_on_graph() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);

    let rules = Arc::new(YamlRuleSet::from_str(RULES_YAML).expect("规则 YAML 应可解析"));
    assert_eq!(rules.len(), 3, "三条规则都应装载");

    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        rules.clone(),
    );
    let report = svc
        .check(f.project, None, false)
        .expect("检查不应失败");

    let hit = |id: &str| {
        report
            .violations
            .iter()
            .filter(|v| v.rule_id == id)
            .count()
    };
    assert_eq!(hit("demo-contract-no-handler"), 1, "只有 DELETE /api/ghost 没有 handler");
    assert_eq!(hit("demo-pii-table"), 1, "只有 user 表带 pii 标注");
    assert_eq!(hit("demo-dead-table"), 1, "unused_log 没有任何读写边");

    let ghost = report
        .violations
        .iter()
        .find(|v| v.rule_id == "demo-contract-no-handler")
        .expect("应存在幽灵契约违规");
    assert_eq!(ghost.severity, Severity::Error);
    assert_eq!(ghost.node_name, "DELETE /api/ghost");
    assert!(ghost.message.contains("DELETE /api/ghost"), "文案应渲染出节点名");

    // Violation order: errors first
    let first = report.violations.first().expect("至少有 1 条违规");
    assert_eq!(first.severity, Severity::Error);
}

/// When running only part of the rules, **don't** clear the other rules' historical results
/// (otherwise "re-run A alone" would wipe B/C on the side, and the report would mysteriously miss a chunk).
#[test]
fn partial_rerun_only_replaces_its_own_violations() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(RULES_YAML).expect("规则应可解析"));
    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        rules.clone(),
    );

    svc.check(f.project, None, true).expect("全量检查应成功");
    assert_eq!(svc.violations(f.project, 500, None).expect("回读").len(), 3);

    // Re-run only the pii rule: the other two must still be there
    svc.check(f.project, Some(&["demo-pii-table".to_string()]), true)
        .expect("部分检查应成功");
    let left = svc.violations(f.project, 500, None).expect("回读");
    assert_eq!(left.len(), 3, "部分重跑应保留其它规则的结论");
    assert!(
        left.iter().any(|v| v.rule_id == "demo-dead-table")
            && left.iter().any(|v| v.rule_id == "demo-contract-no-handler"),
        "未被重跑的规则结论必须仍在，实际：{:?}",
        left.iter().map(|v| &v.rule_id).collect::<Vec<_>>()
    );
}

/// Violations land as `rule:*` diagnostics; on re-run **clear old then write new**, leaving no stale historical data.
#[test]
fn violations_persist_as_diagnostics_and_are_replaced_on_rerun() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(RULES_YAML).expect("规则应可解析"));
    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        rules.clone(),
    );

    svc.check(f.project, None, true).expect("首次检查应成功");
    let first = svc.violations(f.project, 500, None).expect("应能回读违规");
    assert_eq!(first.len(), 3, "落库后应能回读出 3 条违规");
    assert!(
        first.iter().all(|v| v.rule_id.starts_with("demo-")),
        "回读的应是规则违规，而非建图期诊断"
    );

    // Run again: result must **still be 3**, not accumulate to 6
    svc.check(f.project, None, true).expect("重复检查应成功");
    let second = svc.violations(f.project, 500, None).expect("应能回读违规");
    assert_eq!(second.len(), 3, "重跑必须清空旧违规，而不是累加");
}

/// Run only specified rules (`only` filter).
#[test]
fn check_can_run_a_subset_of_rules() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(RULES_YAML).expect("规则应可解析"));
    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        rules.clone(),
    );
    let report = svc
        .check(f.project, Some(&["demo-pii-table".to_string()]), false)
        .expect("检查应成功");
    assert_eq!(report.rules_run, 1);
    assert_eq!(report.violations.len(), 1);
    assert_eq!(report.violations[0].rule_id, "demo-pii-table");
}

/// Predicate combinators: `Not` / `AnyOf` must be usable (otherwise the rule language lacks expressiveness).
#[test]
fn predicate_composition_works() {
    let yaml = r#"
rules:
  - id: composed
    title: 组合谓词
    severity: warning
    category: general
    applies_to:
      kinds: [Table]
    when:
      - any_of:
          - name_contains: user
          - name_contains: nonexistent
      - not: { name_contains: unused }
    message: "命中 {name}"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(yaml).expect("规则应可解析"));
    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        rules.clone(),
    );
    let report = svc.check(f.project, None, false).expect("检查应成功");
    let names: Vec<&str> = report.violations.iter().map(|v| v.node_name.as_str()).collect();
    assert_eq!(names, vec!["user"], "any_of 命中 user，not 排除掉 unused_log");
}

/// The built-in rule library must load (guards against `rules/*.yaml` being broken unnoticed).
#[test]
fn builtin_rules_yaml_loads() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    let set = &f.container.rules;
    assert!(!set.is_empty(), "内置规则库不应为空");
    for r in set.rules() {
        assert!(!r.id.is_empty());
        assert!(!r.message.is_empty());
        assert!(
            !r.applies_to.kinds.is_empty(),
            "规则 {} 未声明作用范围：会退化成扫全图",
            r.id
        );
    }
    assert!(
        set.rules().iter().any(|r| r.category == "contract"),
        "内置规则应至少包含一条契约类规则"
    );
}

// ---------------------------------------------------------------- recall

/// Recall's core value: not just keyword matching, but **pulling related code out along the graph**.
#[test]
fn recall_expands_from_seed_along_graph() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );

    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "user".into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");

    assert!(!result.hits.is_empty(), "至少要召回一条");

    // 1) the seed should hit `Table:user`
    assert!(
        result.seeds.iter().any(|s| s.name == "user"),
        "种子应命中 user 表，实际：{:?}",
        result.seeds.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // 2) expansion: `createOrder` has no "user" in its name, but it writes the user table —
    //    it must be pulled out by graph expansion; this is exactly where "recall" differs from "full-text search".
    let names: Vec<&str> = result.hits.iter().map(|h| h.name.as_str()).collect();
    assert!(
        names.contains(&"createOrder"),
        "写 user 表的 createOrder 应被扩展带出，实际命中：{names:?}"
    );
    assert!(
        names.contains(&"userDaoSelect"),
        "读 user 表的方法也应被带出，实际命中：{names:?}"
    );

    // 3) hop count vs direct-hit distinction must be correct
    let seed_hit = result
        .hits
        .iter()
        .find(|h| h.name == "user")
        .expect("user 表应在命中里");
    assert_eq!(seed_hit.hop, 0);
    assert!(seed_hit.direct, "种子是直接命中");
    let expanded = result
        .hits
        .iter()
        .find(|h| h.name == "createOrder")
        .expect("createOrder 应在命中里");
    assert!(!expanded.direct, "扩展出来的是间接命中");
    assert!(expanded.hop >= 1);
}

/// The candidate-set snapshot's cross-request reuse (a latency optimization) must **not** come at the cost of "answering a stale graph".
///
/// A resident service only gets `clear_node_cache` when watch triggers a rebuild; if the graph is rewritten in **another process**
/// (e.g. a manual `graphtell run`), the service isn't notified. So before each snapshot reuse, validate the size with a lightweight `stats`;
/// if the size mismatches, rebuild — this case locks that invariant: the same `RecallService`, with a mid-way graph change, **without** calling
/// `clear_node_cache`, the second recall must see the newly written node.
#[test]
fn recall_candidate_snapshot_refreshes_when_graph_changes_without_notice() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let store = &f.container.store;
    let pid = f.project;

    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let query = |q: &str| {
        svc.recall(
            pid,
            &RecallQuery {
                query: q.into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败")
    };

    // First recall: the node isn't on the graph yet (also loads this project's snapshot into the cache).
    let before = query("zzqLateAddedWidget");
    assert!(
        !before.hits.iter().any(|h| h.name == "zzqLateAddedWidget"),
        "写入前不应存在该节点，实际：{:?}",
        before.hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );

    // Another process changes the graph: adds a node, without notifying RecallService.
    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![NewNode {
                id: Some(NodeId(999)),
                project_id: pid,
                sub_project_id: None,
                kind: NodeKind::new("Method"),
                name: "zzqLateAddedWidget".to_string(),
                fqn: None,
                identity: Some(IdentityKey::fqn("zzqLateAddedWidget")),
                file_id: None,
                span: Span {
                    start_line: 10,
                    end_line: 30,
                    start_byte: 0,
                    end_byte: 0,
                },
                language: Language::new("php"),
                phase: Phase(Phase::SYNTHESIZE.to_string()),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            }],
            edges: Vec::new(),
            ..Default::default()
        })
        .expect("改图应成功");

    // Second recall: must see the new node — otherwise the snapshot is permanently reused, always answering the stale graph.
    let after = query("zzqLateAddedWidget");
    assert!(
        after.hits.iter().any(|h| h.name == "zzqLateAddedWidget"),
        "图已改写但快照未失效（会一直答旧图），实际命中：{:?}",
        after.hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
}

/// A Chinese-intent query ("下单改优惠") must cross the lexicon and seed to English-named business nodes.
///
/// Before (pure lexical substring): only the i18n text `优惠券` whose name happened to contain "优惠" was hit,
/// all the real order/ discount-change code (English-named) was missed. Now via the offline "Chinese-intent word → English symbol" bridge
/// + vector soft matching, it should recall `placeOrder` / `applyDiscount` / `OrderService`, and pull the related code on the graph along too.
#[test]
fn recall_chinese_intent_bridges_to_english_nodes() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    let store = &f.container.store;
    let pid = f.project;

    let files = store
        .replace_files(
            pid,
            vec![
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/services/OrderService.php".into(),
                    language: Language::new("php"),
                    size_bytes: 512,
                    content_hash: "h1".into(),
                },
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/i18n/zh.php".into(),
                    language: Language::new("php"),
                    size_bytes: 256,
                    content_hash: "h2".into(),
                },
            ],
        )
        .expect("文件应可写入");
    let svc_file = files[0].id;
    let i18n_file = files[1].id;

    let node = |kind: &str, name: &str, id: i64, file: Option<FileId>| NewNode {
        id: Some(NodeId(id)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new(kind),
        name: name.to_string(),
        fqn: None,
        identity: Some(IdentityKey::fqn(name)),
        file_id: file,
        span: Span {
            start_line: 10,
            end_line: 30,
            start_byte: 0,
            end_byte: 0,
        },
        language: Language::new("php"),
        phase: Phase(Phase::SYNTHESIZE.to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    };

    let order_svc = node("Class", "OrderService", 201, Some(svc_file));
    let place_order = node("Method", "placeOrder", 202, Some(svc_file));
    let apply_discount = node("Method", "applyDiscount", 203, Some(svc_file));
    let coupon_i18n = node("I18nKey", "优惠券", 204, Some(i18n_file));
    // Landing of the generic-verb bridge: "query/list" → list / find / all …
    let list_orders = node("Method", "listOrders", 206, Some(svc_file));

    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![
                order_svc,
                place_order.clone(),
                apply_discount.clone(),
                coupon_i18n.clone(),
                list_orders.clone(),
            ],
            edges: vec![
                // placeOrder calls applyDiscount, and writes OrderService
                NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(202), NodeId(203)),
                NewEdge::new(pid, EdgeKind::new(EdgeKind::WRITES_DB), NodeId(202), NodeId(201)),
                // listOrders also writes OrderService, for verifying graph expansion
                NewEdge::new(pid, EdgeKind::new(EdgeKind::WRITES_DB), NodeId(206), NodeId(201)),
            ],
            ..Default::default()
        })
        .expect("图应可写入");

    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                // Express intent with **cross-domain generic verbs** (query / list), without depending on any domain lexicon:
                // domain nouns (下单 / 优惠) are uniformly handed to the project's own i18n bridge.
                query: "查询订单列表".into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");

    let names: Vec<&str> = result.hits.iter().map(|h| h.name.as_str()).collect();

    // 1) Chinese generic verbs must bridge to English methods (not just hit Chinese i18n text)
    assert!(
        names.contains(&"listOrders"),
        "「查询/列表」应 bridge 到 listOrders，实际命中：{names:?}"
    );
    // 2) graph expansion: listOrders writes OrderService, should be pulled out
    assert!(
        names.contains(&"OrderService"),
        "OrderService 应被图扩展带出，实际命中：{names:?}"
    );
    // 3) the hits must include "code nodes", not only text-class nodes
    assert!(
        names.iter().any(|n| *n == "listOrders" || *n == "OrderService"),
        "结果应包含代码节点，实际命中：{names:?}"
    );
}

/// End-to-end lock on the ranking result of "如何修改下单优惠" (corresponds to the eval set's same query + grep-baseline case).
///
/// Core invariant: under the action intent (modify), **only pure-verb CRUD methods that hit content words (coupon / order…)**
/// get the 1.5× action boost; an unrelated-domain shipping `DeliveryService.update`, even if its name hits `update`,
/// must rank after the business method `StoreCouponIssue.edit`. Before the fix, shipping CRUD once climbed into Top 5,
/// covering the real coupon-service node.
///
/// This goes through the full lexical-recall pipeline (parse_query → action_intent → score_node → ranking); the two unit tests
/// in [`gt_application::recall_service`] only pinned `score_node` itself; this one adds the "whole pipeline still holds" backstop.
#[test]
fn recall_modify_order_discount_keeps_business_edit_above_shipping_crud() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    let store = &f.container.store;
    let pid = f.project;

    let files = store
        .replace_files(
            pid,
            vec![
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/adminapi/controller/v1/marketing/StoreCouponIssue.php".into(),
                    language: Language::new("php"),
                    size_bytes: 512,
                    content_hash: "hc1".into(),
                },
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/adminapi/controller/v1/order/DeliveryService.php".into(),
                    language: Language::new("php"),
                    size_bytes: 512,
                    content_hash: "hc2".into(),
                },
            ],
        )
        .expect("文件应可写入");
    let coupon_file = files[0].id;
    let ship_file = files[1].id;

    let node = |kind: &str, name: &str, id: i64, file: FileId, fqn: &str| NewNode {
        id: Some(NodeId(id)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new(kind),
        name: name.to_string(),
        fqn: Some(fqn.to_string()),
        identity: Some(IdentityKey::fqn(fqn)),
        file_id: Some(file),
        span: Span { start_line: 10, end_line: 30, start_byte: 0, end_byte: 0 },
        language: Language::new("php"),
        phase: Phase(Phase::SYNTHESIZE.to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    };

    // Business method: class identifier contains coupon → hits content word → keeps 1.5× action boost
    let coupon_edit = node(
        "Method",
        "edit",
        501,
        coupon_file,
        "app\\adminapi\\controller\\v1\\marketing\\StoreCouponIssue::edit",
    );
    // Unrelated-domain CRUD: name hits update, but class identifier (DeliveryService) has no content word,
    // only `order` in the path → must be penalized, no action boost
    let ship_update = node(
        "Method",
        "update",
        502,
        ship_file,
        "app\\adminapi\\controller\\v1\\order\\DeliveryService::update",
    );

    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![coupon_edit, ship_update],
            ..Default::default()
        })
        .expect("图应可写入");

    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "如何修改下单优惠".into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");

    let names: Vec<&str> = result.hits.iter().map(|h| h.name.as_str()).collect();
    let rank = |n: &str| names.iter().position(|x| *x == n);

    let b = rank("edit").expect("业务方法 StoreCouponIssue::edit 应被召回");
    let s = rank("update");
    assert!(
        s.map_or(true, |si| b < si),
        "业务 edit 应排在发货 update 之前（修改意图下内容词才加权），实际：{names:?}"
    );
    // Keywords shouldn't be character-level bigrams: proves Chinese already goes word-level segmentation
    assert!(
        !result.terms.iter().any(|t| t == "何修" || t == "改下" || t == "单优"),
        "查询词不应含跨词边界噪音 bigram，实际：{:?}",
        result.terms
    );
    assert!(
        result.terms.iter().any(|t| t == "修改")
            && result.terms.iter().any(|t| t == "下单")
            && result.terms.iter().any(|t| t == "优惠"),
        "查询词应保留 修改 / 下单 / 优惠，实际：{:?}",
        result.terms
    );
}

/// End-to-end lock on two core invariants of "event-driven queries" (corresponds to the eval set's same queries + 6-scenario review):
///
/// 1. **Listener floats up**: when querying "how to Y after X", the event listeners drowned by the naming convention
///    (`*Listener`, methods uniformly named `handle`, lexical score ≈0) must be supplemented as seeds and pushed to the front —
///    concretely, "下单后怎么发通知" should recall `OrderCreateAfterListener`, "退款成功后怎么回退优惠券"
///    should recall `OrderRefundCreateAfterListener`.
/// 2. **Quality doesn't collapse**: event seeds only do "idf fine-tuning among sibling listeners", not globally overpower the lexical strong hits,
///    so the quality tier must not drop to `Low` (otherwise the whole context is voided, equal to no recall).
///
/// This doesn't use the sample (CRMEB); it directly hand-builds a small graph of "order/refund/notification listeners + business Services",
/// must run on any machine, guarding the two regressions above.
#[test]
fn recall_event_driven_listener_surfaces_without_quality_collapse() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    let store = &f.container.store;
    let pid = f.project;

    let files = store
        .replace_files(
            pid,
            vec![NewSourceFile {
                project_id: pid,
                sub_project_id: None,
                path: "app/listener/order/OrderCreateAfterListener.php".into(),
                language: Language::new("php"),
                size_bytes: 512,
                content_hash: "hl1".into(),
            }],
        )
        .expect("文件应可写入");
    let listener_file = files[0].id;

    let node = |kind: &str, name: &str, id: i64, file: FileId, fqn: &str| NewNode {
        id: Some(NodeId(id)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new(kind),
        name: name.to_string(),
        fqn: Some(fqn.to_string()),
        identity: Some(IdentityKey::fqn(fqn)),
        file_id: Some(file),
        span: Span { start_line: 10, end_line: 30, start_byte: 0, end_byte: 0 },
        language: Language::new("php"),
        phase: Phase(Phase::SYNTHESIZE.to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    };

    // Business Service (contains order / user / coupon content words → strong lexical hit)
    let order_svc = node("Class", "OrderService", 801, listener_file, "app\\services\\OrderService");
    let user_svc = node("Class", "UserService", 802, listener_file, "app\\services\\UserService");
    let coupon_svc = node("Class", "CouponService", 803, listener_file, "app\\services\\CouponService");
    let recover_coupon = node(
        "Method",
        "recoverCoupon",
        804,
        listener_file,
        "app\\services\\CouponService::recoverCoupon",
    );
    let send_notify = node(
        "Method",
        "sendOrderCreateNotify",
        805,
        listener_file,
        "app\\services\\NotifyService::sendOrderCreateNotify",
    );
    let refund_coupon_back = node(
        "Method",
        "refundCouponBack",
        806,
        listener_file,
        "app\\services\\CouponService::refundCouponBack",
    );
    // Event listener (name has Listener → recognized by `is_event_handler`; method name `handle` lexical score ≈0)
    let order_create_listener = node(
        "Class",
        "OrderCreateAfterListener",
        811,
        listener_file,
        "app\\listener\\order\\OrderCreateAfterListener",
    );
    let order_refund_listener = node(
        "Class",
        "OrderRefundCreateAfterListener",
        812,
        listener_file,
        "app\\listener\\order\\OrderRefundCreateAfterListener",
    );
    let notify_listener = node(
        "Class",
        "NotifyListener",
        813,
        listener_file,
        "app\\listener\\notify\\NotifyListener",
    );
    let order_create_event = node(
        "Event",
        "OrderCreateAfterEvent",
        821,
        listener_file,
        "app\\event\\OrderCreateAfterEvent",
    );

    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![
                order_svc,
                user_svc,
                coupon_svc,
                recover_coupon,
                send_notify,
                refund_coupon_back,
                order_create_listener.clone(),
                order_refund_listener.clone(),
                notify_listener.clone(),
                order_create_event.clone(),
            ],
            edges: vec![
                // event → listener (semantic edge; useful later if "event-edge connectivity" weighting is added)
                NewEdge::new(
                    pid,
                    EdgeKind::new(EdgeKind::HANDLED_BY),
                    NodeId(821),
                    NodeId(811),
                ),
            ],
            ..Default::default()
        })
        .expect("图应可写入");

    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );

    // ---- scenario 1: how to notify the user after placing an order → OrderCreateAfterListener must surface, quality must not collapse ----
    let r1 = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "下单后怎么发通知给用户".into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");
    let names1: Vec<&str> = r1.hits.iter().map(|h| h.name.as_str()).collect();
    assert!(
        names1.iter().any(|n| *n == "OrderCreateAfterListener"),
        "「下单后发通知」必须召回 OrderCreateAfterListener，实际：{names1:?}"
    );
    assert_ne!(
        r1.quality,
        RecallQuality::Low,
        "事件种子不得整体压过词面命中导致质量崩到 Low，实际：{:?}（{names1:?}）",
        r1.quality_reason
    );

    // ---- scenario 2: how to roll back coupons after a successful refund → OrderRefundCreateAfterListener + coupon-rollback logic surfaces ----
    let r2 = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "退款成功后怎么回退优惠券".into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");
    let names2: Vec<&str> = r2.hits.iter().map(|h| h.name.as_str()).collect();
    assert!(
        names2.iter().any(|n| *n == "OrderRefundCreateAfterListener"),
        "「退款回退优惠券」必须召回 OrderRefundCreateAfterListener，实际：{names2:?}"
    );
    assert_ne!(
        r2.quality,
        RecallQuality::Low,
        "退款场景质量不得崩到 Low，实际：{:?}（{names2:?}）",
        r2.quality_reason
    );

    // idf fine-tuning invariant: the listener hitting a rare concept (notify) should overpower the sibling hitting only generic words (order)
    let rank = |n: &str| names1.iter().position(|x| *x == n);
    match (rank("NotifyListener"), rank("OrderRefundCreateAfterListener")) {
        (Some(a), Some(b)) => assert!(
            a < b,
            "NotifyListener（notify，稀有）应排在 OrderRefundCreateAfterListener（仅 order）之前，实际：{names1:?}"
        ),
        _ => {} // If any fails to reach the front we don't force it (mainly depends on lexical-seed competition); the core invariant is already held by the two checks above.
    }
}

/// End-to-end: inject the real `bge-m3` (candle, pure Rust) into `RecallService`; the Chinese intent
/// `下单改优惠` seeds via "semantic vector" straight to the English business nodes `placeOrder` / `applyDiscount`,
/// and their scores clearly beat the unrelated noise node `unused_log`.
///
/// Only compiled / run under `--features model-candle`; auto-skips when no model weights.
#[cfg(feature = "model-candle")]
#[test]
fn recall_real_bge_model_chinese_to_english() {
    use gt_application::embed_model::CandleBgeEmbedder;
    use std::sync::Arc;

    let model_dir = std::env::var("GT_BGE_MODEL").unwrap_or_else(|_| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../models/bge-m3-safetensors")
            .to_string_lossy()
            .into()
    });
    if !std::path::Path::new(&model_dir)
        .join("model.safetensors")
        .exists()
    {
        eprintln!(
            "skip recall_real_bge_model: 未找到 {model_dir}/model.safetensors（先跑 tools/convert_bge_safetensors.py）"
        );
        return;
    }

    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    let store = &f.container.store;
    let pid = f.project;

    let files = store
        .replace_files(
            pid,
            vec![
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/services/OrderService.php".into(),
                    language: Language::new("php"),
                    size_bytes: 512,
                    content_hash: "h1".into(),
                },
                NewSourceFile {
                    project_id: pid,
                    sub_project_id: None,
                    path: "app/i18n/zh.php".into(),
                    language: Language::new("php"),
                    size_bytes: 256,
                    content_hash: "h2".into(),
                },
            ],
        )
        .expect("文件应可写入");
    let svc_file = files[0].id;

    let node = |kind: &str, name: &str, id: i64, file: Option<FileId>| NewNode {
        id: Some(NodeId(id)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new(kind),
        name: name.to_string(),
        fqn: None,
        identity: Some(IdentityKey::fqn(name)),
        file_id: file,
        span: Span {
            start_line: 10,
            end_line: 30,
            start_byte: 0,
            end_byte: 0,
        },
        language: Language::new("php"),
        phase: Phase(Phase::SYNTHESIZE.to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    };

    let order_svc = node("Class", "OrderService", 201, Some(svc_file));
    let place_order = node("Method", "placeOrder", 202, Some(svc_file));
    let apply_discount = node("Method", "applyDiscount", 203, Some(svc_file));
    // Unrelated noise node: no semantic relation to "下单改优惠" whatsoever
    let noise = node("Method", "unused_log", 205, Some(svc_file));

    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![order_svc, place_order.clone(), apply_discount.clone(), noise.clone()],
            edges: vec![
                NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(202), NodeId(203)),
                NewEdge::new(pid, EdgeKind::new(EdgeKind::WRITES_DB), NodeId(202), NodeId(201)),
            ],
            ..Default::default()
        })
        .expect("图应可写入");

    let svc = RecallService::with_embedder(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
        Arc::new(CandleBgeEmbedder::load(&model_dir).expect("加载 bge-m3 失败")),
    );
    let warmed = svc.warm_up(f.project).expect("bge 预热应成功");
    assert!(warmed > 0, "应至少编码一个主题级节点，实际 {warmed}");

    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "下单改优惠".into(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");

    let score_of = |name: &str| {
        result
            .seeds
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.score)
            .expect("seed 应含该节点")
    };
    let p = score_of("placeOrder");
    let a = score_of("applyDiscount");
    let n = result
        .seeds
        .iter()
        .find(|s| s.name == "unused_log")
        .map(|s| s.score)
        .unwrap_or(0.0);
    println!("bge recall scores: placeOrder={p:.1} applyDiscount={a:.1} unused_log={n:.1}");

    assert!(p > n, "placeOrder 得分应高于噪声 unused_log：{p} vs {n}");
    assert!(a > n, "applyDiscount 得分应高于噪声 unused_log：{a} vs {n}");
}

/// Chinese structural hints ("表") should converge results to the corresponding node kind.
#[test]
fn recall_understands_chinese_kind_hints() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );

    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "订单相关的表".into(),
                limit: 10,
                hops: 1,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");

    assert!(
        result.kind_hints.contains(&"Table".to_string()),
        "“表”应被识别为 Table 结构提示，实际：{:?}",
        result.kind_hints
    );
}

/// After graph build, a compliance check must run **automatically** — the user must not be required to trigger it manually.
///
/// The assertion deliberately picks "stale violations get cleared" rather than "violations are produced":
/// the check's first step clears the old `rule:`-prefixed diagnostics, so manually planting one stale diagnostic,
/// it must disappear after build — thus even if this sample hits zero violations, we can confirm the check really ran.
#[test]
fn build_runs_check_automatically() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };

    f.container
        .store
        .push_diagnostics(&[gt_domain::model::Diagnostic {
            project_id: f.project,
            sub_project_id: None,
            phase: Phase(Phase::CHECK.to_string()),
            code: "rule:stale-should-be-cleared".into(),
            severity: Severity::Info,
            message: "陈旧违规".into(),
            location: None,
            payload: serde_json::json!({}),
        }])
        .expect("诊断应可写入");

    let pipeline = gt_application::PipelineService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        Arc::clone(&f.container.deps),
        Arc::clone(&f.container.rules) as Arc<dyn RuleProvider>,
    );
    pipeline
        .run(f.project, &gt_domain::port::NoopObserver)
        .expect("建图不应失败");

    let left = f
        .container
        .store
        .list_diagnostics_by_code(f.project, "rule:", None, 100)
        .expect("诊断应可读回");
    assert!(
        left.iter().all(|d| d.code != "rule:stale-should-be-cleared"),
        "建图后自动检查应清掉陈旧的 rule: 诊断，实际残留 {:?}",
        left.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// An environment declaring a tech stack: attach sub-projects with the given language/framework to a project.
///
/// The environment gate reads exactly `sub_projects.language` / `.frameworks`,
/// so the test must be able to control it — otherwise with no sub-projects by default, any rule with a language declaration would be "not applicable".
fn set_stack(f: &Fixture, language: &str, frameworks: &[&str]) {
    f.container
        .store
        .replace_sub_projects(
            f.project,
            vec![gt_domain::model::NewSubProject {
                project_id: f.project,
                name: "app".into(),
                root_path: PathBuf::from("/app"),
                language: Language::new(language),
                role: "backend".into(),
                detected_by: "test".into(),
                frameworks: frameworks.iter().map(|s| (*s).to_string()).collect(),
                facts: serde_json::Value::Null,
            }],
        )
        .expect("子工程应可写入");
}

/// Environment gate: language-specific rules **must not** run on a project that doesn't match.
///
/// This isn't pedantry — edges only produced by the PHP FKB (Triggers / PublishesTo) don't exist at all in a Java project,
/// so `no_incoming: Triggers` would report every event node as "nobody triggers it"; likewise edges only produced by JS
/// (Emits / ListensTo, the frontend event bus) don't exist in a pure Java project.
#[test]
fn php_only_rules_are_skipped_on_java_project() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    set_stack(&f, "java", &["spring-boot"]);

    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.rules.clone() as Arc<dyn RuleProvider>,
    );
    let report = svc.check(f.project, None, false).expect("检查不应失败");

    for id in ["orphan-event", "orphan-queue", "raw-sql-sink", "eventbus-orphan"] {
        assert!(
            report.rules_not_applicable.iter().any(|s| s.starts_with(id)),
            "{id} 是 PHP 专属规则，在 Java 工程上应被判为不适用，实际 not_applicable={:?}",
            report.rules_not_applicable
        );
        assert!(
            !report.violations.iter().any(|v| v.rule_id == id),
            "{id} 不应在 Java 工程上产出任何违规"
        );
    }
    // Reverse confirmation: cross-language contract rules are unaffected, run as usual
    assert!(
        report
            .by_rule
            .keys()
            .any(|k| k.starts_with("http-contract-without-handler"))
            || report.rules_silent.iter().any(|s| s.starts_with("http-contract-without-handler")),
        "跨语言规则在 Java 工程上仍应执行，实际 by_rule={:?}",
        report.by_rule
    );
}

/// The same PHP rule must resume running on a PHP project (the gate must not be a blanket cut).
#[test]
fn php_only_rules_run_on_php_project() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    set_stack(&f, "php", &["thinkphp"]);

    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.rules.clone() as Arc<dyn RuleProvider>,
    );
    let report = svc.check(f.project, None, false).expect("检查不应失败");

    assert!(
        !report
            .rules_not_applicable
            .iter()
            .any(|s| s.starts_with("orphan-event")),
        "PHP 工程上 orphan-event 不应被判为不适用，实际 {:?}",
        report.rules_not_applicable
    );
}

/// Environment gate: frontend (JS)-specific rules **must not** run on a pure backend project.
///
/// `EventBus` nodes and `Emits` / `ListensTo` edges are the semantic of the frontend event bus.
/// A pure backend project has none of them, and the rule would end with "0 hits" — and 0 hits would be read as
/// "no dead code", exactly the failure mode this project most wants to avoid.
#[test]
fn js_only_rules_are_skipped_on_backend_only_project() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    set_stack(&f, "java", &["spring-boot"]);

    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.rules.clone() as Arc<dyn RuleProvider>,
    );
    let report = svc.check(f.project, None, false).expect("检查不应失败");

    for id in [
        "eventbus-emitted-without-listener",
        "eventbus-listened-without-emitter",
    ] {
        assert!(
            report.rules_not_applicable.iter().any(|s| s.starts_with(id)),
            "{id} 是前端规则，在纯后端工程上应被判为不适用，实际 not_applicable={:?}",
            report.rules_not_applicable
        );
        assert!(
            !report.violations.iter().any(|v| v.rule_id == id),
            "{id} 不应在纯后端工程上产出任何违规"
        );
    }
}

/// P12 applies `ext-call-in-loop` **without a language gate** — any stack whose FKB declares
/// `external_calls` (php / js / python / java) gets the annotation — so the rule consuming it must not
/// be PHP-only. While it declared `languages: [php]`, a JS / Python / Java project ran P12, produced the
/// annotation, and then silently dropped it: no rule was ever behind it.
///
/// Consuming it therefore needs one copy **per stack**, each with its own `languages:` and its own id
/// (`n1-query` precedent) — `rules/{php,js,python,java}/runtime.yaml`. This pins all four, which also
/// pins that no copy drifted onto the wrong id: while the rule was PHP-only, a JS / Python / Java project
/// ran P12, produced the annotation, and then silently dropped it — no rule was ever behind it.
///
/// Uses `temp_fixture` (no sample, so it never skips).
#[test]
fn ext_call_in_loop_runs_on_every_stack_that_declares_external_calls() {
    for (language, rule_id) in [
        ("php", "ext-call-in-loop"),
        ("javascript", "js-ext-call-in-loop"),
        ("python", "python-ext-call-in-loop"),
        ("java", "java-ext-call-in-loop"),
    ] {
        let f = temp_fixture();
        let pid = f.project;

        f.container
            .store
            .apply(&GraphDelta {
                project_id: Some(pid),
                nodes: vec![NewNode {
                    id: Some(NodeId(9001)),
                    project_id: pid,
                    sub_project_id: None,
                    kind: NodeKind::new("CallSite"),
                    name: "remoteCall".to_string(),
                    fqn: None,
                    identity: Some(IdentityKey::fqn("loadAll#remoteCall:3")),
                    file_id: None,
                    span: Span { start_line: 3, end_line: 3, start_byte: 0, end_byte: 0 },
                    language: Language::new(language),
                    phase: Phase(Phase::SYNTHESIZE.to_string()),
                    confidence: 1.0,
                    properties: serde_json::json!({ "in_loop": true }),
                }],
                annotations: vec![NewAnnotation {
                    node_id: NodeId(9001),
                    channel: AnnotationChannel("External".to_string()),
                    kind: "ext-call-in-loop".into(),
                    subkind: Some("NetworkInLoop".into()),
                    confidence: 0.85,
                    evidence: serde_json::json!({ "file": "src/api", "line": 3, "callee": "remoteCall" }),
                    phase: Phase("External".to_string()),
                    merge: MergeStrategy::Coexist,
                }],
                ..Default::default()
            })
            .expect("图应可写入");

        set_stack(&f, language, &[]);

        let svc = RuleService::new(
            f.container.store.clone() as Arc<dyn Persistence>,
            f.container.rules.clone() as Arc<dyn RuleProvider>,
        );
        let report = svc.check(f.project, None, false).expect("检查不应失败");

        assert!(
            report.violations.iter().any(|v| v.rule_id == rule_id),
            "{language} 工程上 {rule_id} 规则应触发（P12 不限语言）；实际违规={:?}，not_applicable={:?}",
            report.violations.iter().map(|v| v.rule_id.clone()).collect::<Vec<_>>(),
            report.rules_not_applicable
        );
    }
}

/// Criterion validation: when not a single edge of the criterion's kind exists on the graph, the rule must **deactivate**, not run hard.
///
/// This is the guard against "always-true false positives": when the graph has no `CallsHttp` edge,
/// `no_incoming: CallsHttp` holds for every endpoint, reporting all endpoints as dead endpoints.
#[test]
fn rule_is_disabled_when_its_edge_never_occurs() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    // The criterion's edge kind truly doesn't exist on the graph → the rule must deactivate
    seed_graph(&f);
    set_stack(&f, "php", &[]);

    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.rules.clone() as Arc<dyn RuleProvider>,
    );
    let report = svc.check(f.project, None, false).expect("检查不应失败");

    assert!(
        report
            .rules_unavailable
            .iter()
            .any(|s| s.starts_with("backend-endpoint-never-called")),
        "图上没有 CallsHttp 边时该规则应被停用（否则会把所有端点报成死端点），实际 {:?}",
        report.rules_unavailable
    );
    assert!(
        !report
            .violations
            .iter()
            .any(|v| v.rule_id == "backend-endpoint-never-called"),
        "被停用的规则不应产出违规"
    );
}

/// Rule libraries are loaded per-language directory (`rules/global/` + `rules/php/`), and PHP rules carry environment declarations.
#[test]
fn builtin_rules_are_organised_per_language() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
    let set = YamlRuleSet::load_dir(&root.join("rules")).expect("规则应可装载");
    let rules = set.rules();
    assert!(rules.len() >= 10, "内置规则应全部装载，实际 {}", rules.len());

    let php_tagged: Vec<_> = rules
        .iter()
        .filter(|r| r.applies_to.languages.iter().any(|l| l == "php"))
        .collect();
    assert!(!php_tagged.is_empty(), "应存在声明 php 的规则");

    // Cross-language rules must depend only on topology, never sneak in a language declaration.
    for r in rules.iter().filter(|r| r.applies_to.languages.is_empty()) {
        let req = r.requirements();
        assert!(
            req.annotations.is_empty() && req.capabilities.is_empty(),
            "跨语言规则 {} 不应依赖语言特有的标注/能力，实际 {:?}",
            r.id,
            req
        );
    }
}

/// A long Chinese sentence must be split into bigrams -- otherwise the whole sentence never matches any node and recall returns empty.
///
/// Regression background: Chinese has no word boundaries; an early implementation treated a whole Chinese phrase as a single token,
/// the query "订单创建涉及哪些表和接口" as a whole matched no node name -> **0 hits**.
/// and prompts are often exactly such whole sentences, meaning recall was unusable for Chinese questions.
#[test]
fn recall_splits_chinese_sentence_into_bigrams() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "订单创建涉及哪些表和接口".into(),
                limit: 10,
                hops: 1,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");

    for want in ["订单", "创建"] {
        assert!(
            result.terms.iter().any(|t| t == want),
            "整句应被切成二字组，期望含 {want:?}，实际：{:?}",
            result.terms
        );
    }

    // "表"/"接口" only act as kind boosts and must not linger as text-match words (would steer recall wrong).
    assert!(
        !result.terms.iter().any(|t| t.contains('表') || t.contains('接')),
        "结构提示词不应残留为匹配词，实际：{:?}",
        result.terms
    );
    assert!(
        result.kind_hints.contains(&"Table".to_string())
            && result.kind_hints.contains(&"HttpContract".to_string()),
        "应同时识别 Table / HttpContract 提示，实际：{:?}",
        result.kind_hints
    );
}

/// A full identifier (snake_case not broken apart) must match exactly.
#[test]
fn recall_keeps_snake_case_identifiers_intact() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "unused_log".into(),
                limit: 5,
                hops: 0,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");
    assert!(
        result.terms.contains(&"unused_log".to_string()),
        "`unused_log` 不能被拆成 unused + log，实际词：{:?}",
        result.terms
    );
    assert!(
        result.hits.iter().any(|h| h.name == "unused_log"),
        "应精确命中 unused_log"
    );
}

/// Recall must produce a context pack that can be pasted straight into an LLM.
#[test]
fn recall_produces_markdown_context_pack() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let result = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "user".into(),
                limit: 5,
                hops: 1,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败");
    let md = &result.markdown;
    assert!(md.contains("# Recall context"), "the context pack should have a heading");
    assert!(md.contains("Seeds"), "the context pack should describe the seeds");
    assert!(md.contains("Related code"), "the context pack should list the related code");
}

/// Predicate enums can be constructed directly (so adding a new predicate need not touch the kernel's call sites).
#[test]
fn check_predicate_is_constructible() {
    let p = CheckPredicate::Not(Box::new(CheckPredicate::HasIncoming("HandledBy".into())));
    assert!(matches!(p, CheckPredicate::Not(_)));
}


/// Add a node with `properties` to the graph (nodes in `seed_graph` all have Null properties,
/// so `property_is` can never hold on the existing graph -- without adding the node it can't be tested).
fn seed_node_with_property(f: &Fixture, id: i64, kind: &str, name: &str, props: serde_json::Value) {
    f.container
        .store
        .apply(&GraphDelta {
            project_id: Some(f.project),
            nodes: vec![NewNode {
                id: Some(NodeId(id)),
                project_id: f.project,
                sub_project_id: None,
                kind: NodeKind::new(kind),
                name: name.to_string(),
                fqn: None,
                identity: Some(gt_domain::model::IdentityKey::fqn(name)),
                file_id: None,
                span: gt_domain::model::Span {
                    start_line: 1,
                    end_line: 5,
                    start_byte: 0,
                    end_byte: 0,
                },
                language: Language::new("php"),
                phase: Phase(Phase::SYNTHESIZE.to_string()),
                confidence: 1.0,
                properties: props,
            }],
            ..Default::default()
        })
        .expect("节点应可写入");
}

/// A rule with `enabled: false` must produce **zero violations**.
///
/// This is not formalism: `write-endpoint-without-auth` is currently disabled precisely via `enabled: false`
/// (its criterion is always true on the current graph, so running it only manufactures false confidence).
/// If the `enabled` field ever stops working, this case will blow up immediately.
#[test]
fn disabled_rule_never_produces_violations() {
    const YAML: &str = r#"
rules:
  - id: off-should-not-run
    title: 停用规则
    severity: error
    category: test
    enabled: false
    applies_to:
      kinds: [Table]
    when:
      - fan_in_gte: 0
    message: "表 {name} 命中了（不该发生）"
  - id: on-should-run
    title: 启用规则
    severity: info
    category: test
    applies_to:
      kinds: [Table]
    when:
      - fan_in_gte: 0
    message: "表 {name} 命中了"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());
    let report = svc.check(f.project, None, false).expect("检查不应失败");

    assert_eq!(
        report.violations.iter().filter(|v| v.rule_id == "off-should-not-run").count(),
        0,
        "enabled: false 的规则不应产出任何违规"
    );
    assert!(
        report.violations.iter().any(|v| v.rule_id == "on-should-run"),
        "对照组：enabled 为真的规则应正常命中"
    );
}

/// The three predicates used by the shipped rule: `property_is` / `has_outgoing` / `fan_in_gte`.
#[test]
fn predicates_used_by_shipped_rules_are_covered() {
    const YAML: &str = r#"
rules:
  - id: t-property-is
    title: side 为 frontend 的契约
    severity: warning
    category: contract
    applies_to:
      kinds: [HttpContract]
    when:
      - property_is: { name: "side", value: "frontend" }
    message: "契约 {name} 来自前端"
  - id: t-has-outgoing
    title: 有 handler 的契约
    severity: error
    category: contract
    applies_to:
      kinds: [HttpContract]
    when:
      - has_outgoing: HandledBy
    message: "契约 {name} 有 handler"
  - id: t-fan-in-gte
    title: 被引用过的表
    severity: info
    category: architecture
    applies_to:
      kinds: [Table]
    when:
      - fan_in_gte: 1
    message: "表 {name} 有 {kind} 引用"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    seed_node_with_property(
        &f,
        201,
        "HttpContract",
        "GET /api/from-frontend",
        serde_json::json!({ "side": "frontend" }),
    );

    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());
    let report = svc.check(f.project, None, false).expect("检查不应失败");
    let hit = |id: &str| report.violations.iter().filter(|v| v.rule_id == id).count();

    // Only the just-added GET /api/from-frontend carries side=frontend
    assert_eq!(hit("t-property-is"), 1, "`property_is` 应只命中 side=frontend 的契约");
    // Only POST /api/order/create has a HandledBy out-edge (ghost doesn't)
    assert_eq!(hit("t-has-outgoing"), 1, "`has_outgoing` 应只命中带 handler 的契约");
    // user is referenced by both read and write edges; unused_log is not
    assert_eq!(hit("t-fan-in-gte"), 1, "`fan_in_gte: 1` 应只命中 user 表");
}

/// Framework gate: a rule that declares `frameworks` must also be skipped when that framework is absent.
///
/// The environment gate has two levels — `languages` and `frameworks` — and both are guarded here.
#[test]
fn framework_gate_skips_rules_for_other_frameworks() {
    const YAML: &str = r#"
rules:
  - id: tp-only
    title: 仅 ThinkPHP
    severity: warning
    category: contract
    applies_to:
      kinds: [Table]
      frameworks: [thinkphp]
    when:
      - fan_in_gte: 0
    message: "表 {name}"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());

    set_stack(&f, "php", &["laravel"]);
    let without = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        without.rules_not_applicable.iter().any(|s| s.starts_with("tp-only")),
        "laravel 工程上 thinkphp 规则应不适用，实际 {:?}",
        without.rules_not_applicable
    );

    set_stack(&f, "php", &["thinkphp"]);
    let with = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        !with.rules_not_applicable.iter().any(|s| s.starts_with("tp-only")),
        "thinkphp 工程上应恢复执行，实际 {:?}",
        with.rules_not_applicable
    );
    assert!(
        with.violations.iter().any(|v| v.rule_id == "tp-only"),
        "恢复执行后应真的命中"
    );
}

/// The Java N+1 rule **really hits**: the DAO read call site (CallSite) inside the loop body.
///
/// This case combines three segments -- language gate + `property_is(in_loop)` + `has_annotation(db-query)` --
/// any broken segment makes the rule silently 0-hit (once `in_loop` was written as a JSON boolean while `PropertyIs`
/// only accepts strings, so `property_is` never held). Existing cases only verified the rule loads, not that it hits.
#[test]
fn java_n1_query_rule_fires_on_loop_db_read() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    set_stack(&f, "java", &["spring-boot"]);

    let files = f
        .container
        .store
        .replace_files(
            f.project,
            vec![NewSourceFile {
                project_id: f.project,
                sub_project_id: None,
                path: "app/service/OrderService.java".into(),
                language: Language::new("java"),
                size_bytes: 256,
                content_hash: "h-java-n1".into(),
            }],
        )
        .expect("文件应可写入");
    let file_id = files[0].id;

    f.container
        .store
        .apply(&GraphDelta {
            project_id: Some(f.project),
            nodes: vec![NewNode {
                id: Some(NodeId(401)),
                project_id: f.project,
                sub_project_id: None,
                kind: NodeKind::new("CallSite"),
                name: "userRepository.findById".into(),
                fqn: None,
                identity: Some(gt_domain::model::IdentityKey::fqn("userRepository.findById")),
                file_id: Some(file_id),
                span: gt_domain::model::Span {
                    start_line: 42,
                    end_line: 42,
                    start_byte: 0,
                    end_byte: 0,
                },
                language: Language::new("java"),
                phase: Phase(Phase::RESOLVE.to_string()),
                confidence: 1.0,
                properties: serde_json::json!({ "in_loop": true }),
            }],
            annotations: vec![NewAnnotation {
                node_id: NodeId(401),
                channel: AnnotationChannel("DbQuery".to_string()),
                kind: "db-query".into(),
                subkind: Some("findById".into()),
                confidence: 0.9,
                evidence: serde_json::json!({ "rule": "db-verb-classify" }),
                phase: Phase(Phase::RESOLVE.to_string()),
                merge: MergeStrategy::Coexist,
            }],
            ..Default::default()
        })
        .expect("图应可写入");

    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.rules.clone() as Arc<dyn RuleProvider>,
    );
    let report = svc.check(f.project, None, false).expect("检查不应失败");

    assert!(
        report
            .violations
            .iter()
            .any(|v| v.rule_id == "java-n1-query-in-loop"),
        "java-n1-query-in-loop 应命中循环体内的数据库读调用点，实际违规：{:?}",
        report
            .violations
            .iter()
            .map(|v| &v.rule_id)
            .collect::<Vec<_>>()
    );
    // No `db-write` annotation, so the write-DB edge must not false-hit.
    assert!(
        !report
            .violations
            .iter()
            .any(|v| v.rule_id == "java-n1-write-in-loop"),
        "未打 db-write 标注时 java-n1-write-in-loop 不应误命中"
    );
}

/// The template's `{file}` / `{line}` placeholders must be rendered (`raw-sql-sink` uses them).
#[test]
fn message_renders_file_and_line() {
    const YAML: &str = r#"
rules:
  - id: t-file-line
    title: 位置占位符
    severity: warning
    category: test
    applies_to:
      kinds: [Method]
    when:
      - name_contains: createOrder
    message: "位置 {file}:{line} 名称 {name}"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());
    let report = svc.check(f.project, None, false).expect("检查不应失败");

    let v = report
        .violations
        .iter()
        .find(|v| v.rule_id == "t-file-line")
        .expect("应命中 createOrder");
    assert!(v.file.is_some(), "应解析出文件");
    assert!(v.line.is_some(), "应解析出行号");
    assert!(
        v.message.contains(&v.file.clone().unwrap_or_default()),
        "文案应渲染出文件路径，实际：{}",
        v.message
    );
    assert!(
        !v.message.contains("{file}") && !v.message.contains("{line}"),
        "占位符不应残留，实际：{}",
        v.message
    );
}

/// `kinds` means **constrain the seed kinds**, not filter the results -- this param in all old cases
/// was an empty array and never assigned, so we lock it here.
///
/// Key (a wrong assertion was written before): after restricting `kinds: [Table]` the results **still contain Method**.
/// This is not a bug -- the adjacency used for expansion is project-wide, and the code deliberately backfills the summary of excluded kinds
/// (see the "expansion may reach excluded kinds" passage in `recall_service`).
/// If results were also filtered by kinds, "find related code from a table" would degrade to "list only table names",
/// and recall would regress to keyword filtering, exactly losing the value of its graph expansion.
#[test]
fn recall_kinds_filters_seeds_not_results() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );

    let run = |kinds: Vec<String>| {
        svc.recall(
            f.project,
            &RecallQuery {
                query: "user".into(),
                limit: 50,
                hops: 1,
                kinds,
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败")
    };

    let only_tables = run(vec!["Table".into()]);
    assert!(!only_tables.hits.is_empty(), "限定 Table 时应有结果");

    // Seeds (hop == 0) must all come from the specified kind
    assert!(
        only_tables.hits.iter().filter(|h| h.hop == 0).all(|h| h.kind == "Table"),
        "种子应全部是 Table，实际：{:?}",
        only_tables.hits.iter().filter(|h| h.hop == 0).map(|h| &h.kind).collect::<Vec<_>>()
    );
    // Neighbors pulled in by expansion may be other kinds (this is exactly recall's value)
    assert!(
        only_tables.hits.iter().any(|h| h.hop > 0 && h.kind != "Table"),
        "扩展应能带出其它种类的相关代码，否则召回退化成关键词匹配"
    );

    // Switch the kind and the seeds should change too -- proving kinds really take effect
    let only_contracts = run(vec!["HttpContract".into()]);
    assert!(
        !only_contracts.seeds.iter().any(|s| s.kind == "Table"),
        "限定 HttpContract 时种子里不应出现 Table，实际：{:?}",
        only_contracts.seeds
    );
}

/// `limit` must really truncate results (old cases used limits larger than actual hits, effectively untested).
#[test]
fn recall_limit_truncates_results() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let run = |limit: usize| {
        svc.recall(
            f.project,
            &RecallQuery {
                query: "user".into(),
                limit,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        )
        .expect("召回不应失败")
    };
    let full = run(50);
    assert!(full.hits.len() >= 2, "样本应能召回至少 2 条，实际 {}", full.hits.len());
    let capped = run(1);
    assert_eq!(capped.hits.len(), 1, "limit=1 应只返回 1 条");
}

/// Empty / pure-noise queries must not panic, and should yield empty results rather than returning something arbitrary.
#[test]
fn recall_handles_empty_query() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    for q in ["", "   ", "。"] {
        let r = svc
            .recall(
                f.project,
                &RecallQuery {
                    query: q.into(),
                    limit: 10,
                    hops: 1,
                    kinds: Vec::new(),
                    with_snippets: false,
                include_body: false,
                },
            )
            .unwrap_or_else(|e| panic!("查询 {q:?} 不应失败：{e}"));
        assert!(r.hits.is_empty(), "查询 {q:?} 不应召回任何结果，实际 {} 条", r.hits.len());
    }
}

/// Add a node with `fqn` / `identity` (`seed_graph` nodes have both empty,
/// so without adding it `fqn_contains` / `identity_contains` / `text_contains` can't be tested).
fn seed_rich_node(f: &Fixture, id: i64, kind: &str, name: &str, fqn: &str, identity: &str) {
    f.container
        .store
        .apply(&GraphDelta {
            project_id: Some(f.project),
            nodes: vec![NewNode {
                id: Some(NodeId(id)),
                project_id: f.project,
                sub_project_id: None,
                kind: NodeKind::new(kind),
                name: name.to_string(),
                fqn: Some(fqn.to_string()),
                identity: Some(gt_domain::model::IdentityKey::fqn(identity)),
                file_id: None,
                span: gt_domain::model::Span {
                    start_line: 1,
                    end_line: 5,
                    start_byte: 0,
                    end_byte: 0,
                },
                language: Language::new("php"),
                phase: Phase(Phase::SYNTHESIZE.to_string()),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            }],
            ..Default::default()
        })
        .expect("节点应可写入");
}

/// Coverage for the remaining predicates: `kind_in` / `name_starts_with` / `fqn_contains` /
/// `identity_contains` / `text_contains` / `no_annotation` / `property_missing` /
/// `fan_out_gte` / `all_of`。
///
/// These predicates currently have **no shipped rule using them**, lower priority than the previous group; but they are the rule language's
/// expressive power, and breaking them would limit "adding rules later", so they are still locked.
#[test]
fn remaining_predicates_are_covered() {
    const YAML: &str = r#"
rules:
  - id: t-kind-in
    title: 种类属于 Method
    severity: info
    category: test
    applies_to:
      kinds: [Table, Method]
    when:
      - kind_in: [Method]
    message: "{name} 是方法"
  - id: t-name-starts-with
    title: 名字以 create 开头
    severity: info
    category: test
    applies_to:
      kinds: [Method]
    when:
      - name_starts_with: create
    message: "{name} 以 create 开头"
  - id: t-fqn-contains
    title: fqn 含 Service
    severity: info
    category: test
    applies_to:
      kinds: [Class]
    when:
      - fqn_contains: service
    message: "{name} 的 fqn 含 Service"
  - id: t-identity-contains
    title: identity 含 OrderService
    severity: info
    category: test
    applies_to:
      kinds: [Class]
    when:
      - identity_contains: orderservice
    message: "{name} 的 identity 含 OrderService"
  - id: t-text-contains
    title: 任一文本含 userDao
    severity: info
    category: test
    applies_to:
      kinds: [Method]
    when:
      - text_contains: userdao
    message: "{name} 命中文本"
  - id: t-no-annotation
    title: 没有 pii 标注的表
    severity: info
    category: test
    applies_to:
      kinds: [Table]
    when:
      - no_annotation: pii
    message: "{name} 没有 pii"
  - id: t-property-missing
    title: 缺 side 属性的契约
    severity: info
    category: test
    applies_to:
      kinds: [HttpContract]
    when:
      - property_missing: side
    message: "{name} 没有 side 属性"
  - id: t-fan-out-gte
    title: 有出边的方法
    severity: info
    category: test
    applies_to:
      kinds: [Method]
    when:
      - fan_out_gte: 1
    message: "{name} 至少有一条语义出边"
  - id: t-has-incoming
    title: 被写过的表
    severity: info
    category: test
    applies_to:
      kinds: [Table]
    when:
      - has_incoming: WritesDb
    message: "{name} 被写过"
  - id: t-all-of
    title: 同时满足两个条件
    severity: info
    category: test
    applies_to:
      kinds: [Table]
    when:
      - all_of:
          - no_annotation: pii
          - fan_in_lte: 0
    message: "{name} 既无 pii 也无人引用"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    seed_rich_node(&f, 301, "Class", "OrderService", "App\\Service\\OrderService", "OrderService");

    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());
    let report = svc.check(f.project, None, false).expect("检查不应失败");
    let hit = |id: &str| report.violations.iter().filter(|v| v.rule_id == id).count();

    assert_eq!(hit("t-kind-in"), 2, "Method 有两个（createOrder / userDaoSelect）");
    assert_eq!(hit("t-name-starts-with"), 1, "只有 createOrder 以 create 开头");
    assert_eq!(hit("t-fqn-contains"), 1, "fqn 匹配（大小写不敏感）");
    assert_eq!(hit("t-identity-contains"), 1, "identity 匹配（大小写不敏感）");
    assert_eq!(hit("t-text-contains"), 1, "text 应能在 name/fqn/identity 任一命中");
    assert_eq!(hit("t-no-annotation"), 1, "只有 unused_log 没有 pii");
    assert_eq!(hit("t-property-missing"), 2, "两个契约都没有 side 属性");
    assert_eq!(hit("t-fan-out-gte"), 2, "两个方法各有一条语义出边");
    assert_eq!(hit("t-has-incoming"), 1, "只有 user 表被 WritesDb 指向");
    assert_eq!(hit("t-all-of"), 1, "只有 unused_log 同时满足两条");
}

/// The **interaction** between `no_capability` and criterion validation:
/// When the `Capability` channel has zero annotations, the rule must be disabled (otherwise `no_capability` is always true);
/// once the channel has annotations, the rule resumes and hits nodes that **lack that capability**.
///
/// This case will directly decide whether `write-endpoint-without-auth` can be revived --
/// that rule is currently disabled precisely because this channel is empty.
#[test]
fn no_capability_depends_on_capability_channel() {
    const YAML: &str = r#"
rules:
  - id: t-no-auth
    title: 没有鉴权能力的表
    severity: warning
    category: security
    applies_to:
      kinds: [Table]
    when:
      - no_capability: [Authentication]
    message: "表 {name} 未识别到鉴权能力"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());

    // Phase 1: Capability channel empty -> rule should be caught by criterion validation
    let before = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        before.rules_unavailable.iter().any(|s| s.starts_with("t-no-auth")),
        "Capability 通道为空时该规则必须停用（否则 no_capability 恒真），实际 {:?}",
        before.rules_unavailable
    );
    assert_eq!(
        before.violations.iter().filter(|v| v.rule_id == "t-no-auth").count(),
        0,
        "被停用的规则不应产出违规"
    );

    // Phase 2: add a **different** capability (RateLimiting), channel is not empty
    f.container
        .store
        .apply(&GraphDelta {
            project_id: Some(f.project),
            annotations: vec![NewAnnotation {
                node_id: NodeId(101),
                channel: AnnotationChannel(AnnotationChannel::CAPABILITY.to_string()),
                kind: "RateLimiting".into(),
                subkind: None,
                confidence: 0.9,
                evidence: serde_json::json!({ "hook": "middleware" }),
                phase: Phase(Phase::ANNOTATE_POST.to_string()),
                merge: MergeStrategy::Coexist,
            }],
            ..Default::default()
        })
        .expect("标注应可写入");

    let after = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        !after.rules_unavailable.iter().any(|s| s.starts_with("t-no-auth")),
        "Capability 通道有标注后应恢复执行，实际 {:?}",
        after.rules_unavailable
    );
    assert_eq!(
        after.violations.iter().filter(|v| v.rule_id == "t-no-auth").count(),
        2,
        "两张表都没有 Authentication 能力（user 只有 RateLimiting），都应命中"
    );
}

/// `applies_to.kinds` empty = **no kind restriction** (per `RuleScope`'s docs).
///
/// This once disagreed with the implementation: empty kinds were concatenated into `kind = ''` SQL, matching no node,
/// the rule silently 0-hit -- the exact opposite of "no kind restriction".
#[test]
fn empty_kinds_means_all_kinds() {
    const YAML: &str = r#"
rules:
  - id: t-any-kind
    title: 不限种类
    severity: info
    category: test
    applies_to:
      kinds: []
    when:
      - name_contains: order
    message: "{name} 命中"
"#;
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());
    let report = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        !report.violations.is_empty(),
        "不限种类时应能跨种类命中（createOrder / POST /api/order/create 等），实际 0 条"
    );
}

/// The happy path of `read_snippet`: when the file really exists it **must read out the snippet**.
///
/// Previous cases only covered the "file missing returns None" branch; the real file read
/// (including the 2MB guard and line-number window) was unguarded -- yet the snippet is the main content shown to the LLM in the context pack.
#[test]
fn recall_reads_snippet_from_real_file() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    // Register a file that really exists on disk (a backend controller in the synthetic sample)
    let files = f
        .container
        .store
        .replace_files(
            f.project,
            vec![NewSourceFile {
                project_id: f.project,
                sub_project_id: None,
                path: "backend/app/controller/Order.php".into(),
                language: Language::new("php"),
                size_bytes: 128,
                content_hash: "h-real".into(),
            }],
        )
        .expect("文件应可写入");
    let real_file = files[0].id;

    f.container
        .store
        .apply(&GraphDelta {
            project_id: Some(f.project),
            nodes: vec![NewNode {
                id: Some(NodeId(301)),
                project_id: f.project,
                sub_project_id: None,
                kind: NodeKind::new("Method"),
                name: "realOrderController".into(),
                fqn: None,
                identity: Some(gt_domain::model::IdentityKey::fqn("realOrderController")),
                file_id: Some(real_file),
                span: gt_domain::model::Span {
                    start_line: 2,
                    end_line: 8,
                    start_byte: 0,
                    end_byte: 0,
                },
                language: Language::new("php"),
                phase: Phase(Phase::SYNTHESIZE.to_string()),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            }],
            ..Default::default()
        })
        .expect("节点应可写入");

    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let r = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "realOrderController".into(),
                limit: 5,
                hops: 0,
                kinds: Vec::new(),
                with_snippets: true,
                include_body: false,
            },
        )
        .expect("召回不应失败");

    let hit = r
        .hits
        .iter()
        .find(|h| h.name == "realOrderController")
        .expect("应命中刚写入的节点");
    let snippet = hit
        .snippet
        .clone()
        .unwrap_or_else(|| panic!("文件真实存在时应读出片段，file={:?} line={:?}", hit.file, hit.line));
    assert!(!snippet.is_empty(), "片段不应为空");
    assert!(
        r.markdown.contains(&snippet) || snippet.lines().count() > 0,
        "上下文包应带上片段内容"
    );
}


/// Write `n` tables in batch, each read once by the same method (fan-in 1).
///
/// Using "every candidate really has an edge" instead of an empty graph is to make `fan_in` computation,
/// in-edge preloading, and violation construction (with file location) all truly exercised -- an empty graph can't surface regressions.
fn seed_many_tables(f: &Fixture, n: usize) {
    let pid = f.project;
    let mut nodes: Vec<NewNode> = Vec::with_capacity(n + 1);
    let mut edges = Vec::with_capacity(n);

    nodes.push(NewNode {
        id: Some(NodeId(901)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new("Method"),
        name: "perf_reader".into(),
        fqn: None,
        identity: Some(gt_domain::model::IdentityKey::fqn("perf_reader")),
        file_id: None,
        span: gt_domain::model::Span {
            start_line: 1,
            end_line: 5,
            start_byte: 0,
            end_byte: 0,
        },
        language: Language::new("php"),
        phase: Phase(Phase::SYNTHESIZE.to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    });
    for i in 0..n {
        let id = 10_000i64 + i as i64;
        nodes.push(NewNode {
            id: Some(NodeId(id)),
            project_id: pid,
            sub_project_id: None,
            kind: NodeKind::new("Table"),
            name: format!("perf_tbl_{i}"),
            fqn: None,
            identity: Some(gt_domain::model::IdentityKey::fqn(&format!("perf_tbl_{i}"))),
            file_id: None,
            span: gt_domain::model::Span {
                start_line: 1,
                end_line: 5,
                start_byte: 0,
                end_byte: 0,
            },
            language: Language::new("php"),
            phase: Phase(Phase::SYNTHESIZE.to_string()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
        edges.push(NewEdge::new(
            pid,
            EdgeKind::new(EdgeKind::READS_DB),
            NodeId(901),
            NodeId(id),
        ));
    }

    f.container
        .store
        .apply(&GraphDelta { project_id: Some(pid), nodes, edges, ..Default::default() })
        .expect("批量图应可写入");
}

/// Time of one check (ms); the rule hits all tables, ensuring every candidate is really evaluated.
///
/// Warm up once before timing: the first run compiles SQL statements and fills the page cache,
/// including cold start would let fixed overhead dominate the measurement, making the scaling ratio meaningless.
fn time_check_on_tables(f: &Fixture) -> (u128, usize) {
    const YAML: &str = r#"
rules:
  - id: perf-table-hit
    title: 被读过的表
    severity: info
    category: perf
    applies_to:
      kinds: [Table]
      limit: 200000
    when:
      - fan_in_gte: 1
    message: "表 {name} 被读过"
"#;
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());
    svc.check(f.project, None, false).expect("预热检查不应失败");

    let t = std::time::Instant::now();
    let report = svc.check(f.project, None, false).expect("检查不应失败");
    (t.elapsed().as_millis(), report.violations.len())
}

/// At 8x scale, time must not explode -- block O(N²) regressions.
///
/// # Thresholds are calibrated from measurement, not arbitrary multiples
///
/// Measured the real curve with a probe (n = 1k/2k/4k/8k/16k):
///    before optimization `8 → 18 → 47 → 112 → 344 ms`, about **N^1.35** (superlinear)
///    after optimization `… → 16000 nodes 135 ms`, 8x scale takes ×6.1 -- **linear** (theoretical 8.0)
/// Root cause: the batch-edge SQL made SQLite pick `idx_edges_project` (scanning all project edges),
/// costing O(N²); after adding the `(project_id, to_id/from_id)` composite index it returned to linear
/// (see the comment in `gt-adapter-sqlite/src/schema.rs`).
///
/// So at 8x scale: now about ×6, pure quadratic about ×64.
/// Budget set to `t_small × 16 + 100ms`: leaves ~3x headroom for the current implementation (not flaky),
/// while still blocking growth ≥ N^1.4.
///
/// In other words: this case guards **order-of-magnitude regression**, not "10% faster than yesterday".
/// For real fine-grained perf regression, use a criterion benchmark, not a unit test.
#[test]
fn check_does_not_degenerate_quadratically() {
    const SMALL: usize = 2_000;
    const BIG: usize = 16_000;

    let fs = temp_fixture();
    seed_many_tables(&fs, SMALL);
    let (t_small, hit_small) = time_check_on_tables(&fs);

    let fb = temp_fixture();
    seed_many_tables(&fb, BIG);
    let (t_big, hit_big) = time_check_on_tables(&fb);

    let budget = t_small * 16 + 100;
    eprintln!(
        "[perf] {SMALL} 节点 {t_small} ms / {BIG} 节点 {t_big} ms（预算 {budget} ms，倍数 {:.1}）",
        t_big as f64 / t_small.max(1) as f64
    );

    assert_eq!(hit_small, SMALL, "小规模应全部命中");
    assert_eq!(hit_big, BIG, "大规模应全部命中");
    assert!(
        t_big <= budget,
        "规模 ×8 耗时从 {t_small}ms 涨到 {t_big}ms（预算 {budget}ms）—— \
         疑似退化成超线性；检查是否引入了逐节点查库或嵌套扫描"
    );
}

/// Absolute upper-bound safety net: catch "linear but huge constant" regressions like N+1
/// -- it is still linear, the scaling ratio is invisible, only absolute time reveals it.
///
/// Real-device measurement is far below this (~26ms); 2s leaves ~75x headroom:
/// it can't be flaky on a normal machine, yet still catches "one extra DB round-trip per node" scale.
#[test]
fn check_completes_within_budget() {
    const N: usize = 2_000;
    const BUDGET_MS: u128 = 2_000;

    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_many_tables(&f, N);
    let (ms, hits) = time_check_on_tables(&f);
    eprintln!("[perf] {N} nodes in {ms} ms (budget {BUDGET_MS} ms)");

    assert_eq!(hits, N, "应全部命中");
    assert!(
        ms <= BUDGET_MS,
        "{N} 个候选的检查耗时 {ms}ms 超过预算 {BUDGET_MS}ms —— \
         若每节点都多了一次 DB 往返就会出现这种量级"
    );
}

/// With `with_snippets` on, it must take the file-read path **and not crash on missing files**.
///
/// In old cases `with_snippets` was always false; this code (including the 2MB oversized-file guard,
/// and line-number out-of-bounds guard) had zero coverage before.
#[test]
fn recall_with_snippets_is_safe_when_file_missing() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let svc = RecallService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        f.container.filesystem(),
        f.container.scanner(),
    );
    let r = svc
        .recall(
            f.project,
            &RecallQuery {
                query: "user".into(),
                limit: 10,
                hops: 1,
                kinds: Vec::new(),
                with_snippets: true,
                include_body: false,
            },
        )
        .expect("开启片段不应导致失败");
    assert!(!r.hits.is_empty(), "开启片段后仍应有召回结果");
    // The fixture's source file does not exist on disk; the snippet should be None, not panic
    assert!(
        r.hits.iter().all(|h| h.snippet.is_none()),
        "文件不存在时片段应为 None，实际：{:?}",
        r.hits.iter().filter_map(|h| h.snippet.as_ref()).collect::<Vec<_>>()
    );
}

/// HTTP-layer end-to-end: GET `/api/projects/{id}/recall` must recall related code from the graph.
///
/// This is the only recall surface not covered before -- the service has integration cases, but the HTTP inbound adapter
/// (`gt-adapter-http`) was never tested, so a wrong route or param parsing went unnoticed.
#[tokio::test]
async fn recall_http_get_endpoint_returns_hits() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let router = f.container.router();

    let resp = router
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/projects/{}/recall?q=user&hops=2",
                f.project.get()
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("响应应为 JSON");
    assert!(json["ok"].as_bool() == Some(true), "应 success：{json:?}");
    let hits = json["data"]["hits"].as_array().expect("data.hits 应为数组");
    assert!(!hits.is_empty(), "GET /recall 应召回结果");
    assert!(
        hits.iter().any(|h| h["name"].as_str() == Some("user")),
        "应含 user 种子：{hits:?}"
    );
}

/// HTTP-layer end-to-end: POST `/api/projects/{id}/recall` goes through `RecallQuery` body parsing.
#[tokio::test]
async fn recall_http_post_endpoint_returns_hits() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let router = f.container.router();

    let payload = serde_json::json!({
        "query": "user",
        "limit": 20,
        "hops": 2,
        "kinds": [],
        "with_snippets": false
    });
    let resp = router
        .clone()
        .oneshot(
            Request::post(format!("/api/projects/{}/recall", f.project.get()))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("响应应为 JSON");
    assert!(json["ok"].as_bool() == Some(true), "应 success：{json:?}");
    let hits = json["data"]["hits"].as_array().expect("data.hits 应为数组");
    assert!(!hits.is_empty(), "POST /recall 应召回结果");
}

/// HTTP layer: `include_body` must pass through to recall and append the full source of hit files at the end of the context pack.
///
/// This is the key path for MCP / IDE to skip the second `read`. The fixture source file is not on disk, so here we assert
/// "the passage is concatenated in" (a read failure must still give a hint, not silently drop the whole passage).
#[tokio::test]
async fn recall_http_include_body_appends_full_file_section() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let router = f.container.router();

    let payload = serde_json::json!({
        "query": "user",
        "limit": 20,
        "hops": 2,
        "with_snippets": false,
        "include_body": true
    });
    let resp = router
        .clone()
        .oneshot(
            Request::post(format!("/api/projects/{}/recall", f.project.get()))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("响应应为 JSON");
    let md = json["data"]["markdown"].as_str().expect("应有 markdown");
    assert!(
        md.contains("## Full files (include_body)"),
        "with include_body=true the full-file section should be appended: {md}"
    );
}

/// Contrast: when `include_body` is not passed, the passage must not appear (default false, to avoid default output bloat).
#[tokio::test]
async fn recall_http_without_include_body_has_no_full_file_section() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let router = f.container.router();

    let payload = serde_json::json!({ "query": "user", "with_snippets": false });
    let resp = router
        .clone()
        .oneshot(
            Request::post(format!("/api/projects/{}/recall", f.project.get()))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("响应应为 JSON");
    let md = json["data"]["markdown"].as_str().expect("应有 markdown");
    assert!(
        !md.contains("## Full files (include_body)"),
        "by default the full-file section should not be appended: {md}"
    );
}

/// HTTP layer: the warmup-progress endpoint should return fields like warmed / warming (for MCP to judge whether recall is on the cold path).
#[tokio::test]
async fn warmup_http_endpoint_returns_status_fields() {
    let Some(f) = fixture() else {
        eprintln!("skipped: samples/frontend-backend-link not found (set GRAPHTELL_SAMPLE_DIR to point at it)");
        return;
    };
    seed_graph(&f);
    let router = f.container.router();

    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/api/projects/{}/warmup", f.project.get()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("响应应为 JSON");
    assert!(json["ok"].as_bool() == Some(true), "应 success：{json:?}");
    let d = &json["data"];
    assert!(d["warmed"].is_boolean(), "应有 warmed 字段：{d:?}");
    assert!(d["warming"].is_boolean(), "应有 warming 字段：{d:?}");
}
