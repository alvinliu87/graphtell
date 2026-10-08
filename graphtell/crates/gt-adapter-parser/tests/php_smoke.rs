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

/// Find a declaration by its node kind (the first one — each test below uses at most one of a kind).
fn decl_by_kind<'a>(facts: &'a gt_domain::model::SyntaxFacts, kind: &str) -> &'a gt_domain::model::Declaration {
    facts
        .declarations
        .iter()
        .find(|d| d.kind.as_str() == kind)
        .unwrap_or_else(|| panic!("expected a {kind} declaration"))
}

/// Find a call site whose source snippet contains `needle` (verbs repeat within a method, so the line is the
/// only thing that tells two call sites apart).
fn call_with_snippet<'a>(
    facts: &'a gt_domain::model::SyntaxFacts,
    needle: &str,
) -> &'a gt_domain::model::CallSiteFact {
    facts
        .call_sites
        .iter()
        .find(|c| c.snippet.as_deref().is_some_and(|s| s.contains(needle)))
        .unwrap_or_else(|| panic!("expected a call site containing {needle:?}"))
}

/// The smoke test exercises the parser through `DefaultParserRegistry` (the wiring the unit tests in
/// `src/php.rs` skip by calling `PhpParser` directly). The declaration-kind surface below is a gap there:
/// only CLASS / METHOD / PROPERTY were pinned, so a regression that dropped interface / trait / enum /
/// function / const extraction — or turned `use Trait` into the wrong relation — would not be caught.
#[test]
fn extracts_declaration_kinds_beyond_class_method_property() {
    let src = r#"<?php
namespace app\services;

interface OrderService {}

trait Timestampable {}

enum Status
{
    case Active;
    case Inactive;
}

function helper()
{
    return 1;
}

class Cfg
{
    const VERSION = '1';
}

class C implements OrderService
{
    use Timestampable;
}
"#;
    let facts = parse(src);

    let iface = decl_by_kind(&facts, "Interface");
    assert_eq!(iface.name, "OrderService");
    assert_eq!(iface.fqn, "app\\services\\OrderService", "interface FQN must be namespace-qualified");

    let trait_decl = decl_by_kind(&facts, "Trait");
    assert_eq!(trait_decl.name, "Timestampable");

    let func = decl_by_kind(&facts, "Function");
    assert_eq!(func.name, "helper");
    assert_eq!(func.fqn, "app\\services\\helper", "a top-level function is namespace-qualified too");

    let const_decl = decl_by_kind(&facts, "Const");
    assert_eq!(const_decl.name, "VERSION");
    assert_eq!(const_decl.fqn, "app\\services\\Cfg::VERSION");
    assert_eq!(const_decl.parent_fqn.as_deref(), Some("app\\services\\Cfg"));

    let enum_decl = decl_by_kind(&facts, "Enum");
    assert_eq!(enum_decl.name, "Status");

    // `use Timestampable;` is a distinct relation (UsesTrait), not Extends / Implements.
    assert!(
        facts.inheritances.iter().any(|i| i.base_name == "Timestampable" && i.kind.as_str() == "UsesTrait"),
        "`use Trait` must land as a UsesTrait inheritance: {:?}",
        facts.inheritances.iter().map(|i| (i.base_name.as_str(), i.kind.as_str())).collect::<Vec<_>>()
    );
    // The interface is still implemented (Implements), proving the three relations stay separate.
    assert!(
        facts.inheritances.iter().any(|i| i.base_name == "OrderService" && i.kind.as_str() == "Implements"),
        "interface implementation must remain Implements"
    );
}

/// P7 turns a chained facade verb (`Db::name('goods')->insert()`) into `WritesDb` / `ReadsDb` only because the
/// terminal verb carries the table name picked up upstream. This is guarded in `src/php.rs` via `PhpParser`
/// directly; this registry-level test pins it through the public `parse` path.
#[test]
fn extracts_db_table_from_chained_facade_call() {
    let src = r#"<?php
namespace app\services;

class Svc
{
    public function run($data)
    {
        Db::name('goods')->where('id', 1)->insert($data);
    }
}
"#;
    let facts = parse(src);
    let insert = facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some("insert"))
        .expect("the insert call site must be captured");
    assert_eq!(
        insert.db_table.as_deref(),
        Some("goods"),
        "a chained verb must carry the table name from the upstream name('goods'), otherwise P7 cannot land a WritesDb"
    );
}

/// N+1 detection relies on `CallSiteFact.in_loop`: a call inside a loop body runs per-row, a call in the loop
/// *condition* does not. Guarded in `src/php.rs` directly; this pins it through the registry.
#[test]
fn marks_call_sites_inside_loop_bodies() {
    let src = r#"<?php
class S
{
    public function run($list)
    {
        $conf = Db::name('config')->find();
        foreach ($list as $item) {
            $user = Db::name('user')->where('id', $item['id'])->find();
        }
        while ($row = $q->fetch()) {
            $this->dao->save($row);
        }
        return $conf;
    }
}
"#;
    let facts = parse(src);
    assert_eq!(call_with_snippet(&facts, "'config'").in_loop, false, "a call before the loop runs once");
    assert_eq!(call_with_snippet(&facts, "'user'").in_loop, true, "a call inside the foreach body is the real N+1");
    assert_eq!(call_with_snippet(&facts, "fetch()").in_loop, false, "a call in the loop condition is not inside the body");
    assert_eq!(call_with_snippet(&facts, "save(").in_loop, true, "a call inside the while body is per-row");
}

/// Signature-quality judgement keys off `sign_compares`, which must collect **only** loose comparisons
/// (`==` / `!=`) and skip strict ones (`===` / `!==`). Guarded in `src/php.rs` directly; this pins it through
/// the registry.
#[test]
fn extracts_only_loose_signature_comparisons() {
    let src = r#"<?php
class P
{
    public function respond($params)
    {
        if ($this->CreatedSign($params) != $params['sign']) {
            return -1;
        }
        if ($sign === $signature) {
            return -1;
        }
        return 0;
    }
}
"#;
    let facts = parse(src);
    assert_eq!(facts.sign_compares.len(), 1, "strict === must not be collected");
    let c = &facts.sign_compares[0];
    assert_eq!(c.operator, "!=");
    assert!(c.left.contains("CreatedSign"));
    assert!(c.right.contains("['sign']"));
    assert_eq!(c.owner_fqn, "P::respond");
}

/// CORS reflected-origin detection keys off `header_assignments`: only a left side that is a response-header
/// subscript key (e.g. `Access-Control-Allow-Origin`) is captured, not arbitrary header writes like
/// `Access-Control-Allow-Credentials`. Guarded in `src/php.rs` directly; this pins it through the registry.
#[test]
fn extracts_reflected_cors_header_assignment() {
    let src = r#"<?php
Route::miss(function () {
    $header['Access-Control-Allow-Origin'] = app()->request->header('origin');
    $header['Access-Control-Allow-Credentials'] = 'true';
});
"#;
    let facts = parse(src);
    // Only Allow-Origin is captured, not Allow-Credentials (a non-reflected header).
    assert_eq!(facts.header_assignments.len(), 1, "only the reflected Allow-Origin header must be captured");
    let ha = &facts.header_assignments[0];
    assert_eq!(ha.key, "access-control-allow-origin");
    let lower = ha.rhs_snippet.to_ascii_lowercase();
    assert!(
        lower.contains("header(") && lower.contains("origin"),
        "the right-hand side must read the request origin (the Cors phase locates the annotation from it): {lower}"
    );
}

/// P9 Taint backward-tracks within one function from local variable assignments (`$sql = ...`, `$sql .= ...`).
/// Only bare-variable left sides count — `$this->conf` / `$list['k']` are not local variables. Guarded in
/// `src/php.rs` directly; this pins it through the registry.
#[test]
fn extracts_local_variable_assignments() {
    let src = r#"<?php
class M
{
    public function run()
    {
        $sql = 'select 1';
        $exec = str_replace('@table', $t, $sql);
        $this->conf = $sql;
        $list['k'] = $sql;
    }
}
"#;
    let facts = parse(src);
    let vars: Vec<&str> = facts.variable_assignments.iter().map(|a| a.var.as_str()).collect();
    // Only bare-variable left sides are collected: `$this->conf` / `$list['k']` are not local variables.
    assert_eq!(vars, vec!["sql", "exec"], "only local variables, not member/subscript assignments");
    let exec = &facts.variable_assignments[1];
    assert!(
        exec.rhs.contains("$sql"),
        "the verbatim right-hand side must keep the variable reference: {}",
        exec.rhs
    );
    assert!(exec.owner_fqn.ends_with("run"), "owner_fqn={}", exec.owner_fqn);
}
