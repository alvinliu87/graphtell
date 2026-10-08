//! Python parsing smoke test: verify that syntax fact extraction is correct through the public registry.
//!
//! Mirrors `php_smoke.rs`: the unit tests in `src/python.rs` call `PythonParser::new()` directly, so a
//! regression in the facts the *pipeline* depends on (N+1 `in_loop`, the SQLAlchemy/Django `__tablename__`
//! property that P7 turns into a table mapping) would not be caught at the registry level. These tests pin
//! the same downstream-critical surface through `DefaultParserRegistry` — the path the pipeline actually uses.

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{Declaration, FactValue, Language, NodeKind, SyntaxFacts};
use gt_domain::port::ParserRegistry;

fn parse(path: &str, src: &str) -> SyntaxFacts {
    let reg = DefaultParserRegistry::new();
    let p = reg
        .parser_for(&Language::new(Language::PYTHON))
        .expect("python parser registered");
    p.parse(path, src).expect("parse ok")
}

/// Find a call site by its method name (verbs repeat far less than in PHP, so the method is the stable key).
fn call_method<'a>(facts: &'a SyntaxFacts, method: &str) -> &'a gt_domain::model::CallSiteFact {
    facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some(method))
        .unwrap_or_else(|| panic!("expected a call to {method}"))
}

/// N+1 detection relies on `CallSiteFact.in_loop`: a call inside a loop body runs per-row, a call in the loop
/// *iterable* does not. Guarded in `src/python.rs` directly; this pins it through the registry.
#[test]
fn marks_call_sites_inside_loop_bodies() {
    let src = r#"def run(items):
    total = count_all()
    for u in fetch_page():
        db.query(u)
    while True:
        db.flush()
"#;
    let facts = parse("app/services/sync.py", src);
    assert_eq!(call_method(&facts, "count_all").in_loop, false, "a call before the loop runs once");
    assert_eq!(
        call_method(&facts, "fetch_page").in_loop,
        false,
        "a call in the for-in iterable is not inside the loop body"
    );
    assert_eq!(
        call_method(&facts, "query").in_loop,
        true,
        "db.query inside the for body is the real N+1"
    );
    assert_eq!(
        call_method(&facts, "flush").in_loop,
        true,
        "a call inside the while body is per-row"
    );
}

/// P7 turns a class-body literal (`__tablename__ = "users"`) into a table mapping. The parser's job is only to
/// record it as a `Property` declaration (with the value) — the FQN is derived from the file path. Guarded in
/// `src/python.rs` directly; this pins it through the registry so a regression in the property surface (not just
/// the call-site surface) is caught on the path the pipeline uses.
#[test]
fn class_body_literal_becomes_property_with_table_mapping() {
    let src = r#"class UserModel(Base):
    __tablename__ = "users"
    cache_ttl = 600

    def save(self):
        total = 5
        self.x = 1
"#;
    let facts = parse("app/models/user.py", src);

    let props: Vec<&Declaration> = facts
        .declarations
        .iter()
        .filter(|d| d.kind.as_str() == NodeKind::PROPERTY)
        .collect();
    let names: Vec<&str> = props.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"__tablename__"),
        "class-body literal must become a Property (precondition for SQLAlchemy/Django table mapping): {names:?}"
    );
    assert!(names.contains(&"cache_ttl"), "non-table literals are properties too: {names:?}");

    let table = props
        .iter()
        .find(|d| d.name == "__tablename__")
        .expect("the __tablename__ property");
    assert_eq!(table.fqn, "app.models.user.UserModel.__tablename__");
    assert_eq!(table.parent_fqn.as_deref(), Some("app.models.user.UserModel"));

    let value: FactValue = serde_json::from_value(
        table
            .extra
            .get("default")
            .cloned()
            .expect("property must carry its default value"),
    )
    .expect("default value must deserialise to a FactValue");
    assert_eq!(value, FactValue::String("users".to_string()));
}

/// FastAPI / Flask routes are declarative framework calls: FKB matches `@router.get("/users")` like a PHP / Java / JS
/// route. The owner must be the **handler function**, the path literal must land in arg0, and keyword args (`tags=[...]`)
/// must be captured as a named Array. Guarded in `src/python.rs` directly; this pins it through the registry.
#[test]
fn decorators_become_call_sites_of_handler() {
    let src = r#"@router.get("/users")
def list_users():
    pass

@app.delete("/orders/{order_id}", tags=["admin"])
def remove_order():
    pass
"#;
    let facts = parse("app/api/users.py", src);

    let get = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "router.get")
        .expect("expected the @router.get decorator to become a call site");
    assert_eq!(get.owner_fqn, "app.api.users.list_users", "a decorator lands on the handler function FQN");
    assert_eq!(get.owner_class.as_deref(), Some("app.api.users"));
    assert_eq!(get.receiver.as_deref(), Some("router"));
    assert_eq!(get.method.as_deref(), Some("get"));
    assert_eq!(get.args.first().and_then(|a| a.as_str()), Some("/users"), "the path literal must be arg0");

    let del = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "app.delete")
        .expect("expected the @app.delete decorator to become a call site");
    assert_eq!(del.owner_fqn, "app.api.users.remove_order");
    assert_eq!(del.args.first().and_then(|a| a.as_str()), Some("/orders/{order_id}"));
    match del.args.get(1) {
        Some(FactValue::Array(items)) => assert_eq!(items[0].0, "tags", "keyword args are keyed by name"),
        other => panic!("keyword arg should be captured as Array, got: {other:?}"),
    }
}

/// Django / SQLAlchemy: `author = models.ForeignKey(User, ...)` becomes a call site with `owner` pinned to
/// `class.field` and the first positional arg resolved (via import) into the target model FQN, so FKB can link the
/// `References` edge — same mechanism as the JS field decorator / Java field annotation. Guarded in `src/python.rs`
/// directly; this pins it through the registry.
#[test]
fn class_body_field_declaration_becomes_call_site_with_entity() {
    let src = r#"from app.models.user import User

class Post(models.Model):
    title = models.CharField(max_length=200)
    author = models.ForeignKey(User, on_delete=models.CASCADE)
"#;
    let facts = parse("app/models/blog.py", src);

    let char = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "models.CharField")
        .expect("expected a CharField call site");
    assert_eq!(char.owner_fqn, "app.models.blog.Post.title");
    assert_eq!(char.owner_class.as_deref(), Some("app.models.blog.Post"));

    let fk = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "models.ForeignKey")
        .expect("expected a ForeignKey call site");
    assert_eq!(fk.owner_fqn, "app.models.blog.Post.author");
    assert_eq!(
        fk.entity.as_deref(),
        Some("app.models.user.User"),
        "the foreign-key target should resolve to an FQN via import"
    );
}

/// FastAPI DI: `db=Depends(get_db)` -> a call site with `entity` = the resolved dependency FQN, so FKB can build a
/// `DependsOn` edge (the Python analogue of the JS constructor `@Inject`). Guarded in `src/python.rs` directly;
/// this pins it through the registry.
#[test]
fn parameter_default_dependency_becomes_call_site_with_entity() {
    let src = r#"from app.dependencies import get_db

def get_user(user_id: int, db=Depends(get_db)):
    pass
"#;
    let facts = parse("app/api/users.py", src);
    let call = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "Depends")
        .expect("expected the Depends call site");
    assert_eq!(call.owner_fqn, "app.api.users.get_user");
    assert_eq!(
        call.entity.as_deref(),
        Some("app.dependencies.get_db"),
        "the dependency name must resolve to an FQN"
    );
}

/// `from .views import user_list` must restore `app.views.user_list`; otherwise the FQN Django routes / Flask
/// blueprints resolve to would be wrong and cross-file references would never connect. Guarded in `src/python.rs`
/// directly; this pins it through the registry.
#[test]
fn relative_import_resolves_to_absolute_module() {
    let facts = parse("app/urls.py", "from .views import user_list\nfrom ..core import helper\n");
    let has = |want: &str| facts.imports.iter().any(|imp| imp.name == want);
    assert!(has("app.views.user_list"), "a relative import must restore the absolute module: {:?}", facts.imports);
    assert!(has("core.helper"), "a `..` level walks up past the parent: {:?}", facts.imports);
}
