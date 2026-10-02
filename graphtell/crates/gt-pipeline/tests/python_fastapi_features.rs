//! End-to-end self-check of FastAPI (Python) route / config / cache semantic features.
//!
//! Deliberately uses a synthetic sample (no external Python project sample needed): place a minimal FastAPI project
//! in a temp directory (a `pyproject.toml` containing fastapi + a few decorated .py files), run a full graph build,
//! and assert that FKB really turns `@router.get` / `os.environ.get` / `redis.set` into the corresponding
//! semantic nodes and edges.
//!
//! This also proves "a scattered language's framework semantics can be completed by only writing FKB + implementing a
//! `LanguageParser`" — Python, the third language, likewise did not change any kernel code.

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_fastapi_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-python-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/api")).expect("mkdir");
    std::fs::create_dir_all(dir.join("app/models")).expect("mkdir");

    // pyproject.toml is both a sub-project marker (-> language=python)
    // and the manifest FKB's detector reads (text contains fastapi).
    std::fs::write(
        dir.join("pyproject.toml"),
        r#"[project]
name = "demo"
dependencies = [
    "fastapi>=0.100",
    "uvicorn",
    "celery>=5.3",
]
"#,
    )
    .expect("write pyproject");

    // The injection target of dependency injection (a separate module, to verify resolution into an FQN by import).
    std::fs::write(dir.join("app/dependencies.py"), "def get_db():\n    return None\n")
        .expect("write dependencies.py");

    std::fs::write(
        dir.join("app/api/users.py"),
        r#"from fastapi import APIRouter, Depends

from app.dependencies import get_db
from app.tasks import notify_slack, send_email

router = APIRouter()


@router.get("/users")
def list_users():
    cache.get("userCache")
    return []


# 依赖注入：依赖写在形参默认值里（路径刻意与 /users 不前缀冲突）
@router.get("/members/{user_id}")
def get_user(user_id: int, db=Depends(get_db)):
    return None


@router.post("/orders/{order_id}")
def create_order(order_id: int):
    redis_client.set("orderCache", "1")
    os.environ.get("SECRET_KEY")
    # 生产端：**跨模块**投递任务（走默认队列 = 任务名）
    send_email.delay("a@b.c")
    # 生产端：**显式指定队列** —— 应投递到 payments，而不是按任务名推断
    notify_slack.apply_async(args=[order_id], queue="payments")
"#,
    )
    .expect("write users.py");

    // Celery task module: `celery_app.task` / `shared_task` two registration styles,
    // plus one **same-module** delivery (to verify producer / consumer ends merge onto one node).
    std::fs::write(
        dir.join("app/tasks.py"),
        r#"from celery import shared_task

celery_app = None


@celery_app.task
def send_email(address):
    pass


@shared_task
def rebuild_index():
    pass


# 显式指定任务名（而非默认的 "函数名"）
@celery_app.task(name="tasks.notify_slack")
def notify_slack(msg):
    pass


def trigger_reindex():
    rebuild_index.delay()


def setup_periodic_tasks(sender, **kwargs):
    # beat 定时：第 1 个实参是执行间隔（秒）
    sender.add_periodic_task(30.0, notify_slack.s())
"#,
    )
    .expect("write tasks.py");

    std::fs::write(
        dir.join("app/main.py"),
        r#"from fastapi import FastAPI

app = FastAPI()


@app.get("/health")
def health():
    return {"ok": True}


class AdminView:
    @app.get("/admin")
    def dashboard(self):
        pass
"#,
    )
    .expect("write main.py");

    // SQLAlchemy declarative model: the table mapping is written in **class attributes** (no call site to rely on),
    // recognised via the P6 graph-node selector. `Base` is in the same file, the sample is self-contained.
    std::fs::write(
        dir.join("app/models/user.py"),
        r#"class Base:
    pass


class UserModel(Base):
    __tablename__ = "users"
    cache_ttl = 600
"#,
    )
    .expect("write user.py");

    dir
}

fn nodes_of_kind(b: &common::Built, kind: &str) -> Vec<Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
}

/// Find the HttpContract node whose name contains the given path fragment.
fn contract_for(b: &common::Built, path_fragment: &str) -> Node {
    nodes_of_kind(b, "HttpContract")
        .into_iter()
        .find(|n| n.name.contains(path_fragment))
        .unwrap_or_else(|| {
            panic!(
                "未找到含 {path_fragment} 的 HttpContract，实际有：{:?}",
                nodes_of_kind(b, "HttpContract").iter().map(|n| &n.name).collect::<Vec<_>>()
            )
        })
}

/// The node name the contract points to via `HandledBy` (should be pinned to the **handler function** itself).
fn handled_by_targets(b: &common::Built, contract: &Node) -> Vec<String> {
    b.store
        .edges_of(contract.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "HandledBy")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .map(|n| n.name)
        .collect()
}

fn has_incoming_edge(b: &common::Built, kind: &str, edge: &str) -> bool {
    nodes_of_kind(b, kind).iter().any(|n| {
        b.store
            .edges_of(n.id, EdgeDirection::Incoming)
            .expect("edges")
            .iter()
            .any(|e| e.kind.as_str() == edge)
    })
}

#[test]
fn fastapi_features_produce_semantic_nodes_and_edges() {
    let root = synthetic_fastapi_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成 FastAPI 工程建图应成功");
    };

    // The three routes (including in-class decorators) should each become an HttpContract.
    assert!(
        nodes_of_kind(&b, "HttpContract").len() >= 3,
        "应产出 ≥3 个 HttpContract，实际：{:?}",
        nodes_of_kind(&b, "HttpContract").iter().map(|n| &n.name).collect::<Vec<_>>()
    );

    let users = contract_for(&b, "/users");
    assert_eq!(
        handled_by_targets(&b, &users),
        vec!["list_users".to_string()],
        "路由 /users 的 HandledBy 应精确连到模块级函数 list_users"
    );

    let health = contract_for(&b, "/health");
    assert_eq!(
        handled_by_targets(&b, &health),
        vec!["health".to_string()],
        "路由 /health 的 HandledBy 应连到 health"
    );

    // In-class decorator: owner_class is the class, so it should connect to the **method** node, not the class node.
    let admin = contract_for(&b, "/admin");
    assert_eq!(
        handled_by_targets(&b, &admin),
        vec!["dashboard".to_string()],
        "类内装饰器的 HandledBy 应连到方法 dashboard"
    );

    // Config: `os.environ.get("SECRET_KEY")` -> ConfigKey + ReadsConfig.
    let cfg: Vec<String> = nodes_of_kind(&b, "ConfigKey").iter().map(|n| n.name.clone()).collect();
    assert!(cfg.iter().any(|n| n == "SECRET_KEY"), "应产出 SECRET_KEY 配置节点，实际：{cfg:?}");
    assert!(
        has_incoming_edge(&b, "ConfigKey", "ReadsConfig"),
        "ConfigKey 应有 ReadsConfig 入边"
    );

    // Cache: reads (cache.get) and writes (redis_client.set) are separated.
    assert!(has_incoming_edge(&b, "Cache", "ReadsCache"), "Cache 应有 ReadsCache 入边");
    assert!(has_incoming_edge(&b, "Cache", "WritesCache"), "Cache 应有 WritesCache 入边");

    // Table mapping: `__tablename__ = "users"` becomes a Table via "class attribute -> Property fact -> graph-node selector",
    // and MapsTo must **originate from the model class** (not dangle or point elsewhere).
    let model = nodes_of_kind(&b, "Class")
        .into_iter()
        .find(|n| n.name == "UserModel")
        .expect("应有 UserModel 类节点");
    let mapped: Vec<String> = b
        .store
        .edges_of(model.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "MapsTo")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .map(|n| n.name)
        .collect();
    assert_eq!(
        mapped,
        vec!["user".to_string()],
        "UserModel 应经 MapsTo 连到表 user（表名经 singularize 归一）"
    );
}

/// Dependency injection: `def get_user(..., db=Depends(get_db))` — the dependency is written in a **parameter default**,
/// which should build a `handler function --DependsOn--> dependency function` edge.
#[test]
fn fastapi_depends_injection_links_handler_to_dependency() {
    let root = synthetic_fastapi_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成工程建图应成功");
    };
    let functions = nodes_of_kind(&b, "Function");
    let handler = functions
        .iter()
        .find(|n| n.name == "get_user")
        .unwrap_or_else(|| {
            panic!(
                "应有 get_user 函数节点，实际：{:?}",
                functions.iter().map(|n| &n.name).collect::<Vec<_>>()
            )
        });
    let deps: Vec<String> = b
        .store
        .edges_of(handler.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "DependsOn")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .map(|n| n.name)
        .collect();
    assert_eq!(
        deps,
        vec!["get_db".to_string()],
        "get_user 应 DependsOn 到依赖函数 get_db"
    );
}

/// Celery advanced capabilities: **explicit declaration takes precedence over convention inference**.
///
/// * Explicit queue name `apply_async(..., queue="payments")` -> delivered to payments, not the task name
/// * Explicit task name `@celery_app.task(name="tasks.notify_slack")` -> the node is built under that name
/// * beat schedule `add_periodic_task(30.0, ...)` -> Schedule node + Triggers out-edge
#[test]
fn celery_explicit_queue_and_beat_schedule() {
    let root = synthetic_fastapi_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成工程建图应成功");
    };
    let queues = nodes_of_kind(&b, "Queue");
    let names: Vec<&str> = queues.iter().map(|n| n.name.as_str()).collect();

    // Explicit queue name beats task-name inference
    let payments = queues
        .iter()
        .find(|n| n.name == "payments")
        .unwrap_or_else(|| panic!("应按显式 queue= 产出 payments 队列，实际：{names:?}"));
    let incoming: Vec<String> = b
        .store
        .edges_of(payments.id, EdgeDirection::Incoming)
        .expect("edges")
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect();
    assert!(
        incoming.iter().any(|k| k == "PublishesTo"),
        "payments 应有 PublishesTo，实际入边 {incoming:?}"
    );

    // Explicit task name (`@celery_app.task(name="tasks.notify_slack")`) built under that name
    assert!(
        names.contains(&"notify_slack"),
        "应按显式 name= 落成 notify_slack 队列，实际：{names:?}"
    );

    // beat schedule
    let schedules = nodes_of_kind(&b, "Schedule");
    let detail: Vec<&str> = schedules.iter().map(|n| n.name.as_str()).collect();
    let schedule = schedules
        .iter()
        .find(|n| n.name == "30.0")
        .unwrap_or_else(|| panic!("应以间隔字面量建成 Schedule 节点，实际：{detail:?}"));
    let triggers: Vec<String> = b
        .store
        .edges_of(schedule.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect();
    assert!(
        triggers.iter().any(|k| k == "Triggers"),
        "Schedule 应有 Triggers 出边，实际出边 {triggers:?}"
    );
}

/// The producer / consumer loop of a Celery task: both ends merge onto the same Queue node by **task name**,
/// rather than a split view of "register one node, deliver another".
#[test]
fn celery_tasks_form_publish_subscribe_loop() {
    let root = synthetic_fastapi_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成工程建图应成功");
    };
    let queues = nodes_of_kind(&b, "Queue");
    let names: Vec<&str> = queues.iter().map(|n| n.name.as_str()).collect();

    assert!(
        has_incoming_edge(&b, "Queue", "ListensTo"),
        "Queue 应有 ListensTo 入边（任务注册），实际：{names:?}"
    );
    assert!(
        has_incoming_edge(&b, "Queue", "PublishesTo"),
        "Queue 应有 PublishesTo 入边（任务投递），实际：{names:?}"
    );

    // The core assertion of type-level merge: the same task name should **simultaneously** carry two in-edges.
    for task in ["rebuild_index", "send_email"] {
        let node = queues
            .iter()
            .find(|n| n.name == task)
            .unwrap_or_else(|| panic!("未见任务节点 {task}，实际：{names:?}"));
        let incoming: Vec<String> = b
            .store
            .edges_of(node.id, EdgeDirection::Incoming)
            .expect("edges")
            .iter()
            .map(|e| e.kind.as_str().to_string())
            .collect();
        assert!(
            incoming.iter().any(|k| k == "ListensTo"),
            "{task} 应有 ListensTo，实际入边 {incoming:?}；全部队列：{names:?}"
        );
        assert!(
            incoming.iter().any(|k| k == "PublishesTo"),
            "{task} 应有 PublishesTo，实际入边 {incoming:?}；全部队列：{names:?}"
        );
    }
}
