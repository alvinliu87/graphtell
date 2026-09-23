//! Python 解析器 —— **第三语言**，进一步验证语言层抽象可插拔。
//!
//! 只做语法层（P2）该做的事：把 Python 语法树翻译成语言无关的 [`SyntaxFacts`]。
//! 语义层（FastAPI 的 `@app.get` / SQLAlchemy 的 `__tablename__` / Celery 的
//! `@app.task`）应由 FKB 声明，与 ThinkPHP / Spring Boot 走同一套机制。
//!
//! ## 与 Java 的对应（同一个概念，不同表达）
//! | 概念         | Java                     | Python                    |
//! |--------------|--------------------------|---------------------------|
//! | 声明式框架调用 | `@GetMapping("/x")` 注解  | `@app.get("/x")` 装饰器    |
//! | 类           | `class X`                | `class X`                 |
//! | 成员分隔      | `.`                      | `.`                       |
//!
//! 两者都被建模为 [`CallSiteFact`]，因此 FKB 的 `kind: call` 选择器无需任何扩展
//! 即可匹配装饰器 —— 「注解 / 装饰器是同一个概念」在 Python 侧同样成立。
//!
//! ## Python 特有的两个建模决策
//! 1. **模块即命名空间**：函数常常直接在模块里（不在类里）。故按文件路径推出
//!    模块点分名（`app/api/users.py` → `app.api.users`），既登记为 `Namespace`
//!    声明，也作为调用点的 `owner_class`。这样 FKB 的 `HandledBy` 链接才能
//!    先找到模块节点、再按 `{module}.{func}` 落到**函数本身**（`find_target_node`
//!    要求先定位到一个父节点，否则 Python 的路由方法会全部落空）。
//! 2. **装饰器归属被装饰者**：装饰器的 `owner_fqn` 是它修饰的函数 / 类，
//!    与 Java 里「方法级注解的 owner 是该方法」一致。

use std::cell::RefCell;
use std::collections::HashSet;

use gt_domain::error::Result;
use gt_domain::model::{
    CallSiteFact, Declaration, EdgeKind, FactValue, ImportFact, InheritanceFact, Language, NodeKind,
    Span, SyntaxFacts,
};
use gt_domain::port::LanguageParser;
use serde_json::json;
use tree_sitter::{Node, Parser};

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

pub struct PythonParser {
    language: tree_sitter::Language,
}

impl PythonParser {
    pub fn new() -> Result<Self> {
        let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        Ok(Self { language })
    }
}

impl LanguageParser for PythonParser {
    fn language(&self) -> Language {
        Language::new(Language::PYTHON)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                // 语言在构造时已校验，这里失败属于编程错误
                p.set_language(&self.language).expect("python language");
                p
            });
            parser.parse(source, None)
        });
        let Some(tree) = tree else {
            return Ok(SyntaxFacts::default());
        };

        let mut facts = SyntaxFacts::default();
        let module = module_fqn(path);
        facts.namespace = Some(module.clone());
        // 模块登记为 Namespace 节点：Python 的模块级函数没有类可挂，
        // 只有把模块做成图里的节点，P5 的 `HandledBy` 才能连到函数本身。
        facts.declarations.push(Declaration {
            kind: NodeKind(NodeKind::NAMESPACE.to_string()),
            name: module.clone(),
            fqn: module.clone(),
            parent_fqn: None,
            span: span_of(tree.root_node()),
            extra: json!({}),
        });

        let mut ctx = Ctx {
            src: source.as_bytes(),
            facts: &mut facts,
            module: module.clone(),
            owner_fqn: module,
            current_class: None,
            loop_depth: 0,
        };
        walk(tree.root_node(), &mut ctx);
        Ok(facts)
    }

    /// Python 的命名空间分隔符是 `.`（PHP 是 `\`）。
    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    /// Python 的成员分隔符是 `.`（PHP 是 `::`）。
    fn member_separator(&self) -> &'static str {
        "."
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["pyproject.toml", "requirements.txt", "setup.py", "Pipfile"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &["__pycache__", "venv", ".venv", "site-packages", ".tox", ".mypy_cache", ".pytest_cache"]
    }
}

/// 解析过程的上下文。
struct Ctx<'a> {
    src: &'a [u8],
    facts: &'a mut SyntaxFacts,
    /// 本文件的模块点分名（`app.api.users`）。
    module: String,
    /// 当前所处作用域的 FQN：模块 → 类 → 方法，随嵌套向下追加。
    owner_fqn: String,
    /// 当前所处的**类** FQN；模块级（不在类体内）时为 `None`。
    current_class: Option<String>,
    /// 当前嵌套在几层 `for` / `while` 循环体内（同 PHP 的 `loop_depth`）：
    /// 用深度而非布尔，嵌套循环退出内层后外层剩余语句仍算「在循环内」。
    loop_depth: u32,
}

impl Ctx<'_> {
    /// 调用点的所属类：类内为类 FQN，模块级时为**模块 FQN**。
    ///
    /// Python 的路由处理函数多为模块级，写 `None` 会让依赖 `owner_class` 的链接
    /// 全部落空；用模块名兜底，才能在 `Namespace` 节点上继续按成员名定位函数。
    fn class_or_module(&self) -> String {
        self.current_class.clone().unwrap_or_else(|| self.module.clone())
    }
}

fn walk(node: Node, ctx: &mut Ctx) {
    match node.kind() {
        "import_statement" | "import_from_statement" => {
            collect_import(node, ctx);
            return;
        }
        "class_definition" => {
            collect_class(node, ctx);
            return;
        }
        "function_definition" => {
            collect_function(node, ctx);
            return;
        }
        // 被装饰的定义：先登记定义本身（含其体内的调用点），
        // 再把装饰器按「被装饰者的 FQN」落成调用点。
        "decorated_definition" => {
            collect_decorated(node, ctx);
            return;
        }
        "call" => {
            collect_call(node, ctx);
        }
        // 循环语句：只有 **body 内**的调用算「循环体内」（同 PHP）。
        "for_statement" | "while_statement" => {
            collect_loop(node, ctx);
            return;
        }
        // 类体内的字面量赋值就是**类属性**（`__tablename__ = "users"`），
        // 与 PHP 的类属性同机制登记为 Property 声明。这里不做任何框架假设。
        "expression_statement" if ctx.current_class.is_some() => {
            collect_class_attribute(node, ctx);
        }
        _ => {}
    }
    recurse(node, ctx);
}

fn recurse(node: Node, ctx: &mut Ctx) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, ctx);
    }
}

// ------------------------------------------------------------------ 声明

/// 类定义 → 声明 + 继承事实 + 类体内的成员。返回类 FQN。
fn collect_class(node: Node, ctx: &mut Ctx) -> Option<String> {
    let name = opt_text(node.child_by_field_name("name"), ctx.src)?;
    let fqn = format!("{}.{}", ctx.owner_fqn, name);
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::CLASS.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: Some(ctx.owner_fqn.clone()),
        span: span_of(node),
        extra: json!({}),
    });
    collect_supertypes(node, ctx, &fqn);

    let prev_fqn = ctx.owner_fqn.clone();
    let prev_class = ctx.current_class.clone();
    ctx.owner_fqn = fqn.clone();
    ctx.current_class = Some(fqn.clone());
    if let Some(body) = node.child_by_field_name("body") {
        walk(body, ctx);
    }
    ctx.owner_fqn = prev_fqn;
    ctx.current_class = prev_class;
    Some(fqn)
}

/// 基类列表 → 继承事实。
///
/// `metaclass=Meta` 这类关键字参数不是基类（PyORM 常用），跳过。
fn collect_supertypes(node: Node, ctx: &mut Ctx, child_fqn: &str) {
    let supers = node
        .child_by_field_name("superclasses")
        .or_else(|| {
            (0..node.named_child_count())
                .filter_map(|i| node.named_child(i as u32))
                .find(|n| n.kind() == "argument_list")
        });
    let Some(supers) = supers else { return };
    let mut cursor = supers.walk();
    for arg in supers.named_children(&mut cursor) {
        if arg.kind() == "keyword_argument" {
            continue;
        }
        // `Base(polymorphic_on=x)` 这类带参基类：取类名部分。
        let base_src = if arg.kind() == "call" {
            arg.child_by_field_name("function")
        } else {
            Some(arg)
        };
        let Some(base) = base_src.and_then(|b| text(b, ctx.src)) else {
            continue;
        };
        ctx.facts.inheritances.push(InheritanceFact {
            child_fqn: child_fqn.to_string(),
            base_name: bare_typename(base),
            kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
            span: span_of(arg),
        });
    }
}

/// 函数 / 方法定义 → 声明。返回其 FQN。
///
/// 形参加类型记入 `extra.parameters`：后续可据此解析 `依赖注入参数 -> 实例调用`
/// 的调用链（与 PHP 侧 `__construct(LoginServices $services)` 的用法同源）。
fn collect_function(node: Node, ctx: &mut Ctx) -> Option<String> {
    let name = opt_text(node.child_by_field_name("name"), ctx.src)?;
    let fqn = format!("{}.{}", ctx.owner_fqn, name);
    // 类体内的函数是方法，模块级 / 嵌套函数是普通函数。区分的意义在于：
    // P3 只对 METHOD 额外解析形参类型（供后续按变量类型解析实例调用）。
    let kind = if ctx.current_class.is_some() {
        NodeKind::METHOD
    } else {
        NodeKind::FUNCTION
    };
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(kind.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: Some(ctx.owner_fqn.clone()),
        span: span_of(node),
        extra: json!({ "parameters": params_of(node, ctx.src) }),
    });

    let prev_fqn = ctx.owner_fqn.clone();
    ctx.owner_fqn = fqn.clone();
    if let Some(body) = node.child_by_field_name("body") {
        walk(body, ctx);
    }
    ctx.owner_fqn = prev_fqn;
    Some(fqn)
}

/// 类体里的**字面量赋值** → Property 声明（`__tablename__ = "users"`）。
///
/// Python 的类属性没有专门的声明语法，就是类体内的一条赋值语句。把它登记成
/// Property 后，`cf_ast` 会把默认值记录到**类节点**上，FKB 即可用
/// 「节点选择器 + `HasProperty`」命中 —— 解析器侧不需要认识任何 ORM 框架。
///
/// 只收**裸标识符左侧 + 字面量右侧**的组合：`self.x = 1`(属性式赋值)与
/// `cols = Column(...)`(右值不是字面量)都不在此列，避免把赋值噪声混进属性表。
fn collect_class_attribute(node: Node, ctx: &mut Ctx) {
    // 只收**类体直接作用域**：方法体内的 `total = 5` 是局部变量（`owner_fqn` 是
    // `Class.method` 而非类本身），当成类属性会把局部变量污染进属性表。
    if ctx.current_class.as_deref() != Some(ctx.owner_fqn.as_str()) {
        return;
    }
    let Some(assign) = node.named_child(0).filter(|n| n.kind() == "assignment") else {
        return;
    };
    let (Some(left), Some(right)) = (
        assign.child_by_field_name("left"),
        assign.child_by_field_name("right"),
    ) else {
        return;
    };
    if left.kind() != "identifier" {
        return;
    }
    let Some(name) = text(left, ctx.src) else { return };
    let value = match right.kind() {
        "string" => string_value(right, ctx.src).map(FactValue::String),
        "integer" => text(right, ctx.src).map(FactValue::String),
        "true" => Some(FactValue::Bool(true)),
        "false" => Some(FactValue::Bool(false)),
        "none" => Some(FactValue::Null),
        _ => None,
    };
    let Some(value) = value else { return };
    let Some(class_fqn) = ctx.current_class.clone() else { return };
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::PROPERTY.to_string()),
        name: name.clone(),
        fqn: format!("{}.{}", class_fqn, name),
        parent_fqn: Some(class_fqn),
        span: span_of(node),
        extra: json!({ "default": value }),
    });
}

/// 形参列表 → `[{name, type}]`（类型缺失时为 `null`）。
fn params_of(node: Node, src: &[u8]) -> Vec<serde_json::Value> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for p in params.named_children(&mut cursor) {
        // `typed_parameter` / `default_parameter` / `typed_default_parameter` 的类型
        // 字段统一叫 `type`；名字则**没有**字段（tree-sitter-python 把它作为裸的
        // 首个子节点），故先试字段名、退回首个具名子节点。
        let pname = opt_text(p.child_by_field_name("name"), src)
            .or_else(|| opt_text(p.named_child(0), src));
        let Some(pname) = pname else { continue };
        let ty = opt_text(p.child_by_field_name("type"), src);
        out.push(json!({ "name": pname, "type": ty }));
    }
    out
}

/// 被装饰的定义：装饰器 → **调用点**（owner = 被装饰者）。
///
/// 与 Java 注解同道：装饰器的 `owner_fqn` 是被装饰的函数 FQN，
/// `owner_class` 是所属类（模块级函数则用模块名）。
fn collect_decorated(node: Node, ctx: &mut Ctx) {
    let mut def: Option<Node> = None;
    let mut decorators: Vec<Node> = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "decorator" => decorators.push(child),
            _ => def = Some(child),
        }
    }
    let Some(def) = def else { return };
    // 装饰器挂载的目标类 / 模块：定义在类体内则取该类，否则取模块。
    let owner_class = ctx.class_or_module();
    let target = match def.kind() {
        "function_definition" => collect_function(def, ctx),
        "class_definition" => collect_class(def, ctx),
        _ => {
            walk(def, ctx);
            None
        }
    };
    let Some(target_fqn) = target else { return };
    for d in decorators {
        collect_decorator(d, ctx, &target_fqn, &owner_class);
    }
}

fn collect_decorator(node: Node, ctx: &mut Ctx, target_fqn: &str, owner_class: &str) {
    // `decorator` 的首个具名子节点即 `@` 之后的表达式。
    let Some(expr) = node.named_child(0) else { return };
    collect_call_like(expr, ctx, target_fqn, owner_class);
    // 装饰器实参里还可能有其它调用（`@app.get("/x", dependencies=[Depends(auth)])`）
    recurse(expr, ctx);
}

// ------------------------------------------------------------------ 调用点

/// 普通调用 `f(x)` / `obj.m(x)` → 调用点。
fn collect_call(node: Node, ctx: &mut Ctx) {
    // 先取走两份所有权：`collect_call_like` 还要独占借用 ctx，
    // 参数表达式里再借用 `*ctx` 会触发借用冲突。
    let owner_fqn = ctx.owner_fqn.clone();
    let owner_class = ctx.class_or_module();
    collect_call_like(node, ctx, &owner_fqn, &owner_class);
}

/// 把一个「调用形表达式」记成调用点。
///
/// 同时服务于**装饰器**（`@app.get("/x")`）与**普通调用**（`redis.get(k)`）——
/// 两者在 FKB 眼里都是 `kind: call`，由 `callee` 选择器区分。
fn collect_call_like(node: Node, ctx: &mut Ctx, owner_fqn: &str, owner_class: &str) {
    let func = if node.kind() == "call" {
        // `@app.get("/x")`：本身就是调用，取 function 字段。
        node.child_by_field_name("function")
    } else {
        // 未调用的装饰器（`@app.deprecated`）：节点自身即被调用者。
        Some(node)
    };
    let Some(func) = func else { return };
    let (callee_text, receiver, method) = resolve_callee(func, ctx.src);
    if callee_text.is_empty() {
        return;
    }
    // 实参**按位置**捕获（见 `positional_args`）：第 1 个实参若是字面量则取其值，
    // 否则占位 Unknown —— 保证 `arg:0` 始终对应「第 1 个实参」。
    let args = node
        .child_by_field_name("arguments")
        .map(|a| positional_args(a, ctx.src))
        .unwrap_or_default();
    ctx.facts.call_sites.push(CallSiteFact {
        owner_fqn: owner_fqn.to_string(),
        owner_class: Some(owner_class.to_string()),
        callee_text,
        receiver,
        method,
        args,
        span: span_of(node),
        snippet: snippet_of(node, ctx.src),
        db_table: None,
        in_loop: ctx.loop_depth > 0,
        entity: None,
    });
}

/// 被调用者 → `(callee_text, receiver, method)`。
///
/// `os.environ.get(...)` 的接收者是 `os.environ`（而非 `os`）—— 保留完整的
/// 属性链，FKB 才能用 `os.environ::get` 这类带点作用域精确命中。
fn resolve_callee(node: Node, src: &[u8]) -> (String, Option<String>, Option<String>) {
    if node.kind() == "attribute" {
        let obj = opt_text(node.child_by_field_name("object"), src);
        let attr = opt_text(node.child_by_field_name("attribute"), src);
        return match (obj, attr) {
            (Some(o), Some(a)) => (format!("{}.{}", o, a), Some(o), Some(a)),
            (Some(o), None) => (o.clone(), Some(o), None),
            (None, Some(a)) => (a.clone(), None, Some(a)),
            (None, None) => (String::new(), None, None),
        };
    }
    let t = text(node, src).unwrap_or_default();
    (t.clone(), None, Some(t).filter(|s| !s.is_empty()))
}

/// 实参列表里**按位置**捕获字面量。
///
/// 与 Java 的 `positional_args` 同义：第 i 个实参是字面量则取值，否则占位
/// `Unknown`。非字面量一律不带原文 —— 避免 FKB 漏写 `require_literal` 时
/// 把 Python 表达式文本误当成语义身份。
fn positional_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        out.push(match child.kind() {
            // 关键字实参：`queue="payments"` → `[("queue", "payments")]`。
            //
            // 用 `Array` 承载，FKB 即可用 `{ arg: n, field: "queue" }` **按名**取值
            // （与 PHP 侧 `[Ctrl::class, 'method']` 的取值机制同源）。这样诸如
            // 「显式指定队列」这类写在关键字实参里的语义就不再丢失 —— 且不必假设
            // 它出现在第几个位置。
            "keyword_argument" => {
                match (
                    opt_text(child.child_by_field_name("name"), src),
                    child.child_by_field_name("value").map(|v| literal_value(v, src)),
                ) {
                    (Some(k), Some(v)) => FactValue::Array(vec![(k, v)]),
                    _ => FactValue::Unknown(None),
                }
            }
            _ => literal_value(child, src),
        });
    }
    out
}

/// 单个表达式节点 → 字面量事实；非字面量一律 `Unknown(None)`（不带源码原文 ——
/// 避免 FKB 漏写 `require_literal` 时把 Python 表达式文本当成语义身份）。
fn literal_value(node: Node, src: &[u8]) -> FactValue {
    match node.kind() {
        "string" => match string_value(node, src) {
            Some(s) => FactValue::String(s),
            None => FactValue::Unknown(None),
        },
        "integer" | "float" => match text(node, src) {
            Some(s) => FactValue::String(s),
            None => FactValue::Unknown(None),
        },
        "true" => FactValue::Bool(true),
        "false" => FactValue::Bool(false),
        "none" => FactValue::Null,
        _ => FactValue::Unknown(None),
    }
}

// ------------------------------------------------------------------ 导入

fn collect_import(node: Node, ctx: &mut Ctx) {
    if node.kind() == "import_statement" {
        // `import os.path` / `import numpy as np`
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            push_import(child, None, ctx);
        }
        return;
    }
    // `from app.core.config import settings`
    let module_node = node.child_by_field_name("module_name");
    let module = opt_text(module_node, ctx.src).map(|m| m.trim_start_matches('.').to_string());
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        // 跳过 `module_name`：它本身也是个 `dotted_name`，当作导入项会凭空造出
        // `fastapi.fastapi` 这类「模块自己导入自己」的脏记录。
        if Some(child.id()) == module_node.map(|n| n.id()) {
            continue;
        }
        push_import(child, module.as_deref(), ctx);
    }
}

/// 把一条 `name` / `aliased_import` 记成导入事实。
///
/// `from x.y import z` 记为 `x.y.z`：符号短名 `z` 才能在 P3 被还原成完全限定名，
/// 这与 PHP 的 `use think\facade\Db` 还原成 `Db -> think\facade\Db` 同源。
fn push_import(node: Node, from_module: Option<&str>, ctx: &mut Ctx) {
    let span = span_of(node);
    match node.kind() {
        "dotted_name" | "identifier" => {
            if let Some(name) = text(node, ctx.src) {
                let name = match from_module {
                    Some(m) if !m.is_empty() => format!("{}.{}", m, name),
                    _ => name,
                };
                ctx.facts.imports.push(ImportFact { alias: None, name, span });
            }
        }
        "aliased_import" => {
            let name = opt_text(node.child_by_field_name("name"), ctx.src);
            let alias = opt_text(node.child_by_field_name("alias"), ctx.src);
            if let Some(name) = name {
                let name = match from_module {
                    Some(m) if !m.is_empty() => format!("{}.{}", m, name),
                    _ => name,
                };
                ctx.facts.imports.push(ImportFact { alias, name, span });
            }
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ 循环

fn collect_loop(node: Node, ctx: &mut Ctx) {
    // 只有循环**体**内的调用算 in_loop：迭代式 / 条件里的调用每条记录不会执行，
    // 标成循环内会把「启动前查一次」误报成 N+1（PHP 侧同理）。
    let bodies: HashSet<usize> = field_children(node, "body").iter().map(|n| n.id()).collect();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if bodies.contains(&child.id()) {
            ctx.loop_depth += 1;
            walk(child, ctx);
            ctx.loop_depth -= 1;
        } else {
            walk(child, ctx);
        }
    }
}

/// 取带指定**字段名**的全部子节点（`for` 的 `body` 之外还有 `else` 等）。
fn field_children<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return out;
    }
    loop {
        let child = cursor.node();
        if child.is_named() && cursor.field_name() == Some(field) {
            out.push(child);
        }
        if !cursor.goto_next_sibling() {
            return out;
        }
    }
}

// ------------------------------------------------------------------ 工具

/// 文件路径 → 模块点分名。
///
/// `app/api/users.py` → `app.api.users`；`app/__init__.py` → `app`（包目录本身）。
fn module_fqn(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.strip_suffix(".pyi").or_else(|| p.strip_suffix(".py")).unwrap_or(&p);
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let mut segs = segs;
    if segs.last() == Some(&"__init__") {
        segs.pop();
    }
    segs.join(".")
}

/// 去掉 `<...>` 泛型外壳：`Base[Order]` → `Base`。
fn bare_typename(raw: String) -> String {
    raw.split(['<', '[']).next().unwrap_or(&raw).trim().to_string()
}

/// Python 字符串字面量去引号（兼容前缀与三引号）。
fn string_value(node: Node, src: &[u8]) -> Option<String> {
    let raw = text(node, src)?;
    Some(unquote(&raw))
}

fn unquote(raw: &str) -> String {
    let s = raw.trim();
    // 去掉至多两个前缀字母：`r"..."` / `rb"..."` / `f'...'`
    let body = s
        .trim_start_matches(|c| matches!(c, 'r' | 'R' | 'b' | 'B' | 'f' | 'F' | 'u' | 'U'));
    for q in ["\"\"\"", "'''", "\"", "'"] {
        if body.len() >= q.len() * 2 && body.starts_with(q) && body.ends_with(q) {
            return body[q.len()..body.len() - q.len()].to_string();
        }
    }
    body.trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// 调用点所在**行**的源码文本，便于 UI 直接显示调用语句（同 PHP 侧）。
fn snippet_of(node: Node, src: &[u8]) -> Option<String> {
    const MAX: usize = 160;
    let start = node.start_byte().min(src.len());
    let line_start = src[..start].iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    let rest = std::str::from_utf8(src.get(line_start..).unwrap_or_default()).unwrap_or("");
    let line = rest.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    if line.len() <= MAX {
        return Some(line.to_string());
    }
    let mut end = MAX;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &line[..end]))
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

    fn parse_src(path: &str, src: &str) -> SyntaxFacts {
        PythonParser::new().unwrap().parse(path, src).unwrap()
    }

    /// 按 callee 原文取调用点（同一段源码里方法名会重名，只能按 callee 精确定位）。
    fn call_of<'a>(facts: &'a SyntaxFacts, callee: &str) -> &'a CallSiteFact {
        facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == callee)
            .unwrap_or_else(|| {
                panic!(
                    "未见调用点 {callee}，实际有：{:?}",
                    facts.call_sites.iter().map(|c| &c.callee_text).collect::<Vec<_>>()
                )
            })
    }

    /// 装饰器落成「被装饰者的调用点」—— Python 版「注解即调用点」。
    ///
    /// 这是 FastAPI 路由能被 FKB 命中的前提：owner 必须是**处理函数**本身，
    /// owner_class 落到模块（Python 函数常不在类里，详见 [`Ctx::class_or_module`]）。
    #[test]
    fn decorators_become_call_sites_of_decorated_definition() {
        let facts = parse_src(
            "app/api/users.py",
            r#"@router.get("/users")
def list_users():
    pass

@app.delete("/orders/{order_id}", tags=["admin"])
def remove_order():
    pass
"#,
        );
        let get = call_of(&facts, "router.get");
        assert_eq!(get.owner_fqn, "app.api.users.list_users");
        assert_eq!(get.owner_class.as_deref(), Some("app.api.users"));
        assert_eq!(get.receiver.as_deref(), Some("router"));
        assert_eq!(get.method.as_deref(), Some("get"));
        // 位置语义：第 1 个实参（路径字面量）必须在 arg0
        assert_eq!(get.args.first().and_then(|a| a.as_str()), Some("/users"));

        let del = call_of(&facts, "app.delete");
        assert_eq!(del.owner_fqn, "app.api.users.remove_order");
        assert_eq!(del.args.first().and_then(|a| a.as_str()), Some("/orders/{order_id}"));
        // 关键字实参（`tags=["admin"]`）按名捕获成 `[("tags", 值)]`；
        // 值是列表不是字面量，故为 Unknown（详见 `keyword_arguments_are_captured_by_name`）。
        assert_eq!(del.args.len(), 2);
        match del.args.get(1) {
            Some(FactValue::Array(items)) => {
                assert_eq!(items[0].0, "tags");
                assert!(matches!(items[0].1, FactValue::Unknown(None)));
            }
            other => panic!("关键字实参应捕成 Array，实际：{other:?}"),
        }
    }

    /// 类声明与继承事实：`superclasses` 里的**位置参数**才是基类，
    /// `metaclass=...` 这类关键字形式必须跳过。
    #[test]
    fn class_declaration_records_inheritance_and_methods() {
        let facts = parse_src(
            "app/models/user.py",
            r#"class UserModel(Base, metaclass=Meta):
    def save(self):
        pass
"#,
        );
        let class = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::CLASS)
            .expect("应有类声明");
        assert_eq!(class.fqn, "app.models.user.UserModel");
        let method = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::METHOD)
            .expect("类体内的函数应记为方法");
        assert_eq!(method.fqn, "app.models.user.UserModel.save");

        // 基类收录为 EXTENDS；`metaclass=Meta` 不是基类，必须跳过
        let bases: Vec<&str> = facts.inheritances.iter().map(|i| i.base_name.as_str()).collect();
        assert_eq!(bases, vec!["Base"]);
        assert_eq!(facts.inheritances[0].kind.as_str(), EdgeKind::EXTENDS);
    }

    /// 形参加类型：`svc: OrderService` 是后续解析 `svc.delete()` 调用链的基础。
    #[test]
    fn typed_parameters_are_recorded() {
        let facts = parse_src(
            "app/api/orders.py",
            "def remove_order(order_id: int, svc: OrderService):\n    svc.delete(order_id)\n",
        );
        let d = facts
            .declarations
            .iter()
            .find(|d| d.fqn == "app.api.orders.remove_order")
            .expect("应登记该函数");
        let params = d.extra.get("parameters").and_then(|v| v.as_array()).expect("应带形参表");
        let got: Vec<(&str, &str)> = params
            .iter()
            .map(|p| {
                (
                    p.get("name").and_then(|v| v.as_str()).unwrap(),
                    p.get("type").and_then(|v| v.as_str()).unwrap(),
                )
            })
            .collect();
        assert_eq!(got, vec![("order_id", "int"), ("svc", "OrderService")]);
    }

    /// 只有循环**体**内的调用算 in_loop：迭代式里的调用每条记录不会执行，
    /// 标成循环内会把「循环前查一次」误报成 N+1（与 PHP 侧同一取舍）。
    #[test]
    fn loop_body_marks_call_sites() {
        let facts = parse_src(
            "app/services/sync.py",
            r#"def run(items):
    total = count_all()
    for u in fetch_page():
        db.query(u)
    while True:
        db.flush()
"#,
        );
        assert!(!call_of(&facts, "fetch_page").in_loop, "迭代式里的调用不算循环体内");
        assert!(call_of(&facts, "db.query").in_loop);
        assert!(call_of(&facts, "db.flush").in_loop);
        assert!(!call_of(&facts, "count_all").in_loop);
    }

    /// `from x.y import z` 记为 `x.y.z`（供短名还原）；`module_name` 本身不是导入项。
    #[test]
    fn from_import_records_qualified_symbol() {
        let facts = parse_src("app/main.py", "from fastapi import FastAPI, APIRouter\nimport numpy as np\n");
        let names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["fastapi.FastAPI", "fastapi.APIRouter", "numpy"]);
        assert_eq!(facts.imports.last().unwrap().alias.as_deref(), Some("np"));
    }

    /// 类体内的字面量赋值登记为 Property —— SQLAlchemy / Django 表映射能被识别的前提。
    ///
    /// 注意这里**不含**任何 ORM 知识：解析器只知道「类属性 = 字面量」，
    /// 把 `__tablename__` 解释成表名完全交给 FKB（见 fkb/python/fastapi.yaml）。
    #[test]
    fn class_body_literal_assignment_becomes_property() {
        let facts = parse_src(
            "app/models/user.py",
            r#"class UserModel(Base):
    __tablename__ = "users"
    cache_ttl = 600

    def save(self):
        total = 5
        self.x = 1
"#,
        );
        let props: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.kind.as_str() == NodeKind::PROPERTY)
            .collect();
        // `total = 5` 是方法内的局部变量，`self.x = 1` 不是裸标识符 —— 两者都不该进来
        let names: Vec<&str> = props.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["__tablename__", "cache_ttl"]);

        let table = props[0];
        assert_eq!(table.fqn, "app.models.user.UserModel.__tablename__");
        assert_eq!(table.parent_fqn.as_deref(), Some("app.models.user.UserModel"));
        let value: FactValue =
            serde_json::from_value(table.extra["default"].clone()).expect("应能反序列化属性值");
        assert_eq!(value, FactValue::String("users".to_string()));
    }

    /// 关键字实参按名可取：显式队列 / 显式任务名这类「写在 `k=v` 里」的语义，
    /// 全靠这一步才不会丢。位置语义同时保持 —— 实参下标仍是源码里的顺序。
    #[test]
    fn keyword_arguments_are_captured_by_name() {
        let facts = parse_src(
            "app/tasks.py",
            "def run():\n    send.apply_async(args=[1], queue=\"payments\", countdown=10)\n",
        );
        let call = call_of(&facts, "send.apply_async");
        assert_eq!(call.args.len(), 3, "三个关键字实参各占一个位置");
        // 每个关键字实参在自身位置上形如 [("queue", 值)]
        let (key, value) = match call.args.get(1) {
            Some(FactValue::Array(items)) => (items[0].0.clone(), items[0].1.clone()),
            other => panic!("index 1 应是 Array，实际：{other:?}"),
        };
        assert_eq!(key, "queue");
        assert_eq!(value, FactValue::String("payments".to_string()));
        // 非字面量的值仍是 Unknown（`args=[1]`），不会被误当成身份
        assert!(matches!(
            call.args.get(0),
            Some(FactValue::Array(items)) if matches!(items[0].1, FactValue::Unknown(None))
        ));
    }

    #[test]
    fn module_fqn_derives_from_path() {
        assert_eq!(module_fqn("app/api/users.py"), "app.api.users");
        // 包的 `__init__.py` 就是目录本身
        assert_eq!(module_fqn("app/api/__init__.py"), "app.api");
        assert_eq!(module_fqn("main.py"), "main");
    }
}
