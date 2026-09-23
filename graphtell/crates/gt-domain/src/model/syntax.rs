//! 语言无关的「语法事实」——解析器适配器的输出契约。
//!
//! # 为什么需要这一层
//!
//! `tree-sitter` 的具体语法树每种语言都不同。如果让流水线直接消费
//! `tree_sitter::Node`，那么「兼容所有主流技术栈」就无从谈起。
//!
//! 因此定义一个**稳定的中间表示**：任何语言的解析器适配器都只需把自己的
//! 语法树翻译成 [`SyntaxFacts`]，之后的 CfAst / Synthesize / Resolve 阶段
//! 完全语言无关。这也正是六边形架构中「端口隔离」的体现。

use serde::{Deserialize, Serialize};

use super::graph::Span;
use super::kinds::{EdgeKind, NodeKind};

/// 一个源文件解析出的全部语法事实。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyntaxFacts {
    /// 命名空间（PHP `namespace app\services\order;`）。
    pub namespace: Option<String>,
    /// 声明：类 / 接口 / trait / 枚举 / 方法 / 函数 / 属性 / 常量。
    pub declarations: Vec<Declaration>,
    /// 导入：`use app\dao\order\StoreOrderDao;`。
    pub imports: Vec<ImportFact>,
    /// 继承 / 实现 / trait。
    pub inheritances: Vec<InheritanceFact>,
    /// 方法体内的一次调用点。
    pub call_sites: Vec<CallSiteFact>,
    /// 字段声明与类型（Java `@Autowired` 字段注入等）：`class -> field -> type`，
    /// 供 P7 按字段声明类型解析 `field.method()` 实例调用（service→mapper 链路）。
    pub field_types: Vec<FieldTypeFact>,
    /// 配置文件条目（如 `app/event.php` 的 `listen.*`）。
    pub config_entries: Vec<ConfigEntryFact>,
    /// 响应头赋值（如 `$header['Access-Control-Allow-Origin'] = ...`），供 CORS 反射检测。
    pub header_assignments: Vec<HeaderAssignFact>,
    /// 签名值的**相等性比较**（`$sign == $calc` / `$params['sign'] != ...`），
    /// 供验签质量判定（松散比较 / 非恒定时间）使用。
    ///
    /// 只收 `==` / `!=`（`===` / `!==` 是严格比较，不是这里要找的问题），
    /// 且只有**至少一侧像签名值**时才收 —— 否则一个工程里几万条比较全进来，
    /// 事实集体积爆炸（CORS 那条只收 Allow-Origin 一个头，同理）。
    pub sign_compares: Vec<SignCompareFact>,
    /// 方法体内的**局部变量赋值**（`$sql = ...;` / `$sql .= ...;`）。
    ///
    /// 供 P9 Taint 做**同函数内**的反向追踪：判断一个被拼进 SQL 的变量到底
    /// 是不是来自请求。没有它，Taint 只能看调用点参数文本里"有没有 `$var`" ——
    /// 于是 `Db::execute($execSql)` 这种整段变量传入的写法一律判高危，
    /// 而该变量其实来自随版本包发布的本地文件（实测 CRMEB 40 条里有 34 条是这类）。
    #[serde(default)]
    pub variable_assignments: Vec<VariableAssignFact>,
}

/// 一次局部变量赋值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableAssignFact {
    /// 变量名（不含 `$`），如 `execSql`。
    pub var: String,
    /// 赋值右侧的源码原文，如 `str_replace('@table', $table, $sql)`。
    pub rhs: String,
    /// 所在方法 / 函数的 FQN（反向追踪的作用域边界）。
    pub owner_fqn: String,
    /// 文件（在 `cf_ast` 阶段由 `file.path` 填充，与 `SignCompareFact` 同）。
    pub file: String,
    pub span: Span,
}

/// 声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Declaration {
    pub kind: NodeKind,
    /// 短名。
    pub name: String,
    /// 完全限定名（由命名空间 + 父级声明 + 短名推导）。
    pub fqn: String,
    /// 父级声明的 FQN（方法的所属类、常量的所属类等）。
    pub parent_fqn: Option<String>,
    pub span: Span,
    /// 语言特有字段：可见性、`static`、默认值、`abstract` 等。
    pub extra: serde_json::Value,
}

/// 导入事实。用于建立 `by_name` 索引与短名 → FQN 映射。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportFact {
    /// `use app\dao\order\StoreOrderDao as Dao;` 中的 `Dao`；无别名为 `None`。
    pub alias: Option<String>,
    /// 被导入的名字（可能是相对命名空间的）。
    pub name: String,
    pub span: Span,
}

/// 继承 / 实现 / trait 使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InheritanceFact {
    /// 子类 / 实现类的 FQN。
    pub child_fqn: String,
    /// 基类 / 接口 / trait 的名字（原文，可能是短名或导入别名）。
    pub base_name: String,
    pub kind: EdgeKind,
    pub span: Span,
}

/// 调用点。
///
/// 记录一次调用"怎么写出来"的完整信息，供后续规则匹配与 P7 解析：
/// * `callee_text` —— 原文，如 `Db::name`
/// * `receiver` —— `$this->dao`、`Cache`、或 `None`（自由函数）
/// * `method` —— `name`
/// * `args` —— 字面量参数（能静态求值的部分）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSiteFact {
    /// 所在方法/函数的 FQN（自由函数时为自身 FQN）。
    pub owner_fqn: String,
    /// 该调用点所在的**类** FQN，由 parser 显式记录。
    /// 与 `owner_fqn` 分离：方法级注解的 `owner_fqn` 是 `Class.method`，
    /// 而类级注解的 `owner_fqn` 已是 `Class`。供 `owner_class` 绑定直接取用，
    /// 避免在 Java 里按 `.` 切分时把类级注解误切成包名。PHP 侧暂未填充，
    /// 内核退回字符串切分（兼容旧行为）。
    #[serde(default)]
    pub owner_class: Option<String>,
    pub callee_text: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<FactValue>,
    pub span: Span,
    /// 该调用点所在行的源码文本（由 parser 从 `span` 提取，供 UI 直接显示语句）。
    #[serde(default)]
    pub snippet: Option<String>,
    /// 链式调用里透传下来的「目标表名」：`Db::name('goods')->insert()` 里末端动词
    /// `insert` 落在未标注类型的 Query 上，变量类型解析推不出表，但链内的
    /// `name('goods')` / `table('goods')` 已给出表名。由 parser 在收集调用点时
    /// 沿对象链回溯取得，供 P7 把这类门面链式动词落成 `WritesDb` / `ReadsDb`。
    #[serde(default)]
    pub db_table: Option<String>,
    /// 该调用点是否位于 `for` / `foreach` / `while` / `do-while` 的**循环体内**。
    ///
    /// 由 parser 在下降时记录（只有循环 `body` 内的调用计为 `true`，条件 / 初始化
    /// 表达式不算）。它是 N+1 检测的事实基础：图的其余部分完全没有「循环」概念 ——
    /// `CallSite` 只记"谁调了谁"，不记"调了几次"，没有这个字段就无法区分
    /// 「一次查一堆」与「循环里一条条查」。
    #[serde(default)]
    pub in_loop: bool,
    /// 该调用点关切的「主领域类型」（语义身份的来源之一）。
    ///
    /// 由 parser 按调用种类填入：
    /// * `@EventListener` 处理方法的**首个形参类型**（`onOrder(OrderPlacedEvent e)` → `OrderPlacedEvent`）
    /// * `publisher.publishEvent(new OrderPlacedEvent())` 里 `new` 出来的**事件类型**
    ///
    /// FKB 据此把「同一事件类型」的发布方与订阅方归并到同一个 `Event` 节点
    /// （而非各自以方法名命名），形成真正的发布 / 订阅闭环。取值为 `Option`
    /// —— 取不到（如 `publishEvent(var)`）时由规则的 `value_fallback` 兜底。
    #[serde(default)]
    pub entity: Option<String>,
}

/// 字段声明与类型：`class -> field -> type`。
///
/// 用于 P7 按字段声明类型解析 `field.method()` 实例调用（如 Java `@Autowired`
/// 字段注入的 `service.mapper.findX()` 链路）。`type_name` 为声明处的原始类型
/// 名（含可能的泛型），P2 会剔除泛型后按 `import` 还原成 FQN。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldTypeFact {
    /// 声明该字段的类的 FQN。
    pub class_fqn: String,
    /// 字段名。
    pub field: String,
    /// 字段的原始类型名（可能含泛型）。
    pub type_name: String,
    pub span: Span,
}

/// 配置条目。
///
/// 例如 `app/event.php`：`key_path = "listen.order.pay_success"`，
/// `value = Array([ClassConst("app\\listener\\order\\OrderPaySuccessListener"), ...])`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntryFact {
    /// 点分路径，如 `listen.order.pay_success`。
    pub key_path: String,
    pub value: FactValue,
    pub span: Span,
}

/// 响应头赋值事实：用于检测「CORS 反射源站」。
///
/// 解析期只捕获「左侧是 `Access-Control-Allow-Origin` 这类响应头下标键」的赋值
/// （如 `$header['Access-Control-Allow-Origin'] = app()->request->header('origin')`），
/// 真正的反射判定放在 `phase::cors` —— 需结合右侧是否读取了请求 Origin。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderAssignFact {
    /// 响应头名（数组键原文去引号、转小写），如 `access-control-allow-origin`。
    pub key: String,
    /// 赋值右侧源码片段（用于判断是否来自请求 Origin）。
    pub rhs_snippet: String,
    /// 文件（在 `cf_ast` 阶段由 `file.path` 填充）。
    pub file: String,
    pub span: Span,
}

/// 签名值的相等性比较：`$this->CreatedSign($params) != $params['sign']`。
///
/// 为什么需要单独一类事实：**比较不是调用点**，`CallSite` 里永远看不到 `==`。
/// 而"验签是否做对"恰恰取决于这一步 —— 用 `==` / `!=` 比签名会引入 PHP 松散比较
/// 的类型混淆（`0e...` 摘要互判相等）与非恒定时间（可计时侧信道）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignCompareFact {
    /// 比较左侧源码文本。
    pub left: String,
    /// 比较右侧源码文本。
    pub right: String,
    /// 运算符原文（只可能是 `==` / `!=`）。
    pub operator: String,
    /// 所在方法 / 函数的 FQN：判定时要把它与"同函数内的签名计算调用"对齐
    /// （`$sign == $ipay_signature` 这种两边都是变量的写法，签名计算在别处）。
    pub owner_fqn: String,
    /// 文件（在 `cf_ast` 阶段由 `file.path` 填充）。
    pub file: String,
    pub span: Span,
}

/// 语言的命名空间 / 成员书写规则。
///
/// 从 [`crate::port::LanguageParser`] 抽取后随流水线传递 —— 内核多数位置拿不到
/// 解析器注册表，但都拿得到流水线上下文。这些信息此前以 `\\` 与 `::` 字面量
/// 散落在内核各处，换一门语言就要全量改动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespacePolicy {
    /// 首选命名空间分隔符（用于**拼接**）。
    pub ns_separator: char,
    /// 全部可能的命名空间分隔符（用于归一化与**匹配容错**）。
    pub ns_separators: Vec<char>,
    /// 类与成员之间的分隔符：PHP/C++ 是 `::`，Java/JS/Python 是 `.`。
    pub member_separator: String,
}

impl NamespacePolicy {
    /// 从解析器抽取该语言的书写规则。
    pub fn from_parser(p: &dyn crate::port::LanguageParser) -> Self {
        let seps = p.namespace_separator();
        Self {
            ns_separator: seps.first().copied().unwrap_or('\\'),
            ns_separators: seps.to_vec(),
            member_separator: p.member_separator().to_string(),
        }
    }

    /// 类 + 成员 → 成员的完全限定名。
    pub fn join_member(&self, class_fqn: &str, member: &str) -> String {
        format!("{}{}{}", class_fqn, self.member_separator, member)
    }

    /// PHP 风格（`\` 与 `::`）。
    ///
    /// **未装配语言策略时的兜底**，与改造前内核的硬编码等价。
    /// 全量接入 `LanguageParser` 后，正常路径都应走 [`Self::from_parser`]。
    pub fn php() -> Self {
        Self {
            ns_separator: '\\',
            ns_separators: vec!['\\'],
            member_separator: "::".to_string(),
        }
    }
}

impl Default for NamespacePolicy {
    fn default() -> Self {
        Self::php()
    }
}

/// 可静态求值的字面量值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum FactValue {
    String(String),
    /// `Foo::class` —— 解析为完全限定类名。
    ClassConst(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// 关联数组 / 列表。
    Array(Vec<(String, FactValue)>),
    /// 变量等无法静态求值的值；`Option<String>` 为变量名。
    Unknown(Option<String>),
}

impl FactValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FactValue::String(s) | FactValue::ClassConst(s) => Some(s),
            _ => None,
        }
    }

    /// 数组的值列表（忽略键）。
    pub fn array_values(&self) -> Vec<&FactValue> {
        match self {
            FactValue::Array(items) => items.iter().map(|(_, v)| v).collect(),
            _ => Vec::new(),
        }
    }

    pub fn array_len(&self) -> usize {
        match self {
            FactValue::Array(items) => items.len(),
            _ => 0,
        }
    }

    /// 按字符串键取值。
    pub fn get(&self, key: &str) -> Option<&FactValue> {
        match self {
            FactValue::Array(items) => items
                .iter()
                .find(|(k, _)| k == key || k.trim_matches('\'') == key || k.trim_matches('"') == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }
}

impl Default for FactValue {
    fn default() -> Self {
        FactValue::Null
    }
}
