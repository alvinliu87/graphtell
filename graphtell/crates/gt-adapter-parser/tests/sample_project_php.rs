//! Parser tests against real sample_project source files.
//!
//! These cases verify the accuracy of the **facts** tree-sitter extracts, not the semantics:
//! * namespace -> FQN derivation
//! * the model inheritance chain (with the terminal class beyond vendor)
//! * entry extraction from config arrays (`event.php` / `provider.php`)
//! * `Route::post(...)` call sites inside route closures (the easiest one to miss)

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{Language, SyntaxFacts};
use gt_domain::port::ParserRegistry;
use gt_sample_support::{missing_hint, sample_project_inner_dir, sample_root};

fn parse_php(rel: &str) -> Option<SyntaxFacts> {
    let root = sample_root()?;
    let path = root.join(sample_project_inner_dir()).join(rel);
    let content = std::fs::read_to_string(&path).ok()?;
    let registry = DefaultParserRegistry::new();
    let parser = registry
        .parser_for(&Language::new(Language::PHP))
        .expect("the php parser is registered");
    parser.parse(path.to_str().unwrap(), &content).ok()
}

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_model_namespace_and_fqn() {
    let Some(facts) = parse_php("app/model/order/StoreOrder.php") else {
        panic!("{}", missing_hint());
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "StoreOrder")
        .expect("expected the StoreOrder class to be parsed");
    assert_eq!(
        class.fqn, "app\\model\\order\\StoreOrder",
        "the FQN should be derived from namespace + class"
    );
    // Inheritance chain: StoreOrder -> BaseModel -> Model (terminal class in vendor)
    let inherits = facts
        .inheritances
        .iter()
        .any(|i| i.child_fqn == class.fqn && (i.base_name.contains("BaseModel") || i.base_name.contains("Model")));
    assert!(
        inherits,
        "inheritance should be recorded (even though the leaf class is excluded in vendor)"
    );

    // A class must never appear as its own base: `base_name` comes from the `extends`/`implements` clause text
    // (src/php.rs:327), so a regression pushing the child FQN would create a self-inheritance edge and make P7
    // walk the model hierarchy forever.
    assert!(
        !facts
            .inheritances
            .iter()
            .any(|i| i.child_fqn == class.fqn && i.base_name.contains("StoreOrder")),
        "a class must not inherit from itself: {:?}",
        facts
            .inheritances
            .iter()
            .filter(|i| i.child_fqn == class.fqn)
            .map(|i| &i.base_name)
            .collect::<Vec<_>>()
    );
}

/// A ThinkPHP model does **not** carry its table name in `$table` but in `$name`
/// (`protected $name = 'store_order'`, with the table prefix supplied separately by config); `$pk` likewise
/// gives the primary key. P5's "model -> table" recognition depends on both properties, so this guards
/// "the property value must be extracted" — asserting only that the property exists is not enough, since an
/// empty value means the table name was not recognised.
#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_model_name_and_pk_properties() {
    let Some(facts) = parse_php("app/model/order/StoreOrder.php") else {
        panic!("{}", missing_hint());
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "StoreOrder")
        .expect("expected the StoreOrder class to be parsed");
    // A property is an independent Declaration whose parent_fqn points at the owning class
    let prop = |name: &str| {
        facts.declarations.iter().find(|d| {
            d.kind.is("Property")
                && d.name == name
                && d.parent_fqn.as_deref() == Some(class.fqn.as_str())
        })
    };
    fn default_of(d: &gt_domain::model::Declaration) -> Option<String> {
        let v = d.extra.get("default")?;
        if v.get("t").and_then(|t| t.as_str()) != Some("String") {
            return None;
        }
        v.get("v").and_then(|x| x.as_str()).map(|s| s.to_string())
    }
    let name = prop("name").expect("StoreOrder should have a protected $name (the table-name source)");
    assert_eq!(
        default_of(name).as_deref(),
        Some("store_order"),
        "the $name property value must be extracted"
    );
    let pk = prop("pk").expect("StoreOrder should have a protected $pk (the primary key)");
    assert_eq!(default_of(pk).as_deref(), Some("id"), "the $pk property value must be extracted");
}

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_event_php_config_entries() {
    let Some(facts) = parse_php("app/event.php") else {
        panic!("{}", missing_hint());
    };
    let pay_success = facts
        .config_entries
        .iter()
        .find(|c| c.key_path == "listen.OrderPaySuccessListener")
        .expect("event.php should extract the listen.OrderPaySuccessListener config entry");
    assert!(
        !pay_success.value.array_values().is_empty() || pay_success.value.as_str().is_some(),
        "the OrderPaySuccessListener event should carry a listener list"
    );
    // The listener class itself must land in the entry too, otherwise the event -> listener edge has nothing to attach to.
    assert!(
        facts
            .config_entries
            .iter()
            .any(|c| c.key_path.starts_with("listen.OrderPaySuccessListener.")
                && c.value.as_str() == Some("app\\listener\\order\\OrderPaySuccessListener")),
        "the listener class app\\listener\\order\\OrderPaySuccessListener should be extracted"
    );
    // Nested arrays must keep their dotted prefix (src/php.rs:862): a bare `OrderPaySuccessListener` key —
    // flattened, without the `listen.` nesting — would lose the event context every listener edge hangs on.
    assert!(
        !facts
            .config_entries
            .iter()
            .any(|c| c.key_path == "OrderPaySuccessListener"),
        "config key paths must retain their nesting; got: {:?}",
        facts
            .config_entries
            .iter()
            .map(|c| &c.key_path)
            .collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_provider_php_bindings() {
    let Some(facts) = parse_php("app/provider.php") else {
        panic!("{}", missing_hint());
    };
    let entry = |key: &str| {
        facts
            .config_entries
            .iter()
            .find(|c| c.key_path == key)
            .unwrap_or_else(|| panic!("provider.php should parse a binding for {key}"))
    };
    for (interface, impl_hint) in [
        ("think\\Request", "Request"),
        ("think\\exception\\Handle", "ExceptionHandle"),
    ] {
        let e = entry(interface);
        assert!(
            e.value.as_str().is_some_and(|v| !v.is_empty()),
            "{interface} should bind to a non-empty implementation ({impl_hint})"
        );
    }
}

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_route_call_sites_inside_closures() {
    let Some(facts) = parse_php("app/api/route/v1.php") else {
        panic!("{}", missing_hint());
    };
    // Route registration is written inside a closure: `Route::post('apple_login', 'Login/appleLogin')`
    let route_calls = facts
        .call_sites
        .iter()
        .filter(|c| c.callee_text.contains("Route::post") || c.callee_text.contains("Route::get"))
        .collect::<Vec<_>>();
    assert!(
        !route_calls.is_empty(),
        "Route::post/get call sites in the route file must be collected (even inside closures)"
    );
    let apple = route_calls
        .iter()
        .find(|c| c.args.iter().any(|a| a.as_str() == Some("apple_login")));
    assert!(
        apple.is_some(),
        "expected to locate the call site of the apple_login route"
    );
}

/// NEGATIVE: every `Route::post` / `Route::get` registration must carry its **route path** as an argument — FKB
/// builds an `HttpContract` node from that path, so a zero-argument route call site would produce an anonymous /
/// empty endpoint. Route registration is always written `Route::post('path', 'Handler')`, so an empty `args` is a
/// definite extraction regression (e.g. losing the arguments of a call collected inside a closure).
#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn route_call_sites_carry_their_path_argument() {
    let Some(facts) = parse_php("app/api/route/v1.php") else {
        panic!("{}", missing_hint());
    };
    let empty_route_calls: Vec<&str> = facts
        .call_sites
        .iter()
        .filter(|c| {
            (c.callee_text.contains("Route::post") || c.callee_text.contains("Route::get"))
                && c.args.is_empty()
        })
        .map(|c| c.callee_text.as_str())
        .collect();
    assert!(
        empty_route_calls.is_empty(),
        "a Route::post/get registration must carry its path argument, got empty: {empty_route_calls:?}"
    );
}

/// The existing model test only asserts the **class** FQN; the method FQN (`Class.method`) is the node identity
/// every route and edge hangs on. Verified on a real controller so the namespace + class + method derivation is
/// pinned end-to-end (synthesized sources can't prove the real `app\outapi\controller` namespace resolves).
#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_controller_method_fqn() {
    let Some(facts) = parse_php("app/outapi/controller/StoreCategory.php") else {
        panic!("{}", missing_hint());
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "StoreCategory")
        .expect("expected the StoreCategory class to be parsed");
    let method = facts
        .declarations
        .iter()
        .find(|d| {
            d.kind.is("Method")
                && d.name == "index"
                && d.parent_fqn.as_deref() == Some(class.fqn.as_str())
        })
        .expect("expected the index() method to be parsed");
    assert_eq!(
        method.fqn, "app\\outapi\\controller\\StoreCategory::index",
        "a method FQN must be derived from namespace + class + method (PHP uses `::` as the member separator)"
    );

    // PHP's member separator is `::` (src/php.rs:58), NOT the `.` that Java / JS use. A cross-language
    // "unify the separator" regression would silently change every PHP method node identity while leaving the
    // positive assertion above untouched only if it were written with the same wrong separator — this negative
    // pins the separator independently.
    assert_ne!(
        method.fqn, "app\\outapi\\controller\\StoreCategory.index",
        "PHP method FQNs must use `::`, never the Java/JS `.` separator"
    );
}

/// Short-name -> FQN resolution (P2 restores decls by, P7 resolves `MapsTo`) depends on `use` imports being
/// extracted with their full dotted name. Unguarded anywhere on the corpus; pinned here so a regression that
/// drops import collection silently breaks every later phase's name lookup.
#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_model_use_imports() {
    let Some(facts) = parse_php("app/model/order/StoreOrder.php") else {
        panic!("{}", missing_hint());
    };
    let names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
    // These two are what let `Model` / `BaseModel` resolve to an FQN rather than a bare short name.
    assert!(
        names.contains(&"think\\Model"),
        "the base Model import must be collected: {names:?}"
    );
    assert!(
        names.contains(&"crmeb\\basic\\BaseModel"),
        "the crmeb BaseModel import must be collected: {names:?}"
    );
}
