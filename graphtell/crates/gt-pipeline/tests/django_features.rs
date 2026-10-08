//! Django ORM end-to-end self-check (synthetic project, no external sample needed).
//!
//! Covers the chain "model field declaration -> `Column` (`HasColumn`)" and "model relation -> `References` ->
//! `Project` projected into `ForeignKey` (table -> table)" — the same edges and `Project` action as TypeORM /
//! Laravel / ThinkPHP, differing only in that the Python parser turns a `name = models.CharField(...)` field
//! declaration into a `class.field` call site (see `gt-adapter-parser/src/python/mod.rs`).

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// Synthetic Django project: User (two scalar fields) + Post (one scalar field + FK + M2M).
///
/// User / Post are written in the **same file** `myapp/models.py`: the relation field `ForeignKey(User, ...)`'s
/// first positional argument `User` is a class-name identifier; when "not imported", the parser falls back to
/// `{module}.User` (= `myapp.models.User`), which happens to equal the real class FQN, so `References` connects
/// without an extra import.
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

/// Fresh isolated temp dir for a single-file Django model test (mirrors the inline dir setup of the route tests,
/// factored out so the relation tests below stay terse).
fn django_tmp(sub: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-django-{sub}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp")).expect("mkdir");
    dir
}

/// Whether the node named `from` has an out-edge of a given kind reaching the node named `target`.
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

/// Among the targets reached by `from`'s out-edge of a given kind, whether any node has kind == `target_kind`.
/// Used for assertions like "model class -> column" where the target identity carries the full FQN and is awkward to match by short name.
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

/// The number of `from`'s out-edges of a given kind whose target kind is `target_kind`.
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
        panic!("graphing the synthetic Django project must succeed");
    };

    // Model -> table (snake_plural + strip_namespace + singularize): User -> user, Post -> post
    let tables: Vec<String> = nodes_of_kind(&b, "Table").iter().map(|n| n.name.clone()).collect();
    assert!(
        tables.iter().any(|t| t == "user") && tables.iter().any(|t| t == "post"),
        "the user and post tables must be produced, got: {tables:?}"
    );

    // Scalar field -> Column: `User.name` / `User.email` / `Post.title`
    let cols: Vec<String> = nodes_of_kind(&b, "Column").iter().map(|n| n.name.clone()).collect();
    assert!(cols.iter().any(|c| c.contains("User.name")), "User.name must become a column, got: {cols:?}");
    assert!(cols.iter().any(|c| c.contains("User.email")), "User.email must become a column, got: {cols:?}");
    assert!(cols.iter().any(|c| c.contains("Post.title")), "Post.title must become a column, got: {cols:?}");

    // A relation field is **not** an ordinary column: FK / M2M fields must not enter Column
    assert!(
        !cols.iter().any(|c| c.contains("Post.author")),
        "a ForeignKey field must not be an ordinary column, got: {cols:?}"
    );
    assert!(
        !cols.iter().any(|c| c.contains("Post.tags")),
        "a ManyToManyField must not be an ordinary column, got: {cols:?}"
    );

    // HasColumn: model class -> column (field-level impact is drillable)
    assert!(
        links_to_kind(&b, "Class", "Post", "HasColumn", "Column"),
        "the Post model must connect to its columns via HasColumn"
    );
    assert!(
        links_to_kind(&b, "Class", "User", "HasColumn", "Column"),
        "the User model must connect to its columns via HasColumn"
    );

    // References: model -> model (Post.author -> User)
    assert!(
        links_to_named(&b, "Class", "Post", "References", "User"),
        "Post must connect to User via References (the ForeignKey holder)"
    );
    // Post has only one References edge (author -> User); tags is a string reference with no class-constant argument -> no edge
    assert_eq!(
        out_edge_count_to_kind(&b, "Class", "Post", "References", "Class"),
        1,
        "Post must have exactly 1 References edge (author->User); the string-referenced tags must not create one"
    );

    // Table-level foreign key: `References` projected via `Project`, each end landing on a table along `MapsTo`
    assert!(
        links_to_named(&b, "Table", "post", "ForeignKey", "user"),
        "the post table must connect to the user table via the foreign key"
    );
}

/// The argument of `models.ManyToManyField("Tag")` is a **string** (not a class constant), so the target class cannot
/// be resolved and, per `require_class`, it should be skipped without building a dangling `References` edge (same handling as PHP's `morphTo()`).
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
        panic!("graphing must succeed");
    };
    // No References edge at all (the target "Tag" is a string, and here Tag is declared but still a string reference,
    // the parser does not parse class names inside strings) — no fabricated dangling edge.
    let refs = nodes_of_kind(&b, "Class")
        .iter()
        .filter(|n| n.name == "Post")
        .flat_map(|n| b.store.edges_of(n.id, EdgeDirection::Outgoing).expect("edges"))
        .filter(|e| e.kind.as_str() == "References")
        .count();
    assert_eq!(refs, 0, "a string-referenced relation must not create a References edge");
}

/// `path(route, view)` in `urls.py` -> `HttpContract` (`ANY` method + route path) + `HandledBy`
/// connecting to the view function. Django does not encode the HTTP method, so the contract method uses the wildcard `ANY` (matches any frontend call method).
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
        panic!("graphing must succeed");
    };

    // One HttpContract per path(), method wildcard ANY
    let contracts: Vec<Node> = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        names.iter().any(|n| *n == "ANY /users/"),
        "there must be an ANY /users/ contract, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("/users/<int:pk>/")),
        "a route with a converter must be included, got: {names:?}"
    );
    assert_eq!(contracts.len(), 2, "there must be exactly 2 contracts (two path() calls), got: {names:?}");

    // HandledBy: contract -> view function (a bare-name reference is resolved by the parser into the full FQN and connects)
    let users_contract = contracts
        .iter()
        .find(|n| n.name == "ANY /users/")
        .expect("the users contract");
    let handlers: Vec<Node> = b
        .store
        .edges_of(users_contract.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "HandledBy")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .collect();
    assert!(!handlers.is_empty(), "ANY /users/ must connect to the view via HandledBy");
    assert!(
        handlers.iter().any(|n| n.name.ends_with("user_list")),
        "the handler must be the user_list function, got: {handlers:?}"
    );
}

/// Cover three view-reference styles, verifying `HandledBy` connects in each:
/// ① bare name `from .views import user_list` -> `path("users/", user_list)`
/// ② module attribute `from . import views` then `path("articles/", views.article_list)`
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
        panic!("graphing must succeed");
    };

    let contracts: Vec<Node> = nodes_of_kind(&b, "HttpContract");
    assert_eq!(contracts.len(), 3, "there must be exactly 3 contracts, got: {:?}",
        contracts.iter().map(|n| &n.name).collect::<Vec<_>>());

    // Helper: take the target node name a contract connects to via HandledBy
    let handler_of = |name: &str| -> String {
        let c = contracts.iter().find(|n| n.name == name).expect("contract");
        let hs: Vec<Node> = b
            .store
            .edges_of(c.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .filter(|e| e.kind.as_str() == "HandledBy")
            .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
            .collect();
        assert!(!hs.is_empty(), "{name} must have a HandledBy target");
        hs[0].name.clone()
    };

    // ① bare name -> user_list function
    assert_eq!(handler_of("ANY /users/"), "user_list");
    // ② module attribute `views.article_list` -> restored to the myapp.views.article_list function
    assert_eq!(handler_of("ANY /articles/"), "article_list");
    // ③ CBV `.as_view()` -> ArticleListView class
    assert_eq!(handler_of("ANY /detail/"), "ArticleListView");
}

/// `ManyToManyField(Tag)` with a **class constant** target (not the `"Tag"` string form) must resolve and
/// materialise a `References` edge — the positive counterpart to `django_relation_with_string_target_is_skipped`,
/// which only pins that the *string* form is skipped. Without this, a regression that stopped resolving positional
/// class arguments for M2M would silently drop every many-to-many relation from the graph while the skip test stayed green.
#[test]
fn django_m2m_with_class_target_materializes_reference() {
    let dir = django_tmp("m2m-class");
    std::fs::write(dir.join("requirements.txt"), "django==5.0\n").expect("write requirements.txt");
    std::fs::write(
        dir.join("myapp/models.py"),
        r#"
from django.db import models

class Tag(models.Model):
    label = models.CharField(max_length=50)

class Post(models.Model):
    title = models.CharField(max_length=200)
    tags = models.ManyToManyField(Tag)
"#,
    )
    .expect("write models.py");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("graphing must succeed");
    };
    assert!(
        links_to_named(&b, "Class", "Post", "References", "Tag"),
        "a class-constant ManyToManyField must connect Post -> Tag via References"
    );
}

/// `OneToOneField(User, ...)` is a distinct Django relation kind; it must resolve to a `References` edge (and must
/// not be treated as an ordinary scalar column). Only `ForeignKey` and the string-M2M skip were pinned before, so a
/// OneToOne regression would have gone unnoticed.
#[test]
fn django_onetoone_field_materializes_reference() {
    let dir = django_tmp("onetoone");
    std::fs::write(dir.join("requirements.txt"), "django==5.0\n").expect("write requirements.txt");
    std::fs::write(
        dir.join("myapp/models.py"),
        r#"
from django.db import models

class User(models.Model):
    name = models.CharField(max_length=100)

class Account(models.Model):
    owner = models.OneToOneField(User, on_delete=models.CASCADE)
"#,
    )
    .expect("write models.py");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("graphing must succeed");
    };
    assert!(
        links_to_named(&b, "Class", "Account", "References", "User"),
        "a OneToOneField must connect Account -> User via References"
    );
    // It must not be smuggled in as a scalar column.
    let cols: Vec<String> = nodes_of_kind(&b, "Column").iter().map(|n| n.name.clone()).collect();
    assert!(
        !cols.iter().any(|c| c.contains("Account.owner")),
        "a OneToOneField must not be an ordinary column, got: {cols:?}"
    );
}

/// A Django model subclass (`class Admin(User)`) must record an `Extends` edge to its base. Python inheritance was
/// otherwise untested at the build level — only PHP `extends` was pinned in `cf_ast_structure.rs` — so a regression
/// in the Python `class X(Base)` -> `Extends` path would not have been caught.
#[test]
fn django_model_inheritance_records_extends() {
    let dir = django_tmp("inherit");
    std::fs::write(dir.join("requirements.txt"), "django==5.0\n").expect("write requirements.txt");
    std::fs::write(
        dir.join("myapp/models.py"),
        r#"
from django.db import models

class User(models.Model):
    name = models.CharField(max_length=100)

class Admin(User):
    role = models.CharField(max_length=50)
"#,
    )
    .expect("write models.py");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("graphing must succeed");
    };
    assert!(
        links_to_named(&b, "Class", "Admin", "Extends", "User"),
        "Admin(User) must record an Extends edge to User"
    );
}
