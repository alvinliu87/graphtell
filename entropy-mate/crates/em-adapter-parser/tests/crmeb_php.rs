//! 解析器对真实 CRMEB 源文件的测试。
//!
//! 这些用例验证 tree-sitter 提取**事实**的准确度，而不是语义：
//! * 命名空间 → FQN 推导
//! * 模型继承链（ transcend vendor 的末端类）
//! * 配置数组（`event.php` / `provider.php`）的条目提取
//! * 路由闭包里的 `Route::post(...)` 调用点（这是最容易漏的）

use std::path::PathBuf;

use em_adapter_parser::DefaultParserRegistry;
use em_domain::model::{Language, SyntaxFacts};
use em_domain::port::ParserRegistry;

fn sample_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ENTROPY_MATE_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../分析样本/CRMEB-master");
    if candidate.is_dir() {
        return Some(candidate.canonicalize().unwrap_or(candidate));
    }
    None
}

fn parse_php(rel: &str) -> Option<SyntaxFacts> {
    let root = sample_root()?;
    let path = root.join(rel);
    let content = std::fs::read_to_string(&path).ok()?;
    let registry = DefaultParserRegistry::new();
    let parser = registry
        .parser_for(&Language::new(Language::PHP))
        .expect("php parser 已注册");
    parser.parse(path.to_str().unwrap(), &content).ok()
}

#[test]
fn parses_model_namespace_and_fqn() {
    let Some(facts) = parse_php("crmeb/app/model/order/StoreOrder.php") else {
        eprintln!("跳过：未找到 CRMEB 样本");
        return;
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "StoreOrder")
        .expect("应解析出 StoreOrder 类");
    assert_eq!(
        class.fqn, "app\\model\\order\\StoreOrder",
        "FQN 应由 namespace + class 推出"
    );
    // 继承链：StoreOrder → BaseModel → Model（末端在 vendor）
    let inherits = facts
        .inheritances
        .iter()
        .any(|i| i.child_fqn == class.fqn && (i.base_name.contains("BaseModel") || i.base_name.contains("Model")));
    assert!(
        inherits,
        "应记录继承（即便末端类在 vendor 也被排除）"
    );
}

#[test]
fn parses_model_table_property() {
    let Some(facts) = parse_php("crmeb/app/model/order/StoreOrder.php") else {
        eprintln!("跳过：未找到 CRMEB 样本");
        return;
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "StoreOrder")
        .expect("应解析出 StoreOrder 类");
    // 属性是独立的 Declaration，parent_fqn 指向所属类
    let table = facts
        .declarations
        .iter()
        .find(|d| {
            d.kind.is("Property")
                && d.name == "table"
                && d.parent_fqn.as_deref() == Some(class.fqn.as_str())
        });
    assert!(
        table.is_some(),
        "StoreOrder 应有 protected $table 属性（P5 据此识别表名）"
    );
    let value = table
        .unwrap()
        .extra
        .get("value")
        .and_then(|v| v.as_str());
    assert!(
        value.is_some(),
        "$table 的属性值必须被提取（如 eb_store_order）"
    );
}

#[test]
fn parses_event_php_config_entries() {
    let Some(facts) = parse_php("crmeb/app/event.php") else {
        eprintln!("跳过：未找到 CRMEB 样本");
        return;
    };
    // 顶层 return [...] 里的 'listen' 配置应被提取成 config_entry
    let listen = facts
        .config_entries
        .iter()
        .find(|c| c.key_path == "listen.order.pay_success")
        .expect("event.php 应提取出 listen.order.pay_success 配置项");
    // 值是数组（监听器类列表）
    assert!(
        !listen.value.array_values().is_empty() || listen.value.as_str().is_some(),
        "order.pay_success 事件应带有监听器列表"
    );
}

#[test]
fn parses_provider_php_bindings() {
    let Some(facts) = parse_php("crmeb/app/provider.php") else {
        eprintln!("跳过：未找到 CRMEB 样本");
        return;
    };
    // 容器绑定形如 'order_services' => StoreOrderServices::class
    let has_binding = facts
        .config_entries
        .iter()
        .any(|c| c.key_path.starts_with("bind.") || c.key_path.starts_with("providers."));
    assert!(
        has_binding,
        "provider.php 应解析出容器绑定（P7 动态解析的关键）"
    );
}

#[test]
fn parses_route_call_sites_inside_closures() {
    let Some(facts) = parse_php("crmeb/app/api/route/v1.php") else {
        eprintln!("跳过：未找到 CRMEB 样本");
        return;
    };
    // 路由注册写在闭包内：`Route::post('apple_login', 'Login/appleLogin')`
    let route_calls = facts
        .call_sites
        .iter()
        .filter(|c| c.callee_text.contains("Route::post") || c.callee_text.contains("Route::get"))
        .collect::<Vec<_>>();
    assert!(
        !route_calls.is_empty(),
        "路由文件里的 Route::post/get 调用点必须被收集（即便在闭包内）"
    );
    let apple = route_calls
        .iter()
        .find(|c| c.args.iter().any(|a| a.as_str() == Some("apple_login")));
    assert!(
        apple.is_some(),
        "应能定位 apple_login 路由的调用点"
    );
}
