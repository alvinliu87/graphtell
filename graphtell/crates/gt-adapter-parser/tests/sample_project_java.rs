//! Parser tests against the real Java sample corpus (`mall`, a Spring Boot / MyBatis project).
//!
//! These mirror `sample_project_php.rs`: they verify the *facts* tree-sitter extracts from real
//! source, not the semantics. The `*_smoke.rs` files already pin the same parser contracts against
//! tiny synthesized snippets; this file pins them on a real, oversized checkout where the interesting
//! failures live:
//!
//! * namespace -> FQN derivation through the real `com.macro.mall.*` package tree
//! * `@Autowired` field-type resolution back to a **fully-qualified** type (P7 looks `MapsTo` up by the
//!   receiver's FQN — a short name finds nothing, so `orderService.list(...)` would yield no edge)
//! * class-level vs method-level `@RequestMapping` routing onto the right FQN
//! * interface (DAO mapper) FQN + method declaration

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{Language, SyntaxFacts};
use gt_domain::port::ParserRegistry;
use gt_sample_support::{java_sample_inner_dir, java_sample_root, missing_hint_named};

fn parse_java(rel: &str) -> Option<SyntaxFacts> {
    let root = java_sample_root()?;
    let path = root.join(java_sample_inner_dir()).join(rel);
    let content = std::fs::read_to_string(&path).ok()?;
    let registry = DefaultParserRegistry::new();
    let parser = registry
        .parser_for(&Language::new(Language::JAVA))
        .expect("the java parser is registered");
    parser.parse(path.to_str().unwrap(), &content).ok()
}

#[test]
#[ignore = "needs the java sample (mall), which is not committed (too large to ship with the repo)"]
fn parses_controller_class_fqn_and_class_level_route() {
    let Some(facts) = parse_java("mall-admin/src/main/java/com/macro/mall/controller/OmsOrderController.java")
    else {
        panic!("{}", missing_hint_named("java"));
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "OmsOrderController")
        .expect("expected the OmsOrderController class to be parsed");
    assert_eq!(
        class.fqn, "com.macro.mall.controller.OmsOrderController",
        "the FQN must be derived from the package + class"
    );

    // `@RequestMapping("/order")` is a class-level annotation -> call site owned by the class FQN.
    let rm = facts
        .call_sites
        .iter()
        .find(|c| {
            c.method.as_deref() == Some("RequestMapping") && c.owner_fqn == class.fqn
        })
        .expect("the class-level @RequestMapping must become a call site");
    assert!(
        rm.args.iter().any(|a| a.as_str() == Some("/order")),
        "the class-level route prefix \"/order\" must be carried: {:?}",
        rm.args
    );
}

/// P7 resolves `service.mapper.findX()` against the injected field type, and `MapsTo` hangs off an FQN —
/// so the `@Autowired OmsOrderService orderService` field must resolve to its **fully qualified** name,
/// not the bare `OmsOrderService`. The import `com.macro.mall.service.OmsOrderService` is what restores it.
#[test]
#[ignore = "needs the java sample (mall), which is not committed (too large to ship with the repo)"]
fn parses_autowired_field_type_as_qualified_fqn() {
    let Some(facts) = parse_java("mall-admin/src/main/java/com/macro/mall/controller/OmsOrderController.java")
    else {
        panic!("{}", missing_hint_named("java"));
    };
    let ft = facts
        .field_types
        .iter()
        .find(|t| t.field == "orderService")
        .expect("the @Autowired `orderService` field must be recorded");
    assert_eq!(
        ft.type_name, "com.macro.mall.service.OmsOrderService",
        "an import that restores the FQN exactly must win over same-package guessing"
    );
    assert_eq!(
        ft.class_fqn, "com.macro.mall.controller.OmsOrderController",
        "the field must be attributed to the declaring class"
    );
}

/// A method-level `@RequestMapping(value = "/list", method = RequestMethod.GET)` is a **declarative
/// framework call** that must land on the `Class.method` FQN (not merely the class), so FKB can match
/// the route to the handler method precisely.
#[test]
#[ignore = "needs the java sample (mall), which is not committed (too large to ship with the repo)"]
fn method_level_route_lands_on_method_fqn() {
    let Some(facts) = parse_java("mall-admin/src/main/java/com/macro/mall/controller/OmsOrderController.java")
    else {
        panic!("{}", missing_hint_named("java"));
    };
    let method = facts
        .declarations
        .iter()
        .find(|d| {
            d.kind.is("Method") && d.name == "list" && d.parent_fqn.as_deref() == Some("com.macro.mall.controller.OmsOrderController")
        })
        .expect("expected the list() method to be parsed");

    let route = facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some("RequestMapping") && c.owner_fqn == method.fqn)
        .expect("the method-level @RequestMapping must become a call site owned by Class.method");
    assert!(
        route.args.iter().any(|a| a.as_str() == Some("/list")),
        "the \"/list\" path argument must be carried: {:?}",
        route.args
    );
}

/// A MyBatis mapper is an `interface` whose FQN and methods are the nodes every DAO edge hangs on.
/// Synthesized sources can't prove the real `com.macro.mall.mapper` package resolves.
#[test]
#[ignore = "needs the java sample (mall), which is not committed (too large to ship with the repo)"]
fn parses_mapper_interface_fqn_and_methods() {
    let Some(facts) = parse_java("mall-mbg/src/main/java/com/macro/mall/mapper/OmsOrderMapper.java") else {
        panic!("{}", missing_hint_named("java"));
    };
    let iface = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Interface") && d.name == "OmsOrderMapper")
        .expect("expected the OmsOrderMapper interface to be parsed");
    assert_eq!(
        iface.fqn, "com.macro.mall.mapper.OmsOrderMapper",
        "the interface FQN must be derived from the package + interface"
    );
    let method = facts
        .declarations
        .iter()
        .find(|d| {
            d.kind.is("Method")
                && d.name == "selectByPrimaryKey"
                && d.parent_fqn.as_deref() == Some(iface.fqn.as_str())
        })
        .expect("expected the selectByPrimaryKey() method to be parsed");
    assert_eq!(
        method.fqn, "com.macro.mall.mapper.OmsOrderMapper.selectByPrimaryKey",
        "a method FQN must be derived from namespace + interface + method"
    );
}
