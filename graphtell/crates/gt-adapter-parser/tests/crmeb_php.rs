//! 解析器对真实 CRMEB 源文件的测试。
//!
//! 这些用例验证 tree-sitter 提取**事实**的准确度，而不是语义：
//! * 命名空间 → FQN 推导
//! * 模型继承链（ transcend vendor 的末端类）
//! * 配置数组（`event.php` / `provider.php`）的条目提取
//! * 路由闭包里的 `Route::post(...)` 调用点（这是最容易漏的）

use std::path::PathBuf;

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{Language, SyntaxFacts};
use gt_domain::port::ParserRegistry;

fn sample_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    // 从 `CARGO_MANIFEST_DIR` 向上逐层查找 `samples/**/CRMEB-master`：
    // 先试 `samples/CRMEB-master`，再遍历一层子目录（样本按技术栈分目录放置时也能命中）。
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let samples = cur.join("samples");
        let direct = samples.join("CRMEB-master");
        if direct.is_dir() {
            return Some(direct.canonicalize().unwrap_or(direct));
        }
        let mut hits: Vec<PathBuf> = std::fs::read_dir(&samples)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("CRMEB-master"))
            .filter(|p| p.is_dir())
            .collect();
        hits.sort();
        if let Some(hit) = hits.into_iter().next() {
            return Some(hit.canonicalize().unwrap_or(hit));
        }
        if !cur.pop() {
            break;
        }
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
#[ignore = "需要未入库的 CRMEB 样本（体积过大，不随仓库分发）"]
fn parses_model_namespace_and_fqn() {
    let Some(facts) = parse_php("crmeb/app/model/order/StoreOrder.php") else {
        panic!("CRMEB 样本缺失：该测试已标记 #[ignore]，用 --ignored 运行时需要设置 GRAPHTELL_SAMPLE_DIR 或放置 samples/**/CRMEB-master");
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

/// ThinkPHP 模型**不用** `$table` 承载表名，而是 `$name`（`protected $name = 'store_order'`，
/// 表前缀由配置另给）；`$pk` 同理给出主键。P5 识别「模型 → 表」依赖这两个属性，
/// 这里守住"属性值必须被提取"——只断言属性存在是不够的，空值等于没识别到表名。
#[test]
#[ignore = "需要未入库的 CRMEB 样本（体积过大，不随仓库分发）"]
fn parses_model_name_and_pk_properties() {
    let Some(facts) = parse_php("crmeb/app/model/order/StoreOrder.php") else {
        panic!("CRMEB 样本缺失：该测试已标记 #[ignore]，用 --ignored 运行时需要设置 GRAPHTELL_SAMPLE_DIR 或放置 samples/**/CRMEB-master");
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "StoreOrder")
        .expect("应解析出 StoreOrder 类");
    // 属性是独立的 Declaration，parent_fqn 指向所属类
    let prop = |name: &str| {
        facts.declarations.iter().find(|d| {
            d.kind.is("Property")
                && d.name == name
                && d.parent_fqn.as_deref() == Some(class.fqn.as_str())
        })
    };
    // 属性默认值存在 `extra["default"]`（`extra["value"]` 是 const 的键，别混用），
    // 序列化形态是 `FactValue::String` → `{"String": "..."}`。
    // `FactValue` 以 `#[serde(tag = "t", content = "v")]` 序列化：`{"t":"String","v":"..."}`。
    fn default_of(d: &gt_domain::model::Declaration) -> Option<String> {
        let v = d.extra.get("default")?;
        if v.get("t").and_then(|t| t.as_str()) != Some("String") {
            return None;
        }
        v.get("v").and_then(|x| x.as_str()).map(|s| s.to_string())
    }
    let name = prop("name").expect("StoreOrder 应有 protected $name（表名来源）");
    assert_eq!(
        default_of(name).as_deref(),
        Some("store_order"),
        "$name 的属性值必须被提取"
    );
    let pk = prop("pk").expect("StoreOrder 应有 protected $pk（主键）");
    assert_eq!(default_of(pk).as_deref(), Some("id"), "$pk 的属性值必须被提取");
}

#[test]
#[ignore = "需要未入库的 CRMEB 样本（体积过大，不随仓库分发）"]
fn parses_event_php_config_entries() {
    let Some(facts) = parse_php("crmeb/app/event.php") else {
        panic!("CRMEB 样本缺失：该测试已标记 #[ignore]，用 --ignored 运行时需要设置 GRAPHTELL_SAMPLE_DIR 或放置 samples/**/CRMEB-master");
    };
    // 顶层 `return [...]` 里的 'listen' 应被提取成 config_entry。
    // CRMEB 的 listen 是**平铺**的：`'事件名' => [监听器类...]`（不是 `listen.order.pay_success`
    // 这种嵌套分组），数组元素以 `.0` 形式展开成独立条目。
    let pay_success = facts
        .config_entries
        .iter()
        .find(|c| c.key_path == "listen.OrderPaySuccessListener")
        .expect("event.php 应提取出 listen.OrderPaySuccessListener 配置项");
    assert!(
        !pay_success.value.array_values().is_empty() || pay_success.value.as_str().is_some(),
        "OrderPaySuccessListener 事件应带有监听器列表"
    );
    // 监听器类本身也要落进条目里，否则事件 → 监听器这条边无从建立。
    assert!(
        facts
            .config_entries
            .iter()
            .any(|c| c.key_path.starts_with("listen.OrderPaySuccessListener.")
                && c.value.as_str() == Some("app\\listener\\order\\OrderPaySuccessListener")),
        "监听器类 app\\listener\\order\\OrderPaySuccessListener 应被提取"
    );
}

#[test]
#[ignore = "需要未入库的 CRMEB 样本（体积过大，不随仓库分发）"]
fn parses_provider_php_bindings() {
    let Some(facts) = parse_php("crmeb/app/provider.php") else {
        panic!("CRMEB 样本缺失：该测试已标记 #[ignore]，用 --ignored 运行时需要设置 GRAPHTELL_SAMPLE_DIR 或放置 samples/**/CRMEB-master");
    };
    // 容器绑定是**顶层**的 `'think\Request' => Request::class`（不是包在 `bind` / `providers`
    // 子数组里）——ThinkPHP 的 provider.php 直接返回接口 → 实现的映射表。
    // 这是 P7 动态解析的关键：`app(Request::class)` 要能落到 `app\Request`。
    let entry = |key: &str| {
        facts
            .config_entries
            .iter()
            .find(|c| c.key_path == key)
            .unwrap_or_else(|| panic!("provider.php 应解析出绑定 {key}"))
    };
    for (interface, impl_hint) in [
        ("think\\Request", "Request"),
        ("think\\exception\\Handle", "ExceptionHandle"),
    ] {
        let e = entry(interface);
        assert!(
            e.value.as_str().is_some_and(|v| !v.is_empty()),
            "{interface} 应绑定到非空的实现（{impl_hint}）"
        );
    }
}

#[test]
#[ignore = "需要未入库的 CRMEB 样本（体积过大，不随仓库分发）"]
fn parses_route_call_sites_inside_closures() {
    let Some(facts) = parse_php("crmeb/app/api/route/v1.php") else {
        panic!("CRMEB 样本缺失：该测试已标记 #[ignore]，用 --ignored 运行时需要设置 GRAPHTELL_SAMPLE_DIR 或放置 samples/**/CRMEB-master");
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
