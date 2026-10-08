//! Java parsing smoke test: verify that syntax fact extraction is correct through the public registry.
//!
//! Mirrors `php_smoke.rs`: the unit tests in `src/java.rs` call `JavaParser::new()` directly. The facts the
//! *pipeline* depends on — `interface`/`enum` declaration kinds and the `@Autowired` `field_types` that P7 uses
//! to resolve the `service.mapper.findX()` chain — are pinned here through `DefaultParserRegistry`, the path the
//! pipeline actually uses.

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{FactValue, Language, NodeKind, SyntaxFacts};
use gt_domain::port::ParserRegistry;

fn parse(path: &str, src: &str) -> SyntaxFacts {
    let reg = DefaultParserRegistry::new();
    let p = reg
        .parser_for(&Language::new(Language::JAVA))
        .expect("java parser registered");
    p.parse(path, src).expect("parse ok")
}

/// Find a call site by its method name (stable across receiver spellings).
fn call_method<'a>(facts: &'a SyntaxFacts, method: &str) -> &'a gt_domain::model::CallSiteFact {
    facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some(method))
        .unwrap_or_else(|| panic!("expected a call to {method}"))
}

/// String-literal argument slots only — mirrors the src helper so positional-arg assertions stay readable.
fn arg_strings(args: &[FactValue]) -> Vec<Option<String>> {
    args.iter()
        .map(|a| match a {
            FactValue::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// P7 resolves `field.method()` instance calls from the declared field type (the Spring `@Autowired` chain). The
/// `field_types` fact is Java-specific and P7-critical; `interface`/`enum` declaration kinds are the graph's
/// contract surface. Guarded in `src/java.rs` directly; this pins both through the registry.
#[test]
fn extracts_interface_enum_and_field_types() {
    let src = r#"package com.demo;

import com.other.UserRepository;

interface Iface {}
enum Color { RED }

class Svc {
    private UserRepository repo;
    private OrderMapper a, b;
    private List<Order> orders;
}
"#;
    let facts = parse("src/main/java/com/demo/Svc.java", src);

    let iface = facts
        .declarations
        .iter()
        .find(|d| d.kind.as_str() == NodeKind::INTERFACE)
        .expect("interface declaration");
    assert_eq!(iface.name, "Iface");
    assert_eq!(iface.fqn, "com.demo.Iface");

    let enum_decl = facts
        .declarations
        .iter()
        .find(|d| d.kind.as_str() == NodeKind::ENUM)
        .expect("enum declaration");
    assert_eq!(enum_decl.name, "Color");
    assert_eq!(enum_decl.fqn, "com.demo.Color");

    let f = |field: &str| facts.field_types.iter().find(|t| t.field == field);
    assert_eq!(
        f("repo").map(|t| t.type_name.as_str()),
        Some("com.other.UserRepository"),
        "an import that restores the FQN exactly must win over same-package guessing"
    );
    assert_eq!(
        f("a").map(|t| t.type_name.as_str()),
        Some("com.demo.OrderMapper"),
        "same-package completion"
    );
    assert_eq!(
        f("b").map(|t| t.type_name.as_str()),
        Some("com.demo.OrderMapper"),
        "every variable of one declaration must be registered"
    );
    assert_eq!(
        f("orders").map(|t| t.type_name.as_str()),
        Some("com.demo.List"),
        "the generic shell <...> must be stripped"
    );
    assert_eq!(
        facts.field_types.iter().filter(|t| t.class_fqn == "com.demo.Svc").count(),
        4,
        "a field belongs to the class declaring it"
    );
}

/// N+1 detection relies on `CallSiteFact.in_loop` (same trade-off as the PHP / Python parsers). Guarded in
/// `src/java.rs` directly; this pins it through the registry.
#[test]
fn marks_call_sites_inside_loop_bodies() {
    let src = r#"package com.demo;

class Svc {
    void run(java.util.List<Item> items) {
        int total = countAll();
        for (Item i : items) {
            db.query(i);
        }
        while (cond) {
            db.flush();
        }
    }
}
"#;
    let facts = parse("src/main/java/com/demo/Svc.java", src);
    assert_eq!(call_method(&facts, "countAll").in_loop, false, "a call before the loop runs once");
    assert_eq!(
        call_method(&facts, "query").in_loop,
        true,
        "db.query inside the for body is the real N+1"
    );
    assert_eq!(
        call_method(&facts, "flush").in_loop,
        true,
        "db.flush inside the while body is per-row"
    );
}

/// A Java annotation is a **declarative framework call**: FKB must match `@GetMapping("/users")` the same way it
/// matches a PHP route call. The literal arguments must be carried, class-level annotations land on the class
/// FQN and method-level ones on the `Class.method` FQN. Guarded in `src/java.rs` directly; this pins it through the registry.
#[test]
fn annotations_become_call_sites_with_literal_args() {
    let src = r#"package com.demo;

@RequestMapping("/api")
class Ctrl {
    @GetMapping("/users")
    void list() {}

    @PostMapping(value = "/orders", produces = "json")
    void create() {}
}
"#;
    let facts = parse("src/main/java/com/demo/Ctrl.java", src);
    let get = |m: &str| facts.call_sites.iter().find(|c| c.method.as_deref() == Some(m));

    let rm = get("RequestMapping").expect("a class-level annotation must become a call site");
    assert_eq!(rm.callee_text, "RequestMapping");
    assert_eq!(arg_strings(&rm.args), vec![Some("/api".to_string())]);
    assert_eq!(rm.owner_fqn, "com.demo.Ctrl", "a class-level annotation lands on the class");
    assert_eq!(rm.owner_class.as_deref(), Some("com.demo.Ctrl"));

    let gm = get("GetMapping").expect("a method-level annotation must become a call site");
    assert_eq!(arg_strings(&gm.args), vec![Some("/users".to_string())]);
    assert_eq!(gm.owner_fqn, "com.demo.Ctrl.list", "a method-level annotation lands on the method FQN");
    assert_eq!(gm.owner_class.as_deref(), Some("com.demo.Ctrl"));

    // Literals hidden inside `element_value_pair` must still be found (recursive descent).
    let pm = get("PostMapping").expect("a PostMapping annotation");
    assert_eq!(
        arg_strings(&pm.args),
        vec![Some("/orders".to_string()), Some("json".to_string())]
    );
}

/// FKB merges `publishEvent(new X(...))` publishers and `@EventListener void on(X e)` subscribers onto one
/// `Event` node. Both sides must derive the event type from the **constructed object** / **first parameter**
/// respectively, or the event graph silently splits. Guarded in `src/java.rs` directly; this pins both through the registry.
#[test]
fn event_producer_and_subscriber_share_the_event_node() {
    let src = r#"package com.demo;

class Listener {
    @EventListener
    void on(OrderPlacedEvent e) {}
}

class Svc {
    private Publisher publisher;

    void place() {
        publisher.publishEvent(new OrderPlacedEvent(this));
    }
}
"#;
    let facts = parse("src/main/java/com/demo/App.java", src);

    let sub = facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some("EventListener"))
        .expect("an @EventListener must become a call site");
    assert_eq!(
        sub.entity.as_deref(),
        Some("OrderPlacedEvent"),
        "the first parameter type is the event"
    );
    assert_eq!(sub.owner_fqn, "com.demo.Listener.on");

    let pub_ev = facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some("publishEvent"))
        .expect("publishEvent must be captured");
    assert_eq!(
        pub_ev.entity.as_deref(),
        Some("OrderPlacedEvent"),
        "`new X(...)` derives the event type X so producer and subscriber merge"
    );
    assert_eq!(pub_ev.owner_fqn, "com.demo.Svc.place");
}

/// A Repository / Mapper's generic argument is the entity it operates on (the JPA / MyBatis-Plus DAO convention);
/// P7 resolves `service.mapper.findX()` against this. Guarded in `src/java.rs` directly; this pins it through the registry.
#[test]
fn dao_generic_base_infers_the_entity() {
    let src = r#"package com.demo;

interface UserRepository extends JpaRepository<User, Long> {
}

interface OrderMapper extends BaseMapper<Order> {
}
"#;
    let facts = parse("src/main/java/com/demo/Repos.java", src);
    let generic: Vec<(&str, Option<&str>)> = facts
        .call_sites
        .iter()
        .filter(|c| c.callee_text.starts_with("generic."))
        .map(|c| (c.callee_text.as_str(), c.entity.as_deref()))
        .collect();
    // A same-package bare name is completed into an FQN using the DAO's package (a short name hits no node on the graph).
    assert!(
        generic.contains(&("generic.JpaRepository", Some("com.demo.User"))),
        "expected JpaRepository<User> -> com.demo.User, got: {generic:?}"
    );
    assert!(
        generic.contains(&("generic.BaseMapper", Some("com.demo.Order"))),
        "expected BaseMapper<Order> -> com.demo.Order, got: {generic:?}"
    );
}
