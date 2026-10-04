//! Language-agnostic "syntax facts" — the output contract of a parser adapter.
//!
//! # Why this layer exists
//!
//! The concrete syntax tree differs for every language. If the pipeline consumed `tree_sitter::Node`
//! directly, "supporting every mainstream tech stack" would be out of the question.
//!
//! So a **stable intermediate representation** is defined: a parser adapter for any language only has to
//! translate its own syntax tree into [`SyntaxFacts`], after which the CfAst / Synthesize / Resolve phases are
//! completely language-independent. That is exactly what "port isolation" means in a hexagonal architecture.

use serde::{Deserialize, Serialize};

use super::graph::Span;
use super::kinds::{EdgeKind, NodeKind};

/// All syntax facts parsed out of one source file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyntaxFacts {
    /// Namespace (PHP `namespace app\services\order;`).
    pub namespace: Option<String>,
    /// Declarations: class / interface / trait / enum / method / function / property / constant.
    pub declarations: Vec<Declaration>,
    /// Imports: `use app\dao\order\StoreOrderDao;`.
    pub imports: Vec<ImportFact>,
    /// Inheritance / implementation / trait.
    pub inheritances: Vec<InheritanceFact>,
    /// One call site inside a method body.
    pub call_sites: Vec<CallSiteFact>,
    /// Field declarations and their types (Java `@Autowired` field injection, etc.): `class -> field -> type`,
    /// so P7 can resolve `field.method()` instance calls by declared field type (the service -> mapper chain).
    pub field_types: Vec<FieldTypeFact>,
    /// Config-file entries (e.g. `listen.*` from `app/event.php`).
    pub config_entries: Vec<ConfigEntryFact>,
    /// Response-header assignments (e.g. `$header['Access-Control-Allow-Origin'] = ...`), for reflected-CORS detection.
    pub header_assignments: Vec<HeaderAssignFact>,
    /// **Equality comparisons** of a signature value (`$sign == $calc` / `$params['sign'] != ...`),
    /// used by the signature-quality judgement (loose comparison / non-constant time).
    ///
    /// Only `==` / `!=` are collected (`===` / `!==` are strict comparisons and not the problem being looked for),
    /// and only when **at least one side looks like a signature value** — otherwise tens of thousands of
    /// comparisons across a project would come in and explode the fact set (the CORS fact likewise collects only
    /// the single Allow-Origin header).
    pub sign_compares: Vec<SignCompareFact>,
    /// **Local variable assignments** inside a method body (`$sql = ...;` / `$sql .= ...;`).
    ///
    /// Lets P9 Taint do backward tracking **within one function**: to decide whether a variable concatenated into
    /// SQL really comes from the request. Without it, Taint can only look at "is there a `$var`" in the call site's
    /// argument text — so a whole-variable form like `Db::execute($execSql)` is always judged high-risk, even
    /// though that variable comes from a local file shipped with the release package (measured on CRMEB: 34 of
    /// 40 entries were exactly that).
    #[serde(default)]
    pub variable_assignments: Vec<VariableAssignFact>,
}

/// One local variable assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableAssignFact {
    /// Variable name (without `$`), e.g. `execSql`.
    pub var: String,
    /// Raw source text of the right-hand side, e.g. `str_replace('@table', $table, $sql)`.
    pub rhs: String,
    /// FQN of the enclosing method / function (the scope boundary for backward tracking).
    pub owner_fqn: String,
    /// File (filled in from `file.path` during the `cf_ast` phase, same as `SignCompareFact`).
    pub file: String,
    pub span: Span,
}

/// A declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Declaration {
    pub kind: NodeKind,
    /// Short name.
    pub name: String,
    /// Fully qualified name (derived from namespace + parent declaration + short name).
    pub fqn: String,
    /// FQN of the parent declaration (the method's owning class, the constant's owning class, etc.).
    pub parent_fqn: Option<String>,
    pub span: Span,
    /// Language-specific fields: visibility, `static`, default value, `abstract`, etc.
    pub extra: serde_json::Value,
}

/// An import fact. Used to build the `by_name` index and the short name -> FQN mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportFact {
    /// The `Dao` in `use app\dao\order\StoreOrderDao as Dao;`; `None` when there is no alias.
    pub alias: Option<String>,
    /// The imported name (possibly relative to a namespace).
    pub name: String,
    pub span: Span,
}

/// Inheritance / implementation / trait use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InheritanceFact {
    /// FQN of the subclass / implementing class.
    pub child_fqn: String,
    /// Name of the base class / interface / trait (verbatim; may be a short name or an import alias).
    pub base_name: String,
    pub kind: EdgeKind,
    pub span: Span,
}

/// A call site.
///
/// Records the complete information of "how a call was written", for later rule matching and P7 resolution:
/// * `callee_text` — verbatim, e.g. `Db::name`
/// * `receiver` — `$this->dao`, `Cache`, or `None` (a free function)
/// * `method` — `name`
/// * `args` — literal arguments (whatever can be evaluated statically)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSiteFact {
    /// FQN of the enclosing method / function (its own FQN for a free function).
    pub owner_fqn: String,
    /// The **class** FQN this call site sits in, recorded explicitly by the parser.
    /// Kept separate from `owner_fqn`: a method-level annotation's `owner_fqn` is `Class.method`, while a
    /// class-level annotation's `owner_fqn` is already `Class`. This is what `owner_class` binding reads
    /// directly, avoiding the case where Java splits a class-level annotation on `.` and mistakes it for a
    /// package name. The PHP side does not fill it yet and the kernel falls back to string splitting
    /// (legacy behaviour preserved).
    #[serde(default)]
    pub owner_class: Option<String>,
    pub callee_text: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<FactValue>,
    pub span: Span,
    /// Source text of the line this call site sits on (extracted by the parser from `span`, so the UI can show the statement directly).
    #[serde(default)]
    pub snippet: Option<String>,
    /// The "target table name" passed down a chained call: in `Db::name('goods')->insert()` the terminal verb
    /// `insert` lands on an untyped Query, so variable-type resolution cannot infer the table — but the chain's
    /// `name('goods')` / `table('goods')` already gave it. The parser walks back along the object chain when
    /// collecting the call site, so P7 can turn such facade chained verbs into `WritesDb` / `ReadsDb`.
    #[serde(default)]
    pub db_table: Option<String>,
    /// Whether this call site sits inside the **body** of a `for` / `foreach` / `while` / `do-while`.
    ///
    /// Recorded by the parser as it descends (only calls inside the loop `body` count as `true`; the condition /
    /// initialisation expressions do not). It is the factual basis of N+1 detection: no other part of the graph
    /// has any concept of a loop — `CallSite` only records "who called whom", not "how many times", and without
    /// this field "fetch a batch in one query" cannot be told apart from "fetch row by row in a loop".
    #[serde(default)]
    pub in_loop: bool,
    /// The "primary domain type" this call site is about (one source of semantic identity).
    ///
    /// Filled in by the parser according to the kind of call:
    /// * the **first parameter type** of an `@EventListener` handler method (`onOrder(OrderPlacedEvent e)` -> `OrderPlacedEvent`)
    /// * the **event type** produced by `new` in `publisher.publishEvent(new OrderPlacedEvent())`
    ///
    /// From this, FKB merges the publisher and the subscriber of "the same event type" onto one `Event` node
    /// (rather than naming each after its method), forming a real publish / subscribe loop. It is an `Option`
    /// — when it cannot be obtained (e.g. `publishEvent(var)`) the rule's `value_fallback` covers the gap.
    #[serde(default)]
    pub entity: Option<String>,
}

/// A field declaration and its type: `class -> field -> type`.
///
/// Used by P7 to resolve `field.method()` instance calls from the declared field type (e.g. the
/// `service.mapper.findX()` chain built by Java `@Autowired` field injection). `type_name` is the raw type
/// name as declared (possibly generic); P2 strips the generics and restores it into an FQN via `import`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldTypeFact {
    /// FQN of the class declaring this field.
    pub class_fqn: String,
    /// Field name.
    pub field: String,
    /// Raw declared type name of the field (possibly generic).
    pub type_name: String,
    pub span: Span,
}

/// A config entry.
///
/// For example in `app/event.php`: `key_path = "listen.order.pay_success"`,
/// `value = Array([ClassConst("app\\listener\\order\\OrderPaySuccessListener"), ...])`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntryFact {

    /// Dotted path, e.g. `listen.order.pay_success`.
    pub key_path: String,
    pub value: FactValue,
    pub span: Span,
}

/// A response-header assignment fact: used to detect "CORS reflected origin".
///
/// Parsing only captures assignments **whose left side is a response-header subscript key** such as
/// `Access-Control-Allow-Origin` (e.g.
/// `$header['Access-Control-Allow-Origin'] = app()->request->header('origin')`); whether it is really reflected
/// is decided in `phase::cors` — that needs to combine it with whether the right side reads the request Origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderAssignFact {
    /// Response header name (the array key verbatim, unquoted and lower-cased), e.g. `access-control-allow-origin`.
    pub key: String,
    /// Source fragment of the right-hand side (used to decide whether it comes from the request Origin).
    pub rhs_snippet: String,
    /// File (filled in from `file.path` during the `cf_ast` phase).
    pub file: String,
    pub span: Span,
}

/// An equality comparison of a signature value: `$this->CreatedSign($params) != $params['sign']`.
///
/// Why this needs its own class of fact: **a comparison is not a call site**, and `CallSite` can never show
/// `==`. Yet "whether signature verification is done right" depends exactly on this step — comparing
/// signatures with `==` / `!=` brings in PHP's loose-comparison type confusion (digests of the `0e...` form
/// judge each other equal) and non-constant time (a timing side channel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignCompareFact {
    /// Source text of the left-hand side.
    pub left: String,
    /// Source text of the right-hand side.
    pub right: String,
    /// The operator verbatim (it can only be `==` / `!=`).
    pub operator: String,
    /// FQN of the enclosing method / function: the judgement has to align it with "the signature computation
    /// call inside the same function" (`$sign == $ipay_signature` has variables on both sides, so the
    /// signature is computed elsewhere).
    pub owner_fqn: String,
    /// File (filled in from `file.path` during the `cf_ast` phase).
    pub file: String,
    pub span: Span,
}

/// A language's namespace / member notation rules.
///
/// Extracted from [`crate::port::LanguageParser`] and carried along the pipeline — most places in the kernel
/// cannot reach the parser registry, but they all have the pipeline context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespacePolicy {
    /// The preferred namespace separator (used for **joining**); `None` when this language's notation is
    /// unknown — there is no neutral default to fall back on, so callers must skip the join instead of
    /// guessing one stack's separator.
    pub ns_separator: Option<char>,
    /// Every possible namespace separator (for normalisation and **matching tolerance**).
    pub ns_separators: Vec<char>,
    /// The separator between a class and its member: PHP/C++ use `::`, Java/JS/Python use `.`.
    pub member_separator: String,
    /// How a variable reference is marked inside a string (PHP `$`; JS `` `${` ``; none for languages that
    /// give no textual marker). Empty = "cannot tell a variable from text", and callers must not guess.
    pub variable_prefixes: Vec<String>,
    /// Primitive / builtin type names that are NOT class references (language-specific).
    pub builtin_types: Vec<String>,
}

impl NamespacePolicy {
    /// Extract this language's notation rules from a parser.
    pub fn from_parser(p: &dyn crate::port::LanguageParser) -> Self {
        let seps = p.namespace_separator();
        Self {
            // No `unwrap_or('\\')`: a parser that declares no separator means "unknown", and defaulting to
            // PHP's would hand every other stack PHP's notation.
            ns_separator: seps.first().copied(),
            ns_separators: seps.to_vec(),
            member_separator: p.member_separator().to_string(),
            variable_prefixes: p
                .variable_prefixes()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            builtin_types: p
                .builtin_types()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    /// class + member -> the member's fully qualified name.
    pub fn join_member(&self, class_fqn: &str, member: &str) -> String {
        format!("{}{}{}", class_fqn, self.member_separator, member)
    }

}

impl Default for NamespacePolicy {
    /// "No language strategy wired up" — and therefore **no notation knowledge**, not PHP's.
    ///
    /// This used to be `Self::php()`, which meant every unrecognised language silently inherited PHP's
    /// `\` namespace separator, `::` member separator and builtin-type list. For a multi-stack tool that is
    /// the wrong default twice over: it misreads other stacks (a Java FQN is never split on `\`) and it hides
    /// the real problem (a parser not being wired in) behind plausible-looking output. Empty means every
    /// consumer sees "unknown" and skips the notation-dependent step, which is honest and visible.
    fn default() -> Self {
        Self {
            ns_separator: None,
            ns_separators: Vec::new(),
            member_separator: String::new(),
            variable_prefixes: Vec::new(),
            builtin_types: Vec::new(),
        }
    }
}

/// A statically evaluable literal value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum FactValue {
    String(String),
    /// `Foo::class` — resolves to a fully qualified class name.
    ClassConst(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// An associative array / list.

    Array(Vec<(String, FactValue)>),
    /// A variable or other value that cannot be evaluated statically; `Option<String>` holds the variable name.
    Unknown(Option<String>),
}

impl FactValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FactValue::String(s) | FactValue::ClassConst(s) => Some(s),
            _ => None,
        }
    }

    /// The array's value list (keys ignored).
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

    /// Look up by string key.
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
