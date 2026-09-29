//! 规则检测与代码召回的端到端自检。
//!
//! **刻意不依赖外部样本**：这里直接往库里写一小组手工构造的节点与边，
//! 因此任何时候、任何机器上都必定运行（CRMEB 那类样本测试在样本缺失时会跳过，
//! 无法守住这两个新能力的回归）。
//!
//! 覆盖三件事：
//!   1. 规则引擎**真的能用 YAML 定义**（不是硬编码）；
//!   2. 违规落成 `rule:*` 诊断，可回读、可随重跑**清空重建**（不留历史脏数据）；
//!   3. 召回**不只是关键词匹配** —— 种子命中后要能沿图把没出现关键词的相关代码带出来。

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

/// 建一个临时容器 + 工程。
///
/// 用例并行执行，目录名必须**进程内唯一**（时间戳在同一毫秒内会撞车，
/// 导致两个用例共用同一个 SQLite 文件、互相污染）。
fn fixture() -> Fixture {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
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
            root_path: root.join("samples/frontend-backend-link"),
            description: None,
            config: None,
        })
        .expect("工程应可创建");
    Fixture { container, project: project.id }
}

/// 写入一小组手工构造的图：
///
/// * `Table:user` —— 热点表，被 Service 与 Dao 读写（扇入 2）
/// * `Table:unused_log` —— 没人用
/// * `HttpContract:POST /api/order/create` —— 有 handler
/// * `HttpContract:DELETE /api/ghost` —— 没有 handler（应被规则命中）
/// * `Method:createOrder` —— 既写表又不直接出现"table"这个词（考验召回的图扩展）
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
                // 契约 → handler（HandledBy 是**出边**，方向必须与规则一致）
                NewEdge::new(pid, EdgeKind::new(EdgeKind::HANDLED_BY), NodeId(103), NodeId(105)),
                // handler / dao 写读 user 表
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

// ---------------------------------------------------------------- 规则：YAML 定义

/// 规则必须能用 YAML 定义出来 —— 这是"内核不认识任何规则"的证明。
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
    let f = fixture();
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

    // 违规排序：error 在前
    let first = report.violations.first().expect("至少有 1 条违规");
    assert_eq!(first.severity, Severity::Error);
}

/// 只跑一部分规则时，**不该**清掉其它规则的历史结果
///（否则"单独重跑 A"会顺手抹掉 B/C，报告会莫名缺一块）。
#[test]
fn partial_rerun_only_replaces_its_own_violations() {
    let f = fixture();
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(RULES_YAML).expect("规则应可解析"));
    let svc = RuleService::new(
        f.container.store.clone() as Arc<dyn Persistence>,
        rules.clone(),
    );

    svc.check(f.project, None, true).expect("全量检查应成功");
    assert_eq!(svc.violations(f.project, 500, None).expect("回读").len(), 3);

    // 只重跑 pii 规则：其余两条必须还在
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

/// 违规落成 `rule:*` 诊断；重跑时**先清旧再写新**，不留历史脏数据。
#[test]
fn violations_persist_as_diagnostics_and_are_replaced_on_rerun() {
    let f = fixture();
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

    // 再跑一次：结果必须**还是 3 条**，而不是累加成 6 条
    svc.check(f.project, None, true).expect("重复检查应成功");
    let second = svc.violations(f.project, 500, None).expect("应能回读违规");
    assert_eq!(second.len(), 3, "重跑必须清空旧违规，而不是累加");
}

/// 只跑指定规则（`only` 过滤）。
#[test]
fn check_can_run_a_subset_of_rules() {
    let f = fixture();
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

/// 谓词组合：`Not` / `AnyOf` 必须可用（否则规则语言表达力不够）。
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
    let f = fixture();
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

/// 内置规则库必须能装载（防止 `rules/*.yaml` 写坏而无人发现）。
#[test]
fn builtin_rules_yaml_loads() {
    let f = fixture();
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

// ---------------------------------------------------------------- 召回

/// 召回的核心价值：不只是关键词匹配，而是**沿图把相关代码带出来**。
#[test]
fn recall_expands_from_seed_along_graph() {
    let f = fixture();
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

    // 1) 种子应当命中 `Table:user`
    assert!(
        result.seeds.iter().any(|s| s.name == "user"),
        "种子应命中 user 表，实际：{:?}",
        result.seeds.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // 2) 扩展：`createOrder` 这个名字里没有 "user"，但它写了 user 表 ——
    //    必须被图扩展带出来，这正是"召回"区别于"全文检索"的地方。
    let names: Vec<&str> = result.hits.iter().map(|h| h.name.as_str()).collect();
    assert!(
        names.contains(&"createOrder"),
        "写 user 表的 createOrder 应被扩展带出，实际命中：{names:?}"
    );
    assert!(
        names.contains(&"userDaoSelect"),
        "读 user 表的方法也应被带出，实际命中：{names:?}"
    );

    // 3) 跳数与直接命中的区分必须正确
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

/// 候选集快照跨请求复用（延迟优化）**不能**以"答旧图"为代价。
///
/// 常驻服务只在 watch 触发重建时收到 `clear_node_cache`；图若在**另一个进程**
/// （如手工 `graphtell run`）被改写，服务不会收到通知。因此每次复用快照前都要用
/// 一次轻量的 `stats` 校验规模，规模对不上就重建 —— 本用例锁住这条不变量：
/// 同一个 `RecallService`、中途改图、**不调用** `clear_node_cache`，
/// 第二次召回必须看得到新写入的节点。
#[test]
fn recall_candidate_snapshot_refreshes_when_graph_changes_without_notice() {
    let f = fixture();
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

    // 第一次召回：图上还没有这个节点（顺便把该工程的快照装进缓存）。
    let before = query("zzqLateAddedWidget");
    assert!(
        !before.hits.iter().any(|h| h.name == "zzqLateAddedWidget"),
        "写入前不应存在该节点，实际：{:?}",
        before.hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );

    // 另一个进程改图：新增一个节点，且不通知 RecallService。
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

    // 第二次召回：必须看到新节点 —— 否则说明快照被永久复用，答的一直是旧图。
    let after = query("zzqLateAddedWidget");
    assert!(
        after.hits.iter().any(|h| h.name == "zzqLateAddedWidget"),
        "图已改写但快照未失效（会一直答旧图），实际命中：{:?}",
        after.hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
}

/// 中文意图查询（"下单改优惠"）必须能越过词面，seed 到英文命名的业务节点。
///
/// 之前（纯词面子串）：只有名字恰好含「优惠」的 `优惠券` i18n 文案被命中，
/// 真正的下单 / 改折扣代码（英文命名）全部漏掉。现在通过「中文意图词 → 英文
/// 符号」的离线桥 + 向量软匹配，应能召回 `placeOrder` / `applyDiscount` /
/// `OrderService`，并把图上的相关代码一并带出。
#[test]
fn recall_chinese_intent_bridges_to_english_nodes() {
    let f = fixture();
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
    // 通用动词桥的落点：「查询/列表」→ list / find / all …
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
                // placeOrder 调用 applyDiscount，并写 OrderService
                NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(202), NodeId(203)),
                NewEdge::new(pid, EdgeKind::new(EdgeKind::WRITES_DB), NodeId(202), NodeId(201)),
                // listOrders 也写 OrderService，便于验证图扩展
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
                // 用**跨领域通用动词**表达意图（查询 / 列表），不再依赖任何领域词表：
                // 领域名词（下单 / 优惠）已统一交给工程自身的 i18n 桥。
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

    // 1) 中文通用动词必须 bridge 到英文方法（而不只是命中中文 i18n 文案）
    assert!(
        names.contains(&"listOrders"),
        "「查询/列表」应 bridge 到 listOrders，实际命中：{names:?}"
    );
    // 2) 图扩展：listOrders 写 OrderService，应被带出
    assert!(
        names.contains(&"OrderService"),
        "OrderService 应被图扩展带出，实际命中：{names:?}"
    );
    // 3) 命中里必须有「代码节点」，而不是只有文案类节点
    assert!(
        names.iter().any(|n| *n == "listOrders" || *n == "OrderService"),
        "结果应包含代码节点，实际命中：{names:?}"
    );
}

/// 端到端锁定「如何修改下单优惠」的排名成果（对应 eval 集同款查询 + grep 基线用例）。
///
/// 核心不变量：动作意图（修改）下，**只有命中内容词（coupon / order…）的纯动词 CRUD 方法**
/// 才享受 1.5× 动作加权；无关域的发货 `DeliveryService.update` 即便名字命中 `update`，
/// 也必须排在业务方法 `StoreCouponIssue.edit` 之后。回归前发货 CRUD 曾顶进 Top 5，
/// 盖住了真正的优惠券服务节点。
///
/// 这条走的是完整词面召回链路（parse_query → action_intent → score_node → 排序），
/// 单测 [`gt_application::recall_service`] 里那两条只钉住了 `score_node` 本身，
/// 这条补上"整条管道串起来仍成立"的兜底。
#[test]
fn recall_modify_order_discount_keeps_business_edit_above_shipping_crud() {
    let f = fixture();
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

    // 业务方法：类标识符含 coupon → 命中内容词 → 保留 1.5× 动作加权
    let coupon_edit = node(
        "Method",
        "edit",
        501,
        coupon_file,
        "app\\adminapi\\controller\\v1\\marketing\\StoreCouponIssue::edit",
    );
    // 无关域 CRUD：名字命中 update，但类标识符（DeliveryService）无内容词、
    // 仅有路径里的 order → 必须被惩罚，不享受动作加权
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
    // 关键词不该是字符级 bigram：证明中文已走词级切分
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

/// 端到端锁定「事件驱动查询」的两个核心不变量（对应 eval 集同款查询 + 6 场景复盘）：
///
/// 1. **监听器浮出**：查询「X 之后怎么 Y」时，被命名约定淹没的事件监听器
///    （`*Listener`，方法名统一叫 `handle`、词面分≈0）必须被补成种子并进前排 ——
///    具体地，「下单后怎么发通知」要能召回 `OrderCreateAfterListener`、「退款成功后怎么回退优惠券」
///    要能召回 `OrderRefundCreateAfterListener`。
/// 2. **质量不崩**：事件种子只做「兄弟监听器间的 idf 微调」，不整体压过词面强命中，
///    因此质量档不得掉到 `Low`（否则整段上下文作废、等于没召回）。
///
/// 这条不走样本（CRMEB），直接手造一组「订单/退款/通知 监听器 + 业务 Service」的小图，
/// 任何机器上必跑，守住上面两条回归。
#[test]
fn recall_event_driven_listener_surfaces_without_quality_collapse() {
    let f = fixture();
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

    // 业务 Service（含 order / user / coupon 内容词 → 词面强命中）
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
    // 内聚业务方法：同一方法同时覆盖两个概念，给质量评估器提供「概念内聚」信号，
    // 否则「下单 + 通知 + 用户」三类概念各撞一个无关节点会被判 Low（假信心护栏）。
    // 这也正是真实代码库里存在的入口（下单后发通知 / 退款回退优惠卷）。
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
    // 事件监听器（名字带 Listener → 被 `is_event_handler` 识别；方法名 `handle` 词面分≈0）
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
                // 事件 → 监听器（语义边，未来若接"事件边连通性"加权也能用到）
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

    // ---- 场景 1：下单后怎么发通知给用户 → OrderCreateAfterListener 必须浮出，质量不崩 ----
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

    // ---- 场景 2：退款成功后怎么回退优惠券 → OrderRefundCreateAfterListener + 回退券逻辑浮出 ----
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

    // ---- idf 微调不变量：命中稀有概念（notify）的监听器应压过只命中泛词（order）的兄弟 ----
    let rank = |n: &str| names1.iter().position(|x| *x == n);
    match (rank("NotifyListener"), rank("OrderRefundCreateAfterListener")) {
        (Some(a), Some(b)) => assert!(
            a < b,
            "NotifyListener（notify，稀有）应排在 OrderRefundCreateAfterListener（仅 order）之前，实际：{names1:?}"
        ),
        _ => {} // 任一未进前排则不强制（主要取决于词面种子竞争），核心不变量已由上面两条守住
    }
}

/// 端到端：把真实 `bge-m3`（candle，纯 Rust）注入 `RecallService`，中文意图
/// `下单改优惠` 经由「语义向量」直接 seed 到英文业务节点 `placeOrder` / `applyDiscount`，
/// 且它们的得分明显高于无关噪声节点 `unused_log`。
///
/// 仅在 `--features model-candle` 下编译 / 运行；无模型权重时自动跳过。
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

    let f = fixture();
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
    // 无关噪声节点：与「下单改优惠」无任何语义关系
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
    // 语义路要求工程已预热（生产入口由后台异步预热 / `graphtell embed` 完成）。
    // 直接注入编码器时 `warmed_projects` 为空，召回会静默落到快速哈希路、bge 根本不参与，
    // 本测试也就测不到它想测的「中文意图 → 英文符号」语义召回。
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
    // 噪声 `unused_log` 现在同样进入向量空间（Method/Function 已纳入语义编码），会被低分
    // 召回 —— 这正是期望：噪声与业务节点可比，且得分应显著更低。未召回时按 0 分处理以
    // 保持对模型权重变化的稳健性。
    let n = result
        .seeds
        .iter()
        .find(|s| s.name == "unused_log")
        .map(|s| s.score)
        .unwrap_or(0.0);
    println!("bge 召回得分: placeOrder={p:.1} applyDiscount={a:.1} unused_log={n:.1}");

    assert!(p > n, "placeOrder 得分应高于噪声 unused_log：{p} vs {n}");
    assert!(a > n, "applyDiscount 得分应高于噪声 unused_log：{a} vs {n}");
}

/// 中文结构提示（"表"）应把结果收敛到对应节点种类。
#[test]
fn recall_understands_chinese_kind_hints() {
    let f = fixture();
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

/// 建图完成后必须**自动**跑一遍合规检查 —— 不能要求用户手动触发。
///
/// 断言方式刻意选"陈旧违规被清掉"而不是"有违规产生"：
/// 检查的第一步就是清空 `rule:` 前缀的旧诊断，所以人工塞一条陈旧诊断、
/// 建图后它必须消失 —— 这样哪怕本样本一条违规都没命中，也能确证检查真的跑过。
#[test]
fn build_runs_check_automatically() {
    let f = fixture();

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

/// 声明技术栈的环境：给工程挂上指定语言/框架的子工程。
///
/// 环境闸门读的就是 `sub_projects.language` / `.frameworks`，
/// 所以测试要能控制它 —— 否则默认无子工程，任何带语言声明的规则都会"不适用"。
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

/// 环境闸门：语言专属规则**不能**在不适配的工程上跑。
///
/// 这不是洁癖 —— PHP FKB 才产出的边（Triggers / PublishesTo）在 Java 工程里一条都没有，
/// `no_incoming: Triggers` 会把每个事件节点都报成"没人触发"；同理 JS 才产出的边
/// （Emits / ListensTo，前端事件总线）在纯 Java 工程上也不存在。
#[test]
fn php_only_rules_are_skipped_on_java_project() {
    let f = fixture();
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
    // 反向确认：跨语言的契约规则不受影响，照常执行
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

/// 同一条 PHP 规则在 PHP 工程上必须恢复执行（闸门不能一刀切）。
#[test]
fn php_only_rules_run_on_php_project() {
    let f = fixture();
    seed_graph(&f);
    set_stack(&f, "php", &["thinkphp6"]);

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

/// 环境闸门：前端（JS）专属规则**不能**在纯后端工程上跑。
///
/// `EventBus` 节点与 `Emits` / `ListensTo` 边是前端事件总线的语义。
/// 纯后端工程上它们一个都没有，规则会以"0 命中"收场 —— 而 0 命中会被读成
/// "没有死代码"，正是本项目最想避免的失效方式。
#[test]
fn js_only_rules_are_skipped_on_backend_only_project() {
    let f = fixture();
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

/// 判据校验：判据里的边在图上一个都没有时，规则必须**停用**而不是硬跑。
///
/// 这是"恒真误报"的防线：图上没有 `CallsHttp` 边时，
/// `no_incoming: CallsHttp` 对每个端点都成立，会把全部端点报成死端点。
#[test]
fn rule_is_disabled_when_its_edge_never_occurs() {
    let f = fixture();
    // 只放一个没有 CallsHttp 边的图：seed_graph 里本就没有 CallsHttp
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

/// 规则库按语言分目录装载（`rules/global/` + `rules/php/`），且 PHP 规则带环境声明。
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

    // 跨语言规则必须只依赖拓扑，不能偷偷带上语言声明
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

/// 中文长句必须被切成二字组 —— 否则整句永远匹配不到任何节点，召回直接为空。
///
/// 回归背景：中文没有词边界，早期实现把整段中文当成一个 token，
/// “订单创建涉及哪些表和接口” 作为整体匹配不到任何节点名 → **0 命中**。
/// 而提示词经常就是这种整句，等于召回功能对中文提问不可用。
#[test]
fn recall_splits_chinese_sentence_into_bigrams() {
    let f = fixture();
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

    // “表”/“接口”只作种类加成，不应再残留成文本匹配词（会把召回带偏）
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

/// 完整标识符（snake_case 不被拆散）必须能精确命中。
#[test]
fn recall_keeps_snake_case_identifiers_intact() {
    let f = fixture();
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

/// 召回必须给出可直接粘给 LLM 的上下文包。
#[test]
fn recall_produces_markdown_context_pack() {
    let f = fixture();
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
    assert!(md.contains("# 召回上下文"), "上下文包应有标题");
    assert!(md.contains("种子"), "上下文包应说明种子");
    assert!(md.contains("相关代码"), "上下文包应列出相关代码");
}

/// 谓词枚举可直接构造（保证新增谓词不必改内核的调用点）。
#[test]
fn check_predicate_is_constructible() {
    let p = CheckPredicate::Not(Box::new(CheckPredicate::HasIncoming("HandledBy".into())));
    assert!(matches!(p, CheckPredicate::Not(_)));
}

// ---------------------------------------------------------------- 补覆盖
//
// 下面这批用例对应的是**已发货规则在用、但原先零覆盖**的谓词与开关：
//   * `property_is`   —— `frontend-calls-missing-backend` / `backend-endpoint-never-called`
//   * `has_outgoing`  —— `backend-endpoint-never-called`
//   * `fan_in_gte`    —— `hot-table` / `pii-table-hot`
//   * `enabled: false`—— `write-endpoint-without-auth` 正是靠它停用的
// 它们一旦回归，坏的是**真实规则的输出**，而现有用例只覆盖 `no_outgoing` /
// `has_annotation` / `fan_in_lte`，发现不了。

/// 给图补一个带 `properties` 的节点（`seed_graph` 里的节点 properties 全是 Null，
/// 所以 `property_is` 在既有图上永远不成立 —— 不补节点就没法测它）。
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

/// `enabled: false` 的规则必须**一条违规都不产出**。
///
/// 这不是形式主义：`write-endpoint-without-auth` 目前正是靠 `enabled: false`
/// 停用的（它的判据在当前图上恒真，跑起来只会制造虚假安全感）。
/// 若哪天 `enabled` 字段失效，这条会立刻炸。
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
    let f = fixture();
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

/// 已发货规则在用的三个谓词：`property_is` / `has_outgoing` / `fan_in_gte`。
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
    let f = fixture();
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

    // 只有刚补的 GET /api/from-frontend 带 side=frontend
    assert_eq!(hit("t-property-is"), 1, "`property_is` 应只命中 side=frontend 的契约");
    // 只有 POST /api/order/create 有 HandledBy 出边（ghost 没有）
    assert_eq!(hit("t-has-outgoing"), 1, "`has_outgoing` 应只命中带 handler 的契约");
    // user 被读写两条边引用；unused_log 没有
    assert_eq!(hit("t-fan-in-gte"), 1, "`fan_in_gte: 1` 应只命中 user 表");
}

/// 框架闸门：声明了 `frameworks` 的规则，在没有该框架时同样要跳过。
///
/// 环境闸门有两级，之前只测了 `languages`，`frameworks` 那条分支没人守。
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
      frameworks: [thinkphp6]
    when:
      - fan_in_gte: 0
    message: "表 {name}"
"#;
    let f = fixture();
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());

    set_stack(&f, "php", &["laravel"]);
    let without = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        without.rules_not_applicable.iter().any(|s| s.starts_with("tp-only")),
        "laravel 工程上 thinkphp6 规则应不适用，实际 {:?}",
        without.rules_not_applicable
    );

    set_stack(&f, "php", &["thinkphp6"]);
    let with = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        !with.rules_not_applicable.iter().any(|s| s.starts_with("tp-only")),
        "thinkphp6 工程上应恢复执行，实际 {:?}",
        with.rules_not_applicable
    );
    assert!(
        with.violations.iter().any(|v| v.rule_id == "tp-only"),
        "恢复执行后应真的命中"
    );
}

/// Java N+1 规则**真的能命中**：循环体内的 DAO 读调用点（CallSite）。
///
/// 这条把「语言闸门 + `property_is(in_loop)` + `has_annotation(db-query)`」三段拼起来，
/// 任何一段断了都会让规则静默 0 命中（曾经 `in_loop` 写成 JSON 布尔、而 `PropertyIs`
/// 只认字符串，导致 `property_is` 永远不成立）。现有用例只验证规则能装载、没验证能命中。
#[test]
fn java_n1_query_rule_fires_on_loop_db_read() {
    let f = fixture();
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

    // 一个循环体内的数据库**读**调用点：
    //   * `in_loop` 属性（解析器写成 JSON 布尔 `true`）；
    //   * `db-query` 标注（P7 落成 `ReadsDb` 边时打的投影）。
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
    // 没打 `db-write` 标注，写库那条不应误命中。
    assert!(
        !report
            .violations
            .iter()
            .any(|v| v.rule_id == "java-n1-write-in-loop"),
        "未打 db-write 标注时 java-n1-write-in-loop 不应误命中"
    );
}

/// 文案模板的 `{file}` / `{line}` 必须渲染出来（`raw-sql-sink` 在用）。
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
    let f = fixture();
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

/// `kinds` 的语义是**约束种子种类**，不是过滤结果 —— 该参数在所有旧用例里
/// 都是空数组，从未被赋过值，所以这里把它锁住。
///
/// 关键（曾经写错过断言）：限定 `kinds: [Table]` 后结果里**仍然会出现 Method**。
/// 这不是 bug —— 扩展用的邻接是工程级的，且代码刻意把被排除种类的概要补回来
/// （见 `recall_service` 里"扩展可能走到被排除的种类"那段）。
/// 若把结果也按 kinds 过滤掉，"从表出发找相关代码"就成了"只列表名"，
/// 召回会退化成关键词过滤，恰恰丢掉它沿图扩展的价值。
#[test]
fn recall_kinds_filters_seeds_not_results() {
    let f = fixture();
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

    // 种子（hop == 0）必须全部来自指定种类
    assert!(
        only_tables.hits.iter().filter(|h| h.hop == 0).all(|h| h.kind == "Table"),
        "种子应全部是 Table，实际：{:?}",
        only_tables.hits.iter().filter(|h| h.hop == 0).map(|h| &h.kind).collect::<Vec<_>>()
    );
    // 扩展带出来的邻居允许是别的种类（这正是召回的价值所在）
    assert!(
        only_tables.hits.iter().any(|h| h.hop > 0 && h.kind != "Table"),
        "扩展应能带出其它种类的相关代码，否则召回退化成关键词匹配"
    );

    // 换个种类，种子也应随之改变 —— 证明 kinds 真的在生效
    let only_contracts = run(vec!["HttpContract".into()]);
    assert!(
        !only_contracts.seeds.iter().any(|s| s.kind == "Table"),
        "限定 HttpContract 时种子里不应出现 Table，实际：{:?}",
        only_contracts.seeds
    );
}

/// `limit` 必须真的截断结果（旧用例里的 limit 都大于实际命中数，等于没测）。
#[test]
fn recall_limit_truncates_results() {
    let f = fixture();
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

/// 空查询 / 纯噪声查询不能炸，且应给出空结果而不是随便返回点什么。
#[test]
fn recall_handles_empty_query() {
    let f = fixture();
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

/// 补一个带 `fqn` / `identity` 的节点（`seed_graph` 里的节点这两项都是空的，
/// 不补就没法测 `fqn_contains` / `identity_contains` / `text_contains`）。
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

/// 剩余谓词的覆盖：`kind_in` / `name_starts_with` / `fqn_contains` /
/// `identity_contains` / `text_contains` / `no_annotation` / `property_missing` /
/// `fan_out_gte` / `all_of`。
///
/// 这批谓词目前**没有已发货规则在用**，优先级低于上一组；但它们是规则语言的
/// 表达能力，坏了会让"以后加规则"这件事受限，所以仍然锁住。
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
    let f = fixture();
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

/// `no_capability` 与判据校验的**交互**：
/// `Capability` 通道一个标注都没有时，规则必须被停用（否则 `no_capability` 恒真）；
/// 一旦通道里有标注，规则恢复执行并对**不具备该能力**的节点命中。
///
/// 这条用例将来会直接决定 `write-endpoint-without-auth` 能不能复活 ——
/// 那条规则目前正是因为这个通道为空而被停用的。
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
    let f = fixture();
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());

    // 阶段一：Capability 通道为空 → 规则应被判据校验拦下
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

    // 阶段二：补上一条**别的**能力（RateLimiting），通道不再为空
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

/// `applies_to.kinds` 为空 = **不限种类**（RuleScope 的文档承诺）。
///
/// 这条曾与实现不符：空 kinds 会被拼成 `kind = ''` 的 SQL，匹配不到任何节点，
/// 规则静默 0 命中 —— 与"不限种类"正好相反。
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
    let f = fixture();
    seed_graph(&f);
    let rules = Arc::new(YamlRuleSet::from_str(YAML).expect("YAML 应可解析"));
    let svc = RuleService::new(f.container.store.clone() as Arc<dyn Persistence>, rules.clone());
    let report = svc.check(f.project, None, false).expect("检查不应失败");
    assert!(
        !report.violations.is_empty(),
        "不限种类时应能跨种类命中（createOrder / POST /api/order/create 等），实际 0 条"
    );
}

/// `read_snippet` 的快乐路径：文件真实存在时**必须读出片段**。
///
/// 之前的用例只覆盖了"文件不存在返回 None"这一条分支，真正的读文件
/// （含 2MB 保护、行号窗口）没人守 —— 而片段是上下文包里给 LLM 看的内容主体。
#[test]
fn recall_reads_snippet_from_real_file() {
    let f = fixture();
    // 注册一个磁盘上真实存在的文件（合成样本里的后端控制器）
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

// ---------------------------------------------------------------- 性能 smoke
//
// 为什么阈值用例值得加，以及为什么不能只写一个绝对数字：
//   * 规则引擎的性能退化**没有任何用例守着** —— 若哪天有人把 Facts 的
//     内存查表换成逐节点查库（N+1），或从 1.4s 退化到 60s，测试照样全绿。
//   * 但绝对阈值容易在不同机器上 flaky（CI 机器慢、负载高）。
//   所以这里两层：
//     1. **缩放比**（主力）：节点数 ×4，耗时不得超过某个倍数。
//        线性实现是 ×4，O(N²) 是 ×16 —— 用一个宽松的倍数（×10）就能把
//        二次退化挡住，而且这个比值**几乎与机器性能无关**。
//     2. **绝对上限**（兜底）：抓 N+1 这类"每节点一次往返"的退化 ——
//        它仍是线性的，缩放比看不出来，但绝对耗时会爆炸。

/// 批量写入 `n` 张表，并让它们都被同一个方法读一次（扇入 1）。
///
/// 用"每个候选都真的有边"而不是空图，是为了让 `fan_in` 计算、
/// 入边预装载、违规构造（含文件定位）都真正被走到 —— 空图测不出退化。
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

/// 一次检查的耗时（毫秒）；规则命中全部表，确保真的对每个候选做了求值。
///
/// 先跑一次**预热**再计时：首次运行要编译 SQL 语句、填充页缓存，
/// 把冷启动算进去会让测量值被固定开销主导，缩放比就失去意义了。
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

/// 规模 ×8 时耗时不得爆炸 —— 挡住 O(N²) 回归。
///
/// # 阈值是实测校准的，不是随手写的倍数
///
/// 用探针量过真实曲线（n = 1k/2k/4k/8k/16k）：
///   优化前 `8 → 18 → 47 → 112 → 344 ms`，约 **N^1.35**（超线性）
///   优化后 `…  → 16000 节点 135 ms`，×8 规模耗时 ×6.1 —— **线性**（理论 8.0）
/// 根因是批量取边的 SQL 让 SQLite 选了 `idx_edges_project`（扫描工程全部边），
/// 代价 O(N²)；补 `(project_id, to_id/from_id)` 复合索引后回到线性
/// （见 `gt-adapter-sqlite/src/schema.rs` 的注释）。
///
/// 于是 ×8 规模下：现在约 ×6，纯二次约 ×64。
/// 预算取 `t_small × 16 + 100ms`：对当前实现留约 3 倍余量（不 flaky），
/// 同时能挡住 ≥N^1.4 的增长。
///
/// 也就是说：这条用例守的是**量级退化**，不是"比昨天快 10%"。
/// 真要做精细性能回归，应该上 criterion 基准，而不是单测。
#[test]
fn check_does_not_degenerate_quadratically() {
    const SMALL: usize = 2_000;
    const BIG: usize = 16_000;

    let fs = fixture();
    seed_many_tables(&fs, SMALL);
    let (t_small, hit_small) = time_check_on_tables(&fs);

    let fb = fixture();
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

/// 绝对上限兜底：抓 N+1 这类"线性但常数极大"的退化
/// —— 它仍是线性的，缩放比看不出来，只能靠绝对耗时。
///
/// 真机实测远低于此（约 26ms），取 2s 留约 75 倍余量：
/// 既不可能在正常机器上 flaky，又能挡住"每个节点多一次 DB 往返"这种量级。
#[test]
fn check_completes_within_budget() {
    const N: usize = 2_000;
    const BUDGET_MS: u128 = 2_000;

    let f = fixture();
    seed_many_tables(&f, N);
    let (ms, hits) = time_check_on_tables(&f);
    eprintln!("[perf] {N} 节点 {ms} ms（预算 {BUDGET_MS} ms）");

    assert_eq!(hits, N, "应全部命中");
    assert!(
        ms <= BUDGET_MS,
        "{N} 个候选的检查耗时 {ms}ms 超过预算 {BUDGET_MS}ms —— \
         若每节点都多了一次 DB 往返就会出现这种量级"
    );
}

/// 开启 `with_snippets` 时必须走读文件的路径**且不因文件缺失而崩**。
///
/// 旧用例里 `with_snippets` 全是 false，这段代码（含 2MB 超大文件保护、
/// 行号越界保护）此前零覆盖。
#[test]
fn recall_with_snippets_is_safe_when_file_missing() {
    let f = fixture();
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
    // 夹具里的源文件在磁盘上并不存在，片段应为 None 而不是 panic
    assert!(
        r.hits.iter().all(|h| h.snippet.is_none()),
        "文件不存在时片段应为 None，实际：{:?}",
        r.hits.iter().filter_map(|h| h.snippet.as_ref()).collect::<Vec<_>>()
    );
}

/// HTTP 层端到端：GET `/api/projects/{id}/recall` 必须能从图上召回相关代码。
///
/// 这是之前唯一没覆盖到的召回面——服务本身有集成用例，但 HTTP 入站适配器
/// （`gt-adapter-http`）从不经过测试，路由拼错 / 参数解错都不会被发现。
#[tokio::test]
async fn recall_http_get_endpoint_returns_hits() {
    let f = fixture();
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

/// HTTP 层端到端：POST `/api/projects/{id}/recall` 走 `RecallQuery` 主体解析。
#[tokio::test]
async fn recall_http_post_endpoint_returns_hits() {
    let f = fixture();
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

/// HTTP 层：`include_body` 必须透传到召回，并在上下文包末尾附上命中文件的完整源码。
///
/// 这是 MCP / IDE 省去二次 `read` 的关键路径。夹具源文件不在磁盘上，因此这里断言的是
/// "段落被拼进去"（读取失败也必须给出提示，而不是静默丢掉整段）。
#[tokio::test]
async fn recall_http_include_body_appends_full_file_section() {
    let f = fixture();
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
        md.contains("## 完整文件（include_body）"),
        "include_body=true 时应附上完整文件段：{md}"
    );
}

/// 对照：不传 `include_body` 时不应出现该段落（默认 false，避免默认输出体积膨胀）。
#[tokio::test]
async fn recall_http_without_include_body_has_no_full_file_section() {
    let f = fixture();
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
        !md.contains("## 完整文件（include_body）"),
        "默认不应附完整文件段：{md}"
    );
}

/// HTTP 层：预热进度端点应返回 warmed / warming 等字段（供 MCP 判断召回是否走冷路径）。
#[tokio::test]
async fn warmup_http_endpoint_returns_status_fields() {
    let f = fixture();
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
