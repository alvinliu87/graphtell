//! PHP parsing smoke test: verify that syntax fact extraction is correct.

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{FactValue, Language, NodeKind};
use gt_domain::port::ParserRegistry;

fn parse(src: &str) -> gt_domain::model::SyntaxFacts {
    let reg = DefaultParserRegistry::new();
    let p = reg
        .parser_for(&Language::new(Language::PHP))
        .expect("php parser registered");
    p.parse("test.php", src).expect("parse ok")
}

#[test]
fn extracts_namespace_class_method_and_property() {
    let src = r#"<?php
namespace app\services\order;

use app\dao\order\StoreOrderDao as Dao;

class StoreOrderServices extends BaseServices implements ShouldQueue
{
    protected $table = 'store_order';

    public function getList(array $where)
    {
        return $this->dao->getList($where);
    }
}
"#;
    let facts = parse(src);

    assert_eq!(facts.namespace.as_deref(), Some("app\\services\\order"));

    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.as_str() == NodeKind::CLASS)
        .expect("class");
    assert_eq!(class.fqn, "app\\services\\order\\StoreOrderServices");

    let method = facts
        .declarations
        .iter()
        .find(|d| d.kind.as_str() == NodeKind::METHOD)
        .expect("method");
    assert_eq!(method.fqn, "app\\services\\order\\StoreOrderServices::getList");

    let prop = facts
        .declarations
        .iter()
        .find(|d| d.kind.as_str() == NodeKind::PROPERTY)
        .expect("property");
    assert_eq!(prop.name, "table");
    assert_eq!(
        prop.extra.get("default").and_then(|v| serde_json::from_value::<FactValue>(v.clone()).ok()),
        Some(FactValue::String("store_order".into()))
    );

    // Import: with an alias
    let import = facts.imports.first().expect("import");
    assert_eq!(import.name, "app\\dao\\order\\StoreOrderDao");
    assert_eq!(import.alias.as_deref(), Some("Dao"));

    // Inheritance and implementation
    assert!(facts
        .inheritances
        .iter()
        .any(|i| i.base_name == "BaseServices" && i.kind.as_str() == "Extends"));
    assert!(facts
        .inheritances
        .iter()
        .any(|i| i.base_name == "ShouldQueue" && i.kind.as_str() == "Implements"));

    // Call site
    let call = facts.call_sites.first().expect("call site");
    assert_eq!(call.callee_text, "$this->dao->getList");
    assert_eq!(call.method.as_deref(), Some("getList"));
    assert_eq!(call.owner_fqn, "app\\services\\order\\StoreOrderServices::getList");
}

#[test]
fn extracts_static_calls_and_class_const() {
    let src = r#"<?php
namespace app\services;

class PayServices
{
    public function pay()
    {
        Db::name('store_order')->where('uid', 1)->select();
        event('order.pay_success', [$order]);
        $cls = \app\model\order\StoreOrder::class;
    }
}
"#;
    let facts = parse(src);
    let calls: Vec<&str> = facts.call_sites.iter().map(|c| c.callee_text.as_str()).collect();
    assert!(calls.contains(&"Db::name"), "{calls:?}");
    assert!(calls.iter().any(|c| c.ends_with("->where")), "{calls:?}");
    assert!(calls.contains(&"event"), "{calls:?}");

    let name_call = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "Db::name")
        .expect("Db::name call");
    assert_eq!(name_call.args.first().and_then(|a| a.as_str()), Some("store_order"));

    let event_call = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "event")
        .unwrap();
    assert_eq!(event_call.args.first().and_then(|a| a.as_str()), Some("order.pay_success"));
}

#[test]
fn extracts_config_return_entries() {
    let src = r#"<?php
return [
    'listen' => [
        'order.pay_success' => [
            \app\listener\order\OrderPaySuccessListener::class,
            \app\listener\order\SmsNotifyListener::class,
        ],
    ],
];
"#;
    let facts = parse(src);
    let entry = facts
        .config_entries
        .iter()
        .find(|e| e.key_path == "listen.order.pay_success")
        .expect("config entry");
    assert_eq!(entry.value.array_len(), 2);
    let values = entry.value.array_values();
    let first = values.first().expect("first");
    assert_eq!(
        first.as_str(),
        Some("app\\listener\\order\\OrderPaySuccessListener")
    );
}
