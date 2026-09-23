//! Java 解析器 —— **第二语言**，用于验证语言层抽象是否真的可插拔。
//!
//! 只做语法层（P2）该做的事：把 Java 语法树翻译成语言无关的 [`SyntaxFacts`]。
//! 语义层（Spring 的 `@GetMapping` / MyBatis Mapper / JPA 实体）应由 FKB 声明，
//! 与 ThinkPHP / Laravel 走同一套机制。
//!
//! 注册进 `DefaultParserRegistry` 后，流水线无需任何改动即可处理 Java 工程。

use std::cell::RefCell;

use gt_domain::error::Result;
use gt_domain::model::{
    CallSiteFact, Declaration, EdgeKind, FactValue, FieldTypeFact, ImportFact, InheritanceFact,
    Language, NodeKind, Span, SyntaxFacts,
};
use gt_domain::port::LanguageParser;
use tree_sitter::{Node, Parser};

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

pub struct JavaParser {
    language: tree_sitter::Language,
}

impl JavaParser {
    pub fn new() -> Result<Self> {
        let language: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
        Ok(Self { language })
    }
}

impl LanguageParser for JavaParser {
    fn language(&self) -> Language {
        Language::new(Language::JAVA)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["java"]
    }

    fn parse(&self, _path: &str, source: &str) -> Result<SyntaxFacts> {
        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                // 语言在构造时已校验，这里失败属于编程错误
                p.set_language(&self.language).expect("java language");
                p
            });
            parser.parse(source, None)
        });
        let Some(tree) = tree else {
            return Ok(SyntaxFacts::default());
        };

        let mut out = SyntaxFacts::default();
        let src = source.as_bytes();
        let mut package: Option<String> = None;
        let mut stack: Vec<String> = Vec::new();
        let mut loop_depth: u32 = 0;
        walk(
            tree.root_node(),
            src,
            &mut package,
            &mut out,
            &mut stack,
            &mut loop_depth,
        );
        Ok(out)
    }

    /// Java 的命名空间分隔符是 `.`（PHP 是 `\`）。
    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    /// Java 的成员分隔符是 `.`（PHP 是 `::`）。
    fn member_separator(&self) -> &'static str {
        "."
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["pom.xml", "build.gradle", "build.gradle.kts"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &["target", "build", ".gradle", ".m2"]
    }
}

fn walk(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &mut Vec<String>,
    loop_depth: &mut u32,
) {
    // 循环语句：只有 **body** 子树算「循环内」（与 PHP 侧一致 —— 条件 / 更新表达式
    // 不算逐条执行的部分）。`in_loop` 是图里唯一表达「这段代码会被执行 N 次」的标记，
    // N+1 规则依赖它。
    if is_loop(node) {
        let body_id = node.child_by_field_name("body").map(|b| b.id());
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let in_body = Some(child.id()) == body_id;
            if in_body {
                *loop_depth += 1;
            }
            walk(child, src, package, out, stack, loop_depth);
            if in_body {
                *loop_depth -= 1;
            }
        }
        return;
    }
    match node.kind() {
        "package_declaration" => {
            if let Some(name) = node.named_child(0).and_then(|n| text(n, src)) {
                *package = Some(name);
            }
        }
        "import_declaration" => {
            if let Some(name) = node.named_child(0).and_then(|n| text(n, src)) {
                out.imports.push(ImportFact {
                    alias: None,
                    name,
                    span: span_of(node),
                });
            }
        }
        "class_declaration" | "interface_declaration" | "enum_declaration"
        | "record_declaration" => {
            if let Some(fqn) = declare_type(node, src, package, out, stack) {
                collect_supertypes(node, src, &fqn, out);
                // 类级注解（如 `@RequestMapping("/api")` 作为路径前缀）
                // owner_class 即类本身（类级注解的 owner_fqn 已是类，无需再切）。
                collect_annotations(node, src, &fqn, &fqn, out);
                stack.push(fqn);
                recurse(node, src, package, out, stack, loop_depth);
                stack.pop();
                return;
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(fqn) = declare_member(node, src, out, stack) {
                stack.push(fqn);
                recurse(node, src, package, out, stack, loop_depth);
                stack.pop();
                return;
            }
        }
        "method_invocation" => {
            collect_call(node, src, out, stack, loop_depth);
        }
        "field_declaration" => {
            // 字段级注解（`@Value` / `@Autowired` / `@TableField` …）归属所属类：
            // owner_fqn 用类 FQN（字段属于类而非方法），owner_class 同为该类的 FQN。
            if let Some(class_fqn) = stack.last().cloned() {
                collect_annotations(node, src, &class_fqn, &class_fqn, out);
                collect_field_type(node, src, &class_fqn, out);
            }
        }
        _ => {}
    }
    recurse(node, src, package, out, stack, loop_depth);
}

fn recurse(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &mut Vec<String>,
    loop_depth: &mut u32,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, package, out, stack, loop_depth);
    }
}

/// Java 的循环语句（`for` / 增强 `for` / `while` / `do-while`）。
fn is_loop(node: Node) -> bool {
    matches!(
        node.kind(),
        "for_statement" | "enhanced_for_statement" | "while_statement" | "do_statement"
    )
}

/// 登记类 / 接口 / 枚举 / record，返回其 FQN。
fn declare_type(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &[String],
) -> Option<String> {
    let name = text(node.child_by_field_name("name")?, src)?;
    let fqn = match stack.last() {
        // 内部类：`Outer.Inner`
        Some(outer) => format!("{}.{}", outer, name),
        None => match package {
            Some(p) => format!("{}.{}", p, name),
            None => name.clone(),
        },
    };
    let kind = match node.kind() {
        "interface_declaration" => NodeKind(NodeKind::INTERFACE.to_string()),
        "enum_declaration" => NodeKind(NodeKind::ENUM.to_string()),
        _ => NodeKind(NodeKind::CLASS.to_string()),
    };
    out.declarations.push(Declaration {
        kind,
        name,
        fqn: fqn.clone(),
        parent_fqn: stack.last().cloned(),
        span: span_of(node),
        extra: serde_json::Value::Null,
    });
    Some(fqn)
}

/// `extends` / `implements` → 继承事实。
///
/// 附带：JPA Repository / MyBatis Mapper 的**泛型实参**就是它操作的实体
/// （`interface UserRepository extends JpaRepository<User, Long>` → `User`）。
/// 这是「哪个 DAO 操作哪张表」的唯一静态线索，记成合成调用点交给 FKB 判定
/// （哪些基类算 DAO、实体怎么映射到表，都是框架知识）。
fn collect_supertypes(node: Node, src: &[u8], fqn: &str, out: &mut SyntaxFacts) {
    // 整棵声明子树里第一组 `type_arguments`（`<User, Long>` → `User`）；
    // 类自身的类型参数用的是 `type_parameters`，不会与此处混淆。
    let entity = first_generic_arg(node, src);
    // 一律按 **kind** 遍历，不用 `child_by_field_name`：接口的 `extends` 是
    // `extends_interfaces` 节点且**不带字段名**，按字段取永远取不到 —— 这正是
    // 「接口继承接口」（JPA Repository / MyBatis Mapper 的写法）此前整条丢失的原因。
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "superclass" => {
                if let Some(base) = type_name_of(child, src) {
                    out.inheritances.push(InheritanceFact {
                        child_fqn: fqn.to_string(),
                        base_name: base.clone(),
                        kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
                        span: span_of(child),
                    });
                    push_generic_entity(&base, &entity, fqn, child, src, out);
                }
            }
            "interfaces" | "extends_interfaces" => {
                let mut c2 = child.walk();
                for t in child.named_children(&mut c2) {
                    let bases: Vec<String> = if t.kind() == "type_list" {
                        let mut c3 = t.walk();
                        t.named_children(&mut c3)
                            .filter_map(|n| type_name_of(n, src))
                            .collect()
                    } else {
                        type_name_of(t, src).into_iter().collect()
                    };
                    for base in bases {
                        out.inheritances.push(InheritanceFact {
                            child_fqn: fqn.to_string(),
                            base_name: base.clone(),
                            kind: EdgeKind(EdgeKind::IMPLEMENTS.to_string()),
                            span: span_of(t),
                        });
                        push_generic_entity(&base, &entity, fqn, t, src, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// 取一个类型节点的**裸名**：`generic_type`（`BaseMapper<Order>`）取其中的
/// `type_identifier`（`BaseMapper`），避免把泛型实参混进基类名。
fn type_name_of(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "generic_type" {
        let mut c = node.walk();
        return node
            .named_children(&mut c)
            .find(|c| c.kind() == "type_identifier")
            .and_then(|c| text(c, src));
    }
    text(node, src)
}

/// 把源码里的**裸类型名**还原成 FQN。
///
/// 顺序：① 带点号 → 原样；② 本文件的 `import`（含别名）→ 被导入的 FQN；
/// ③ 同包（同包引用没有 import）→ `{所在类的包}.{名字}`。
///
/// 为什么必须在 parser 做：P7 的读 / 写动词分类拿接收者类型查 `MapsTo`，而 `MapsTo`
/// 挂着的是 **FQN**；短名查不到 ⇒ `mapper.insert()` 落不出 `WritesDb`。
/// 只按同包补会在「service 与 mapper 不同包」时拼出错误的 FQN（宁可缺不可猜，
/// 但能靠 import 精确还原时就应该精确）。
fn resolve_java_type(name: &str, out: &SyntaxFacts, class_fqn: &str) -> String {
    if name.contains('.') {
        return name.to_string();
    }
    for imp in out.imports.iter() {
        if let Some(alias) = &imp.alias {
            if alias == name {
                return imp.name.clone();
            }
        }
        let last = imp.name.rsplit('.').next().unwrap_or(&imp.name);
        if last == name {
            return imp.name.clone();
        }
    }
    match class_fqn.rsplit_once('.') {
        Some((pkg, _)) => format!("{}.{}", pkg, name),
        None => name.to_string(),
    }
}

/// 取子树里第一组 `type_arguments` 的首个类型标识符（`<User, Long>` → `User`）。
fn first_generic_arg(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "type_arguments" {
        let mut c = node.walk();
        return node
            .named_children(&mut c)
            .find(|c| c.kind() == "type_identifier")
            .and_then(|c| text(c, src));
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(v) = first_generic_arg(child, src) {
            return Some(v);
        }
    }
    None
}

/// 把「DAO 泛型实参 = 实体」记成合成调用点。
///
/// `callee_text` 用 `generic.` 前缀（**不带冒号** —— FKB 的 callee 里单冒号会被
/// 解释成 `receiver:method`），`entity` 存实体**短名**，由 FKB 的
/// `resolve: class_const` 经 import 短名索引还原成 FQN。
fn push_generic_entity(
    base: &str,
    entity: &Option<String>,
    fqn: &str,
    node: Node,
    src: &[u8],
    out: &mut SyntaxFacts,
) {
    let Some(entity) = entity else { return };
    // 裸名（最常见：实体与 DAO **同包**，故没有 import）必须补成 FQN ——
    // 图里只有 FQN 能命中，而短名索引只收 import，同包引用根本不在里面
    // （这正是此前 `find_by_name` 落空、Link 不产边的原因）。
    let entity = resolve_java_type(entity, out, fqn);
    out.call_sites.push(CallSiteFact {
        owner_fqn: fqn.to_string(),
        owner_class: Some(fqn.to_string()),
        callee_text: format!("generic.{}", base),
        receiver: None,
        // 刻意留空：FKB 的裸 callee 模式会**按方法名**匹配，留空才不会误命中
        method: None,
        args: Vec::new(),
        span: span_of(node),
        snippet: None,
        db_table: None,
        in_loop: false,
        entity: Some(entity.clone()),
    });
    let _ = src;
}

/// 字段声明 `Type name;` / `Type a, b;` → 记录每条字段与其声明类型。
///
/// 仅取**裸类型名**（剔除泛型 `<...>` 与数组 `[]`），泛型参数在 P2 按
/// `import` 还原时通常无法定位到工程内类，留待需要时再扩展。
fn collect_field_type(node: Node, src: &[u8], class_fqn: &str, out: &mut SyntaxFacts) {
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    let Some(raw) = text(type_node, src) else {
        return;
    };
    let type_name = raw
        .split(['<', '['])
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string();
    if type_name.is_empty() {
        return;
    }
    let type_name = resolve_java_type(&type_name, out, class_fqn);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        if let Some(name) = opt_text(child.child_by_field_name("name"), src) {
            out.field_types.push(FieldTypeFact {
                class_fqn: class_fqn.to_string(),
                field: name,
                type_name: type_name.clone(),
                span: span_of(child),
            });
        }
    }
}

/// 方法 / 构造器 → 成员声明。返回方法 FQN（供调用者把方法压栈，使方法体内的
/// 调用点 `owner_fqn` 精确到 `类.方法` 而非仅类）。
fn declare_member(node: Node, src: &[u8], out: &mut SyntaxFacts, stack: &[String]) -> Option<String> {
    let name = opt_text(node.child_by_field_name("name"), src)?;
    let class_fqn = stack.last().cloned()?;
    let fqn = format!("{}.{}", class_fqn, name);
    out.declarations.push(Declaration {
        kind: NodeKind(NodeKind::METHOD.to_string()),
        name: name.clone(),
        // 成员分隔符由 `member_separator()` 决定（Java `.` / PHP `::`）
        fqn: fqn.clone(),
        parent_fqn: Some(class_fqn.clone()),
        span: span_of(node),
        extra: serde_json::Value::Null,
    });
    // 方法级注解（`@GetMapping("/list")` / `@Value("${x}")` …）
    // owner_class 是所属类（target_fqn 即 `类.方法`，故单独传入 class_fqn）。
    collect_annotations(node, src, &fqn, &class_fqn, out);
    Some(fqn)
}

/// 注解 → **调用点**。
///
/// Java 的注解、Python 的装饰器、C# 的 Attribute 是同一个概念：**声明式的框架调用**
/// （注册路由、注入依赖、映射表）。建模成 `CallSiteFact` 后，现有 FKB DSL
/// （`kind: call` + `callee`）无需扩展即可匹配它们。
///
/// `owner_class_fqn` 是注解所在类的 FQN（类级注解 = `类`，方法级注解 = `类`），
/// 显式记录到 `CallSiteFact.owner_class`，避免内核按 `.` 切分时把类级注解误切成包名。
fn collect_annotations(
    node: Node,
    src: &[u8],
    target_fqn: &str,
    owner_class_fqn: &str,
    out: &mut SyntaxFacts,
) {
    // `modifiers` 在 tree-sitter-java 里**不是具名字段**，需按 kind 遍历子节点。
    let mut cursor = node.walk();
    for mods in node.named_children(&mut cursor) {
        if mods.kind() != "modifiers" {
            continue;
        }
        let mut c2 = mods.walk();
        for child in mods.named_children(&mut c2) {
            collect_one_annotation(child, node, src, target_fqn, owner_class_fqn, out);
        }
    }
}

fn collect_one_annotation(
    child: Node,
    parent: Node,
    src: &[u8],
    target_fqn: &str,
    owner_class_fqn: &str,
    out: &mut SyntaxFacts,
) {
    if !matches!(child.kind(), "annotation" | "marker_annotation") {
        return;
    }
    // 注解名：优先具名字段，取不到退回第一个子节点（`@GetMapping` / `@java.lang.X`）
    let name = opt_text(child.child_by_field_name("name"), src)
        .or_else(|| opt_text(child.named_child(0), src));
    let Some(name) = name else {
        return;
    };
    // 参数列表：`@X("/a")` 或 `@X(value = "/a", method = GET)`
    let args_node = match child.child_by_field_name("arguments") {
        Some(a) => Some(a),
        None => (0..child.named_child_count())
            .filter_map(|i| child.named_child(i as u32))
            .find(|n| n.kind() == "annotation_argument_list"),
    };
    let args = args_node.map(|a| literal_args(a, src)).unwrap_or_default();
    // `@EventListener` 处理方法的**首个形参类型**即事件类型（如 `OrderPlacedEvent`），
    // 供 FKB 把同类事件的发布 / 订阅归并到同一 `Event` 节点（见 `first_param_type`）。
    let entity = if name == "EventListener" {
        first_param_type(parent, src)
    } else {
        None
    };
    out.call_sites.push(CallSiteFact {
        owner_fqn: target_fqn.to_string(),
        owner_class: Some(owner_class_fqn.to_string()),
        callee_text: name.clone(),
        receiver: None,
        method: Some(name),
        args,
        span: span_of(child),
        snippet: None,
        db_table: None,
        // Java 侧暂不识别循环语句（且 FKB 尚无 Java 的 `db_verbs`，
        // N+1 规则本就只在 PHP 上跑）。
        in_loop: false,
        entity,
    });
}

/// 注解参数里的字面量（去引号），供 `ValueSource.arg` 取用。
///
/// 递归下钻：`@Table(name = "x")` 的字面量藏在 `element_value_pair` 里。
fn literal_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    collect_literals(node, src, &mut out);
    out
}

/// 类型名脱去泛型 / 数组外壳，只留裸类型名。
///
/// `List<Order>` → `List`；`Order[]` → `Order`。用于实参 / 形参的类型归并。
fn bare_type_name(raw: String) -> String {
    raw.split(['<', '['])
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string()
}

/// 方法 / 构造器的首个形参类型（去泛型），供 `@EventListener` 取事件类型。
///
/// `onOrderPlaced(OrderPlacedEvent e)` → `OrderPlacedEvent`。非方法声明（类级 /
/// 字段级注解）返回 `None`。
fn first_param_type(node: Node, src: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "method_declaration" | "constructor_declaration") {
        return None;
    }
    let params = node.child_by_field_name("parameters")?;
    let first = (0..params.named_child_count())
        .filter_map(|i| params.named_child(i as u32))
        .find(|n| n.kind() == "formal_parameter")?;
    let type_node = first.child_by_field_name("type")?;
    text(type_node, src).map(bare_type_name)
}

/// 实参列表里首个 `new X(...)` 构造表达式的类型名（去泛型），供
/// `publishEvent(new X())` 取事件类型 X，使发布方与订阅方归并到同一 `Event` 节点。
fn constructed_entity_type(args_node: Node, src: &[u8]) -> Option<String> {
    let mut cursor = args_node.walk();
    for child in args_node.named_children(&mut cursor) {
        if child.kind() == "object_creation_expression" {
            if let Some(t) = child.child_by_field_name("type") {
                return text(t, src).map(bare_type_name);
            }
        }
    }
    None
}

/// 方法调用的**直接实参**按位置捕获字面量。
///
/// 与注解用的 `literal_args`（递归、不保序）不同，这里保证位置语义：
/// 第 i 个实参若是字符串 / 整数字面量取其值，否则占位 `Unknown`。这样
/// `arg:0` 始终对应「第 1 个实参」，不会因前面有变量参数而错位。
///
/// 例：`rabbitTemplate.convertAndSend("orders.queue", msg)` → `["orders.queue", Unknown]`，
/// FKB 即可据此把消息生产端落成 Queue 节点的 `PublishesTo` 边。
fn positional_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_literal" => {
                out.push(
                    text(child, src)
                        .map(|t| FactValue::String(t.trim_matches('"').to_string()))
                        .unwrap_or(FactValue::Unknown(None)),
                );
            }
            "decimal_integer_literal" => {
                out.push(text(child, src).map(FactValue::String).unwrap_or(FactValue::Unknown(None)));
            }
            _ => out.push(FactValue::Unknown(None)),
        }
    }
    out
}

fn collect_literals(node: Node, src: &[u8], out: &mut Vec<FactValue>) {
    match node.kind() {
        "string_literal" => {
            if let Some(t) = text(node, src) {
                out.push(FactValue::String(t.trim_matches('"').to_string()));
                return;
            }
        }
        "decimal_integer_literal" => {
            if let Some(t) = text(node, src) {
                out.push(FactValue::String(t));
                return;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_literals(child, src, out);
    }
}

/// `obj.method(args)` → 调用点。
fn collect_call(
    node: Node,
    src: &[u8],
    out: &mut SyntaxFacts,
    stack: &[String],
    loop_depth: &mut u32,
) {
    let Some(method) = opt_text(node.child_by_field_name("name"), src) else {
        return;
    };
    let receiver = node
        .child_by_field_name("object")
        .and_then(|n| text(n, src));
    let callee_text = match &receiver {
        Some(r) => format!("{}.{}", r, method),
        None => method.clone(),
    };
    // 方法压栈后：`stack.last()` 是方法 FQN，`stack[len-2]` 是所属类；
    // 类级调用（不在方法体内）时两者相同。
    let owner_fqn = stack.last().cloned().unwrap_or_default();
    let owner_class = if stack.len() >= 2 {
        stack.get(stack.len() - 2).cloned()
    } else {
        stack.last().cloned()
    };
    // 实参字面量**按位置**捕获（见 `positional_args`）：首个实参若是字符串 /
    // 整数字面量则取其值，否则占位 Unknown——保证 `arg:0` 对应「第 1 个实参」。
    let args_node = match node.child_by_field_name("arguments") {
        Some(a) => Some(a),
        None => (0..node.named_child_count())
            .filter_map(|i| node.named_child(i as u32))
            .find(|n| n.kind() == "argument_list"),
    };
    let args = args_node.map(|a| positional_args(a, src)).unwrap_or_default();
    // 事件发布：`publishEvent(new X())` 取 `new` 出来的事件类型 X，使发布方与
    // 订阅方按事件类型归并到同一 `Event` 节点（见 `constructed_entity_type`）。
    let entity = if method == "publishEvent" {
        args_node.and_then(|a| constructed_entity_type(a, src))
    } else {
        None
    };
    out.call_sites.push(CallSiteFact {
        owner_fqn,
        owner_class,
        callee_text,
        receiver,
        method: Some(method),
        args,
        span: span_of(node),
        snippet: None,
        db_table: None,
        // 循环体内（含嵌套）的调用：N+1 规则靠它判定「这段会被执行 N 次」。
        in_loop: *loop_depth > 0,
        entity,
    });
}

fn text(node: Node, src: &[u8]) -> Option<String> {
    node.utf8_text(src)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn opt_text(node: Option<Node>, src: &[u8]) -> Option<String> {
    node.and_then(|n| text(n, src))
}

fn span_of(node: Node) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span {
        start_line: start.row as u32 + 1,
        end_line: end.row as u32 + 1,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 循环体内的调用要打上 `in_loop`（N+1 规则唯一能看出「这段代码会被执行 N 次」的地方）。
    /// 与 PHP 侧一致：**只有 body 算循环内**，条件 / 更新表达式不算。
    #[test]
    fn marks_calls_in_loop_bodies() {
        let src = r#"package com.demo;

import java.util.List;

class Svc {
    private Repo repo;

    void run(List<Long> ids) {
        // ① 传统 for：body 内算、条件内不算
        for (int i = 0; i < ids.size(); i++) {
            repo.findById(ids.get(i));
        }
        // ② 增强 for
        for (Long id : ids) {
            repo.findById(id);
        }
        // ③ while
        while (repo.hasNext()) {
            repo.findById(1L);
        }
        // ④ do-while
        do {
            repo.findById(2L);
        } while (repo.hasNext());
        // ⑤ 循环外
        repo.findAll();
        // ⑥ 嵌套循环
        for (Long a : ids) {
            for (Long b : ids) {
                repo.findById(a);
            }
        }
    }
}
"#;
        let parser = JavaParser::new().unwrap();
        let facts = parser.parse("src/main/java/com/demo/Svc.java", src).unwrap();
        let in_loop: Vec<&str> = facts
            .call_sites
            .iter()
            .filter(|c| c.in_loop)
            .map(|c| c.callee_text.as_str())
            .collect();
        // ① ~ ④ 各 1 条 + ⑥ 1 条 = 5 条 findById；另有 ① 实参里嵌套的 `ids.get(i)`
        // 也算循环内（它同样被执行 N 次）。
        assert_eq!(in_loop.len(), 6, "循环内调用应 6 条，实际：{in_loop:?}");
        assert_eq!(
            in_loop.iter().filter(|c| c.contains("findById")).count(),
            5,
            "findById 应 5 条，实际：{in_loop:?}"
        );
        assert!(
            in_loop.iter().any(|c| c.contains("ids.get")),
            "实参里嵌套的调用也算循环内，实际：{in_loop:?}"
        );
        // 循环外的 findAll 未被标记
        assert!(!in_loop.iter().any(|c| c.contains("findAll")));
        // 循环条件里的 hasNext 不算循环内
        assert!(!in_loop.iter().any(|c| c.contains("hasNext")));
    }

    /// Repository / Mapper 的泛型实参 = 它操作的实体（JPA / MyBatis-Plus 的 DAO 约定）。
    #[test]
    fn captures_dao_generic_entity() {
        let src = r#"package com.demo;

interface UserRepository extends JpaRepository<User, Long> {
}

interface OrderMapper extends BaseMapper<Order> {
}
"#;
        let parser = JavaParser::new().unwrap();
        let facts = parser.parse("src/main/java/com/demo/Repos.java", src).unwrap();
        let generic: Vec<(&str, Option<&str>)> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text.starts_with("generic."))
            .map(|c| (c.callee_text.as_str(), c.entity.as_deref()))
            .collect();
        // 同包裸名会按 DAO 所在包补成 FQN（短名在图里命中不了节点）
        assert!(
            generic.contains(&("generic.JpaRepository", Some("com.demo.User"))),
            "应捕获 JpaRepository<User> → com.demo.User，实际：{generic:?}"
        );
        assert!(
            generic.contains(&("generic.BaseMapper", Some("com.demo.Order"))),
            "应捕获 BaseMapper<Order> → com.demo.Order，实际：{generic:?}"
        );
    }
}
