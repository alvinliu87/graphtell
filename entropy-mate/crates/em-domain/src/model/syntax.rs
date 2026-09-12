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
    /// 配置文件条目（如 `app/event.php` 的 `listen.*`）。
    pub config_entries: Vec<ConfigEntryFact>,
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
    pub callee_text: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<FactValue>,
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
