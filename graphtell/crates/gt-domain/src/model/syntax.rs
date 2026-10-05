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
    /// Whether instance-field receivers are bare identifiers (no `$` / `@` marker). Drives the P7
    /// `VariableType` resolver gate so the kernel needs no `language == …` hard-coding.
    pub bare_field_receivers: bool,
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
            bare_field_receivers: p.bare_field_receivers(),
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
            bare_field_receivers: false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Language;
    use crate::port::LanguageParser;
    use serde_json::{json, Value};

    // ===== `FactValue`: the statically-evaluable literal, carried through every parser as `tag = "t"`,
    // `content = "v"`. The tag/content form must round-trip for every variant, and the accessors must agree
    // with the documented semantics. =====

    #[test]
    fn fact_value_serde_roundtrips_every_variant() {
        for v in [
            FactValue::String("x".into()),
            FactValue::ClassConst("Foo".into()),
            FactValue::Int(5),
            FactValue::Float(1.5),
            FactValue::Bool(true),
            FactValue::Null,
            FactValue::Unknown(Some("var".into())),
            FactValue::Unknown(None),
            FactValue::Array(vec![
                ("k".into(), FactValue::String("v".into())),
                ("n".into(), FactValue::Int(3)),
            ]),
        ] {
            let json = serde_json::to_value(&v).expect("serializable");
            let back: FactValue = serde_json::from_value(json).expect("round-trips");
            assert_eq!(back, v);
        }
        // Unit variant serializes without a content wrapper.
        assert_eq!(
            serde_json::to_value(FactValue::Null).unwrap(),
            serde_json::json!({ "t": "Null" })
        );
    }

    #[test]
    fn fact_value_as_str_only_for_text_variants() {
        assert_eq!(FactValue::String("x".into()).as_str(), Some("x"));
        assert_eq!(FactValue::ClassConst("Foo".into()).as_str(), Some("Foo"));
        assert!(FactValue::Int(1).as_str().is_none());
        assert!(FactValue::Bool(true).as_str().is_none());
        assert!(FactValue::Null.as_str().is_none());
        assert!(FactValue::Array(vec![]).as_str().is_none());
        assert!(FactValue::Unknown(Some("v".into())).as_str().is_none());
    }

    #[test]
    fn fact_value_array_helpers_ignore_keys_and_non_arrays() {
        let arr = FactValue::Array(vec![
            ("a".into(), FactValue::Int(1)),
            ("b".into(), FactValue::String("two".into())),
        ]);
        assert_eq!(arr.array_len(), 2);
        assert_eq!(arr.array_values().len(), 2);
        assert!(matches!(arr.array_values()[1], FactValue::String(_)));
        // Non-array values degrade to empty, not a panic.
        assert_eq!(FactValue::Int(1).array_len(), 0);
        assert!(FactValue::Int(1).array_values().is_empty());
    }

    /// `get` matches by exact key OR by a key wrapped in single / double quotes — config entries come from PHP
    /// (`'listen.order'`) and JSON (`"listen.order"`) alike, and the trim is what lets one lookup hit both.
    #[test]
    fn fact_value_get_trims_quotes_and_matches_keys() {
        let arr = FactValue::Array(vec![
            ("'k1'".into(), FactValue::Int(1)),
            ("\"k2\"".into(), FactValue::Int(2)),
            ("k3".into(), FactValue::Int(3)),
        ]);
        assert_eq!(arr.get("k1"), Some(&FactValue::Int(1)), "single-quote trimmed");
        assert_eq!(arr.get("k2"), Some(&FactValue::Int(2)), "double-quote trimmed");
        assert_eq!(arr.get("k3"), Some(&FactValue::Int(3)), "exact match");
        assert!(arr.get("missing").is_none());
        // A non-array has no keys.
        assert!(FactValue::String("x".into()).get("k1").is_none());
    }

    // ===== `NamespacePolicy`: notation rules, derived from a parser. The `Default` is deliberately empty (no
    // PHP fallback) so an unwired language is visible, not misread. =====

    #[test]
    fn namespace_policy_default_is_empty_not_php() {
        let d = NamespacePolicy::default();
        assert!(d.ns_separator.is_none(), "未知语言没有任何命名空间分隔符");
        assert!(d.ns_separators.is_empty());
        assert!(d.member_separator.is_empty());
        assert!(d.variable_prefixes.is_empty());
        assert!(d.builtin_types.is_empty());
        assert!(!d.bare_field_receivers);
    }

    #[test]
    fn join_member_uses_the_member_separator() {
        let php = NamespacePolicy {
            member_separator: "::".into(),
            ..NamespacePolicy::default()
        };
        assert_eq!(php.join_member("App\\Foo", "bar"), "App\\Foo::bar");

        let java = NamespacePolicy {
            member_separator: ".".into(),
            ..NamespacePolicy::default()
        };
        assert_eq!(java.join_member("com.Foo", "bar"), "com.Foo.bar");
    }

    /// `from_parser` wires the notation rules straight from the port — the kernel reads them, never the language.
    struct FakeParser;
    impl LanguageParser for FakeParser {
        fn language(&self) -> Language { Language::new("php") }
        fn extensions(&self) -> &'static [&'static str] { &["php"] }
        fn parse(&self, _path: &str, _src: &str) -> crate::error::Result<SyntaxFacts> {
            Ok(SyntaxFacts::default())
        }
        fn namespace_separator(&self) -> &'static [char] { &['\\', '.'] }
        fn member_separator(&self) -> &'static str { "::" }
        fn variable_prefixes(&self) -> &'static [&'static str] { &["$"] }
        fn builtin_types(&self) -> &'static [&'static str] { &["int", "string"] }
        fn bare_field_receivers(&self) -> bool { true }
    }

    #[test]
    fn namespace_policy_from_parser_maps_port_fields() {
        let p = NamespacePolicy::from_parser(&FakeParser);
        assert_eq!(p.ns_separator, Some('\\'), "取第一个命名空间分隔符");
        assert_eq!(p.ns_separators, vec!['\\', '.']);
        assert_eq!(p.member_separator, "::");
        assert_eq!(p.variable_prefixes, vec!["$".to_string()]);
        assert_eq!(p.builtin_types, vec!["int".to_string(), "string".to_string()]);
        assert!(p.bare_field_receivers);
    }

    // ===== Second pass: the tests above cover `FactValue` semantics and `NamespacePolicy`, but nothing pinned
    // the **parser output contract** itself (`SyntaxFacts` and the nine fact structs) nor its backward
    // compatibility. Those structs are the only interface between a language adapter and the language-agnostic
    // pipeline: a field that silently fails to load is read as "the source code said nothing". =====

    /// `#[serde(tag = "t", content = "v")]`: the exact wire shape, including the two special cases — a unit
    /// variant carries **no** content at all, and `Unknown(None)` carries `null`.
    #[test]
    fn fact_value_wire_shape_is_tag_and_content() {
        assert_eq!(serde_json::to_value(FactValue::String("x".into())).unwrap(), json!({ "t": "String", "v": "x" }));
        assert_eq!(serde_json::to_value(FactValue::ClassConst("Foo".into())).unwrap(), json!({ "t": "ClassConst", "v": "Foo" }));
        assert_eq!(serde_json::to_value(FactValue::Int(-3)).unwrap(), json!({ "t": "Int", "v": -3 }));
        assert_eq!(serde_json::to_value(FactValue::Float(1.5)).unwrap(), json!({ "t": "Float", "v": 1.5 }));
        assert_eq!(serde_json::to_value(FactValue::Bool(false)).unwrap(), json!({ "t": "Bool", "v": false }));
        assert_eq!(
            serde_json::to_value(FactValue::Unknown(Some("v".into()))).unwrap(),
            json!({ "t": "Unknown", "v": "v" })
        );
        assert_eq!(serde_json::to_value(FactValue::Unknown(None)).unwrap(), json!({ "t": "Unknown", "v": null }));
        // Unit variant: no content wrapper.
        assert_eq!(serde_json::to_value(FactValue::Null).unwrap(), json!({ "t": "Null" }));
        // An array item serializes as a `[key, value]` pair.
        assert_eq!(
            serde_json::to_value(FactValue::Array(vec![("k".into(), FactValue::String("v".into()))])).unwrap(),
            json!({ "t": "Array", "v": [["k", { "t": "String", "v": "v" }]] })
        );
    }

    /// `Default` is `Null` ("nothing stated"), and equality is by value so two independently built facts compare
    /// equal — nested arrays included.
    #[test]
    fn fact_value_default_is_null_and_equality_is_by_value() {
        assert_eq!(FactValue::default(), FactValue::Null);
        assert_ne!(FactValue::String("x".into()), FactValue::ClassConst("x".into()), "同类文本但语义不同");
        assert_eq!(FactValue::Int(1), FactValue::Int(1));
        let nested = FactValue::Array(vec![
            ("a".into(), FactValue::Array(vec![("b".into(), FactValue::Int(1))])),
            ("c".into(), FactValue::Unknown(None)),
        ]);
        let json = serde_json::to_value(&nested).unwrap();
        assert_eq!(serde_json::from_value::<FactValue>(json).unwrap(), nested, "嵌套数组原样往返");
        assert_eq!(nested.array_len(), 2);
    }

    /// A duplicate key resolves to the **first** entry, and the quote trim removes every leading / trailing
    /// quote character (`trim_matches`, not `strip_prefix`) — worth knowing before "fixing" it into a strip.
    #[test]
    fn fact_value_get_resolves_duplicates_by_position_and_trims_all_quotes() {
        let arr = FactValue::Array(vec![
            ("dup".into(), FactValue::Int(1)),
            ("dup".into(), FactValue::Int(2)),
            ("''q''".into(), FactValue::Int(3)),
        ]);
        assert_eq!(arr.get("dup"), Some(&FactValue::Int(1)), "同名 key 取第一个");
        assert_eq!(arr.get("q"), Some(&FactValue::Int(3)));
    }

    /// `CallSiteFact` grew `owner_class` / `snippet` / `db_table` / `in_loop` / `entity` later; every one of them
    /// carries `#[serde(default)]` so a payload from an older parser still loads — with the honest defaults
    /// rather than an error (the alternative would read as "this file has no call sites").
    #[test]
    fn call_site_fact_tolerates_the_fields_added_later() {
        let old: CallSiteFact = serde_json::from_value(json!({
            "owner_fqn": "App\\Foo::bar",
            "callee_text": "Db::name",
            "args": [],
            "span": { "start_line": 1, "end_line": 1, "start_byte": 0, "end_byte": 4 }
        }))
        .expect("旧版 payload 必须能加载");
        assert!(old.owner_class.is_none(), "未填老的 owner_class 时内核回退字符串切分");
        assert!(old.snippet.is_none());
        assert!(old.db_table.is_none());
        assert!(!old.in_loop, "未标记 = 不在循环里");
        assert!(old.entity.is_none());
        assert!(old.receiver.is_none() && old.method.is_none(), "Option 字段缺失按 None");

        let full: CallSiteFact = serde_json::from_value(json!({
            "owner_fqn": "App\\Foo::bar",
            "owner_class": "App\\Foo",
            "callee_text": "Db::name",
            "receiver": "Db",
            "method": "name",
            "args": [{ "t": "String", "v": "goods" }],
            "span": { "start_line": 3, "end_line": 3, "start_byte": 10, "end_byte": 30 },
            "snippet": "Db::name('goods')->insert($data);",
            "db_table": "goods",
            "in_loop": true,
            "entity": "OrderPlacedEvent"
        }))
        .unwrap();
        assert_eq!(full.owner_class.as_deref(), Some("App\\Foo"), "类级注解的 owner_class 由 parser 显式给出");
        assert_eq!(full.snippet.as_deref(), Some("Db::name('goods')->insert($data);"));
        assert_eq!(full.db_table.as_deref(), Some("goods"));
        assert!(full.in_loop, "N+1 判断的事实基础");
        assert_eq!(full.entity.as_deref(), Some("OrderPlacedEvent"));
        assert_eq!(full.args.len(), 1);
        assert_eq!(full.args[0].as_str(), Some("goods"));
    }

    /// Backward-compatibility is **per field**, not per struct: every original collection has to be stated in
    /// JSON, while `variable_assignments` (added later) may be omitted and lands as empty. Nothing deserialises
    /// `SyntaxFacts` today — parsers build it in Rust — so this asymmetry costs nothing now, but it is exactly
    /// the kind of thing a future `#[serde(default)]` on the struct would even out. Pinned as-is.
    #[test]
    fn syntax_facts_accepts_a_document_without_the_newest_collection() {
        // All the original collections are required: an incomplete document is rejected rather than read as
        // "this file parsed to nothing".
        let missing_decl = json!({ "imports": [], "inheritances": [], "call_sites": [], "field_types": [],
                                   "config_entries": [], "header_assignments": [], "sign_compares": [] });
        assert!(
            serde_json::from_value::<SyntaxFacts>(missing_decl).is_err(),
            "缺少 declarations 必须报错，而不是当成空事实集"
        );

        let before_variable_assignments = json!({
            "declarations": [], "imports": [], "inheritances": [], "call_sites": [], "field_types": [],
            "config_entries": [], "header_assignments": [], "sign_compares": []
        });
        let facts: SyntaxFacts = serde_json::from_value(before_variable_assignments).expect("旧版文档必须能加载");
        assert!(facts.namespace.is_none());
        assert!(facts.declarations.is_empty());
        assert!(facts.imports.is_empty());
        assert!(facts.inheritances.is_empty());
        assert!(facts.call_sites.is_empty());
        assert!(facts.field_types.is_empty());
        assert!(facts.config_entries.is_empty());
        assert!(facts.header_assignments.is_empty());
        assert!(facts.sign_compares.is_empty());
        assert!(facts.variable_assignments.is_empty(), "后加的集合允许缺失");
    }

    /// Declaration / import / inheritance facts: the three structs every parser must fill, with their optional
    /// halves (`parent_fqn` absent for a top-level class, `alias` absent for a plain `use`).
    #[test]
    fn declaration_import_and_inheritance_facts_round_trip() {
        let span = Span { start_line: 10, end_line: 20, start_byte: 5, end_byte: 50 };

        let cls = Declaration {
            kind: NodeKind("Class".into()),
            name: "OrderDao".into(),
            fqn: "app\\dao\\order\\OrderDao".into(),
            parent_fqn: None,
            span,
            extra: json!({ "final": true }),
        };
        let method = Declaration {
            kind: NodeKind("Method".into()),
            name: "getList".into(),
            fqn: "app\\dao\\order\\OrderDao::getList".into(),
            parent_fqn: Some("app\\dao\\order\\OrderDao".into()),
            span,
            extra: json!({ "visibility": "public" }),
        };
        // A method's `parent_fqn` is what turns `Class::method` into two nodes with a Contains edge.
        assert_eq!(method.parent_fqn.as_deref(), Some("app\\dao\\order\\OrderDao"));
        assert!(cls.parent_fqn.is_none());
        let back: Declaration = round_trip(&method);
        assert_eq!(back.extra, json!({ "visibility": "public" }), "语言专属字段原样存取");
        assert_eq!(back.fqn, method.fqn);

        let plain = ImportFact { alias: None, name: "app\\dao\\order\\OrderDao".into(), span };
        assert!(round_trip(&plain).alias.is_none());
        let aliased = ImportFact { alias: Some("Dao".into()), name: "app\\dao\\order\\OrderDao".into(), span };
        assert_eq!(round_trip(&aliased).alias.as_deref(), Some("Dao"));

        let ext = InheritanceFact {
            child_fqn: "app\\a\\B".into(),
            base_name: "Base".into(),
            kind: EdgeKind("Extends".into()),
            span,
        };
        let back = round_trip(&ext);
        assert_eq!(back.base_name, "Base", "基名按原文保留（可能是短名或 import 别名）");
        assert_eq!(back.kind.as_str(), "Extends");
    }

    /// One composite `SyntaxFacts` carrying **every** fact kind — the shape a real adapter hands to `cf_ast`,
    /// including the two facts added for specific judgements (reflected CORS, signature comparison) and the
    /// config-entry value shape documented on `ConfigEntryFact`.
    #[test]
    fn a_complete_syntax_facts_document_round_trips() {
        let span = Span { start_line: 1, end_line: 2, start_byte: 0, end_byte: 12 };
        let facts = SyntaxFacts {
            namespace: Some("app\\services".into()),
            declarations: vec![Declaration {
                kind: NodeKind("Class".into()),
                name: "OrderPay".into(),
                fqn: "app\\services\\OrderPay".into(),
                parent_fqn: None,
                span,
                extra: Value::Null,
            }],
            imports: vec![ImportFact { alias: None, name: "think\\facade\\Db".into(), span }],
            inheritances: vec![InheritanceFact {
                child_fqn: "app\\services\\OrderPay".into(),
                base_name: "BaseServices".into(),
                kind: EdgeKind("Extends".into()),
                span,
            }],
            call_sites: vec![CallSiteFact {
                owner_fqn: "app\\services\\OrderPay::pay".into(),
                owner_class: Some("app\\services\\OrderPay".into()),
                callee_text: "Db::name".into(),
                receiver: Some("Db".into()),
                method: Some("name".into()),
                args: vec![FactValue::String("store_order".into())],
                span,
                snippet: Some("Db::name('store_order')->update($data);".into()),
                db_table: Some("store_order".into()),
                in_loop: true,
                entity: None,
            }],
            field_types: vec![FieldTypeFact {
                class_fqn: "com.x.OrderService".into(),
                field: "mapper".into(),
                type_name: "OrderMapper<Order>".into(),
                span,
            }],
            config_entries: vec![ConfigEntryFact {
                key_path: "listen.order.pay_success".into(),
                value: FactValue::Array(vec![(
                    "0".into(),
                    FactValue::ClassConst("app\\listener\\order\\OrderPaySuccessListener".into()),
                )]),
                span,
            }],
            header_assignments: vec![HeaderAssignFact {
                key: "access-control-allow-origin".into(),
                rhs_snippet: "app()->request->header('origin')".into(),
                file: "app/api/pay.php".into(),
                span,
            }],
            sign_compares: vec![SignCompareFact {
                left: "$this->CreatedSign($params)".into(),
                right: "$params['sign']".into(),
                operator: "!=".into(),
                owner_fqn: "app\\services\\Pay::verify".into(),
                file: "app/services/Pay.php".into(),
                span,
            }],
            variable_assignments: vec![VariableAssignFact {
                var: "execSql".into(),
                rhs: "str_replace('@table', $table, $sql)".into(),
                owner_fqn: "app\\services\\Upgrade::run".into(),
                file: "app/services/Upgrade.php".into(),
                span,
            }],
        };

        let back: SyntaxFacts = round_trip(&facts);
        assert_eq!(back.namespace.as_deref(), Some("app\\services"));
        assert_eq!(back.declarations.len(), 1);
        assert_eq!(back.call_sites[0].db_table.as_deref(), Some("store_order"));
        assert!(back.call_sites[0].in_loop);
        assert_eq!(back.field_types[0].type_name, "OrderMapper<Order>", "原始类型名含泛型，由 P2 还原");
        assert_eq!(
            back.config_entries[0].value.get("0").and_then(|v| v.as_str()),
            Some("app\\listener\\order\\OrderPaySuccessListener")
        );
        assert_eq!(back.header_assignments[0].key, "access-control-allow-origin");
        assert_eq!(back.sign_compares[0].operator, "!=");
        assert_eq!(back.variable_assignments[0].var, "execSql");
    }

    /// With an unknown policy (`Default`), `join_member` degrades to plain concatenation — which is why callers
    /// are expected to check for emptiness instead of trusting it.
    #[test]
    fn join_member_with_an_unknown_policy_degrades_to_concatenation() {
        let unknown = NamespacePolicy::default();
        assert_eq!(unknown.join_member("App\\Foo", "bar"), "App\\Foobar", "空分隔符 ⇒ 直接拼接（调用方必须先判空）");
    }

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
        serde_json::from_value(serde_json::to_value(v).expect("serialize")).expect("deserialize")
    }
}
