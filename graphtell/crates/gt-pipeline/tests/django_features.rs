//! Django ORM 端到端自检（合成工程，无需外部样本）。
//!
//! 覆盖「模型字段声明 → `Column`（`HasColumn`）」「模型关联 → `References` →
//! `Project` 投影成 `ForeignKey`（表 → 表）」这条链 —— 与 TypeORM / Laravel / ThinkPHP
//! 同一套边与 `Project` 动作，区别只在 Python 解析器把 `name = models.CharField(...)`
//! 字段声明翻成「类.字段」调用点（见 `gt-adapter-parser/src/python/mod.rs`）。

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// 合成 Django 工程：User（两个标量字段）+ Post（一个标量字段 + FK + M2M）。
///
/// User / Post 写在**同一文件** `myapp/models.py`：关系字段 `ForeignKey(User, ...)`
/// 的首个位置实参 `User` 是类名标识符，解析器在「未被 import」时退回 `{module}.User`
/// (= `myapp.models.User`)，恰好等于真实类 FQN，故 `References` 能连上，无需额外 import。
fn synthetic_django_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-django-orm-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp")).expect("mkdir");
    std::fs::write(dir.join("requirements.txt"), "django==5.0\n").expect("write requirements.txt");
    std::fs::write(
        dir.join("myapp/models.py"),
        r#"
from django.db import models

class User(models.Model):
    name = models.CharField(max_length=100)
    email = models.EmailField()

class Post(models.Model):
    title = models.CharField(max_length=200)
    author = models.ForeignKey(User, on_delete=models.CASCADE)
    tags = models.ManyToManyField("Tag")
"#,
    )
    .expect("write models.py");
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

/// 名为 `from` 的节点是否有指定种类的出边连到名为 `target` 的节点。
fn links_to_named(b: &common::Built, kind: &str, from: &str, edge: &str, target: &str) -> bool {
    let Some(src) = nodes_of_kind(b, kind).into_iter().find(|n| n.name == from) else {
        return false;
    };
    b.store
        .edges_of(src.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == edge)
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .any(|n| n.name == target)
}

/// 名为 `from` 的节点指定种类出边连到的目标里，是否有某个种类 == `target_kind` 的节点。
/// 用于「模型类 → 列」这类目标身份含完整 FQN、不便按短名匹配的断言。
fn links_to_kind(b: &common::Built, kind: &str, from: &str, edge: &str, target_kind: &str) -> bool {
    let Some(src) = nodes_of_kind(b, kind).into_iter().find(|n| n.name == from) else {
        return false;
    };
    b.store
        .edges_of(src.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == edge)
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .any(|n| n.kind.as_str() == target_kind)
}

/// 名为 `from` 的节点指定种类出边连到的、且目标种类为 `target_kind` 的边数。
fn out_edge_count_to_kind(b: &common::Built, kind: &str, from: &str, edge: &str, target_kind: &str) -> usize {
    let Some(src) = nodes_of_kind(b, kind).into_iter().find(|n| n.name == from) else {
        return 0;
    };
    b.store
        .edges_of(src.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == edge)
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .filter(|n| n.kind.as_str() == target_kind)
        .count()
}

#[test]
fn django_model_fields_become_columns_and_relations_become_foreign_keys() {
    let root = synthetic_django_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成 Django 工程建图应成功");
    };

    // 模型 → 表（snake_plural + strip_namespace + singularize）：User → user，Post → post
    let tables: Vec<String> = nodes_of_kind(&b, "Table").iter().map(|n| n.name.clone()).collect();
    assert!(
        tables.iter().any(|t| t == "user") && tables.iter().any(|t| t == "post"),
        "应产出 user / post 两张表，实际：{tables:?}"
    );

    // 标量字段 → Column：`User.name` / `User.email` / `Post.title`
    let cols: Vec<String> = nodes_of_kind(&b, "Column").iter().map(|n| n.name.clone()).collect();
    assert!(cols.iter().any(|c| c.contains("User.name")), "User.name 应成为列，实际：{cols:?}");
    assert!(cols.iter().any(|c| c.contains("User.email")), "User.email 应成为列，实际：{cols:?}");
    assert!(cols.iter().any(|c| c.contains("Post.title")), "Post.title 应成为列，实际：{cols:?}");

    // 关系字段**不是**普通列：外键 / 多对多字段不该进 Column
    assert!(
        !cols.iter().any(|c| c.contains("Post.author")),
        "ForeignKey 字段不该是普通列，实际：{cols:?}"
    );
    assert!(
        !cols.iter().any(|c| c.contains("Post.tags")),
        "ManyToManyField 字段不该是普通列，实际：{cols:?}"
    );

    // HasColumn：模型类 → 列（字段级影响面可下钻）
    assert!(
        links_to_kind(&b, "Class", "Post", "HasColumn", "Column"),
        "Post 模型应经 HasColumn 连到列"
    );
    assert!(
        links_to_kind(&b, "Class", "User", "HasColumn", "Column"),
        "User 模型应经 HasColumn 连到列"
    );

    // References：模型 → 模型（Post.author → User）
    assert!(
        links_to_named(&b, "Class", "Post", "References", "User"),
        "Post 应经 References 连到 User（ForeignKey 持有方）"
    );
    // Post 只有一条 References 边（author → User）；tags 是字符串引用、无类常量实参 → 不建边
    assert_eq!(
        out_edge_count_to_kind(&b, "Class", "Post", "References", "Class"),
        1,
        "Post 应只有 1 条 References 边（author→User），字符串引用的 tags 不该建边"
    );

    // 表级外键：`References` 经 `Project` 投影，两端各沿 `MapsTo` 落到表
    assert!(
        links_to_named(&b, "Table", "post", "ForeignKey", "user"),
        "post 表应经 ForeignKey 连到 user 表"
    );
}

/// `models.ManyToManyField("Tag")` 的实参是**字符串**（非类常量），取不到目标类，
/// 应根据 `require_class` 跳过、不建悬空 `References` 边（与 PHP 的 `morphTo()` 同处理）。
#[test]
fn django_relation_with_string_target_is_skipped() {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-django-strrel-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp")).expect("mkdir");
    std::fs::write(dir.join("requirements.txt"), "django==5.0\n").expect("write requirements.txt");
    std::fs::write(
        dir.join("myapp/models.py"),
        r#"
from django.db import models

class Tag(models.Model):
    label = models.CharField(max_length=50)

class Post(models.Model):
    tags = models.ManyToManyField("Tag")
"#,
    )
    .expect("write models.py");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("建图应成功");
    };
    // 没有任何 References 边（目标 "Tag" 是字符串、且此处 Tag 已声明但仍是字符串引用，
    // 解析器不解析字符串里的类名）—— 不应编造悬空边。
    let refs = nodes_of_kind(&b, "Class")
        .iter()
        .filter(|n| n.name == "Post")
        .flat_map(|n| b.store.edges_of(n.id, EdgeDirection::Outgoing).expect("edges"))
        .filter(|e| e.kind.as_str() == "References")
        .count();
    assert_eq!(refs, 0, "字符串引用的关系不应建 References 边");
}

/// `urls.py` 的 `path(route, view)` → `HttpContract`（`ANY` 方法 + route 路径）+ `HandledBy`
/// 连到视图函数。Django 不编码 HTTP method，故契约方法用通配 `ANY`（匹配任意前端调用方法）。
#[test]
fn django_url_routes_become_http_contracts() {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-django-routes-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp")).expect("mkdir");
    std::fs::write(dir.join("requirements.txt"), "django==5.0\n").expect("write requirements.txt");
    std::fs::write(
        dir.join("myapp/views.py"),
        r#"
def user_list(request):
    return []

def user_detail(request, pk):
    return []
"#,
    )
    .expect("write views.py");
    std::fs::write(
        dir.join("myapp/urls.py"),
        r#"
from django.urls import path
from .views import user_list, user_detail

urlpatterns = [
    path("users/", user_list, name="user_list"),
    path("users/<int:pk>/", user_detail, name="user_detail"),
]
"#,
    )
    .expect("write urls.py");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("建图应成功");
    };

    // 每个 path() 一条 HttpContract，方法通配 ANY
    let contracts: Vec<Node> = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        names.iter().any(|n| *n == "ANY /users/"),
        "应有 ANY /users/ 契约，实际：{names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("/users/<int:pk>/")),
        "应含带转换器的路由，实际：{names:?}"
    );
    assert_eq!(contracts.len(), 2, "应恰好 2 条契约（两个 path()），实际：{names:?}");

    // HandledBy：契约 → 视图函数（裸名引用经解析器解析成全 FQN 连上）
    let users_contract = contracts
        .iter()
        .find(|n| n.name == "ANY /users/")
        .expect("users 契约");
    let handlers: Vec<Node> = b
        .store
        .edges_of(users_contract.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "HandledBy")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .collect();
    assert!(!handlers.is_empty(), "ANY /users/ 应经 HandledBy 连到视图");
    assert!(
        handlers.iter().any(|n| n.name.ends_with("user_list")),
        "handler 应为 user_list 函数，实际：{handlers:?}"
    );
}

/// 覆盖三种视图引用写法，验证 `HandledBy` 都能连上：
/// ① 裸名 `from .views import user_list` → `path("users/", user_list)`
/// ② 模块属性 `from . import views` 后 `path("articles/", views.article_list)`
/// ③ CBV `path("detail/", ArticleListView.as_view())`
#[test]
fn django_route_view_reference_variants() {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-django-routes-v-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp")).expect("mkdir");
    std::fs::write(dir.join("requirements.txt"), "django==5.0\n").expect("write requirements.txt");
    std::fs::write(
        dir.join("myapp/views.py"),
        r#"def user_list(request):
    return []

def article_list(request):
    return []

class ArticleListView:
    pass
"#,
    )
    .expect("write views.py");
    std::fs::write(
        dir.join("myapp/urls.py"),
        r#"from django.urls import path
from .views import user_list, ArticleListView
from . import views

urlpatterns = [
    path("users/", user_list, name="user_list"),
    path("articles/", views.article_list, name="article_list"),
    path("detail/", ArticleListView.as_view(), name="detail"),
]
"#,
    )
    .expect("write urls.py");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("建图应成功");
    };

    let contracts: Vec<Node> = nodes_of_kind(&b, "HttpContract");
    assert_eq!(contracts.len(), 3, "应恰好 3 条契约，实际：{:?}",
        contracts.iter().map(|n| &n.name).collect::<Vec<_>>());

    // 辅助：取某契约经 HandledBy 连到的目标节点名
    let handler_of = |name: &str| -> String {
        let c = contracts.iter().find(|n| n.name == name).expect("契约");
        let hs: Vec<Node> = b
            .store
            .edges_of(c.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .filter(|e| e.kind.as_str() == "HandledBy")
            .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
            .collect();
        assert!(!hs.is_empty(), "{name} 应有 HandledBy 目标");
        hs[0].name.clone()
    };

    // ① 裸名 → user_list 函数
    assert_eq!(handler_of("ANY /users/"), "user_list");
    // ② 模块属性 `views.article_list` → 还原成 myapp.views.article_list 函数
    assert_eq!(handler_of("ANY /articles/"), "article_list");
    // ③ CBV `.as_view()` → ArticleListView 类
    assert_eq!(handler_of("ANY /detail/"), "ArticleListView");
}
