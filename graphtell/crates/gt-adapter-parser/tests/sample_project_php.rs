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
use gt_sample_support::{missing_hint, sample_root};

fn parse_php(rel: &str) -> Option<SyntaxFacts> {
    let root = sample_root()?;
    let path = root.join(rel);
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
    let Some(facts) = parse_php("crmeb/app/model/order/StoreOrder.php") else {
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
}

/// A ThinkPHP model does **not** carry its table name in `$table` but in `$name`
/// (`protected $name = 'store_order'`, with the table prefix supplied separately by config); `$pk` likewise
/// gives the primary key. P5's "model -> table" recognition depends on both properties, so this guards
/// "the property value must be extracted" — asserting only that the property exists is not enough, since an
/// empty value means the table name was not recognised.
#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_model_name_and_pk_properties() {
    let Some(facts) = parse_php("crmeb/app/model/order/StoreOrder.php") else {
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
    let Some(facts) = parse_php("crmeb/app/event.php") else {
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
}

#[test]
#[ignore = "needs the sample_project sample, which is not committed (too large to ship with the repo)"]
fn parses_provider_php_bindings() {
    let Some(facts) = parse_php("crmeb/app/provider.php") else {
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
    let Some(facts) = parse_php("crmeb/app/api/route/v1.php") else {
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
