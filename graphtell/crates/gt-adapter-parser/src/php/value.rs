//! Static evaluation of PHP expressions into [`FactValue`].
//!
//! Only what can be determined statically is evaluated; the rest is recorded as `Unknown(var)`, keeping the
//! variable name for P7 constant propagation.

use gt_domain::model::FactValue;
use tree_sitter::Node;

use crate::ts_util::text_owned as text;

/// Evaluate an expression node into a `FactValue`.
pub fn eval_expr(node: Node, src: &str) -> FactValue {
    match node.kind() {
        "string" | "nowdoc_string" | "heredoc" => FactValue::String(unwrap_string(node, src)),
        "encapsed_string" | "heredoc_body" => eval_encapsed(node, src),
        "integer" => FactValue::Int(text(node, src).replace('_', "").parse().unwrap_or(0)),
        "float" => FactValue::Float(text(node, src).replace('_', "").parse().unwrap_or(0.0)),
        "boolean" => {
            // In tree-sitter-php a boolean constant is a `boolean` node whose text is `true` / `false`
            FactValue::Bool(text(node, src).eq_ignore_ascii_case("true"))
        }
        "null" => FactValue::Null,
        "class_constant_access_expression" => eval_class_const(node, src),
        "array_creation_expression" => eval_array(node, src),
        "qualified_name" | "name" | "relative_name" => {
            FactValue::String(text(node, src).trim_start_matches('\\').to_string())
        }
        "variable_name" | "dynamic_variable_name" => FactValue::Unknown(Some(text(node, src))),
        "parenthesized_expression" => node
            .named_child(0)
            .map(|c| eval_expr(c, src))
            .unwrap_or(FactValue::Null),
        "unary_op_expression" => {
            // Handles things like -1
            let t = text(node, src);
            if let Ok(v) = t.parse::<i64>() {
                return FactValue::Int(v);
            }
            if let Ok(v) = t.parse::<f64>() {
                return FactValue::Float(v);
            }
            FactValue::Unknown(None)
        }
        _ => {
            let t = text(node, src);
            if let Ok(v) = t.parse::<i64>() {
                return FactValue::Int(v);
            }
            FactValue::Unknown(Some(truncate(&t, 64)))
        }
    }
}

fn eval_class_const(node: Node, src: &str) -> FactValue {
    // Forms such as `Foo::class` / `self::class` / `static::class`
    let parts: Vec<String> = node
        .named_children(&mut node.walk())
        .map(|c| text(c, src).trim_start_matches('\\').to_string())
        .collect();
    let is_class_const = parts
        .last()
        .map(|p| p.eq_ignore_ascii_case("class"))
        .unwrap_or(false);
    if is_class_const && parts.len() >= 2 {
        FactValue::ClassConst(parts[..parts.len() - 1].join("\\"))
    } else {
        FactValue::Unknown(Some(text(node, src)))
    }
}

fn eval_array(node: Node, src: &str) -> FactValue {
    let mut items = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "array_element_initializer" => {
                // This node has no named field: `[key => value, ...]` has two expression children.
                let parts: Vec<Node> = child.named_children(&mut child.walk()).collect();
                let (key, value) = match parts.as_slice() {
                    [k, v] => (
                        match eval_expr(*k, src) {
                            FactValue::String(s) => s,
                            other => other.to_string_repr(),
                        },
                        eval_expr(*v, src),
                    ),
                    [v] => (items.len().to_string(), eval_expr(*v, src)),
                    _ => continue,
                };
                items.push((key, value));
            }
            "array_pair" => {
                // Compatibility with the old syntax
                let key = child.named_child(0).map(|k| text(k, src)).unwrap_or_default();
                let value = child.named_child(1).map(|v| eval_expr(v, src));
                items.push((key, value.unwrap_or(FactValue::Null)));
            }
            _ => {}
        }
    }
    FactValue::Array(items)
}

fn eval_encapsed(node: Node, src: &str) -> FactValue {
    // Interpolated strings: treated as a plain string when there is no interpolation, otherwise Unknown
    let has_interp = node
        .named_children(&mut node.walk())
        .any(|c| matches!(c.kind(), "variable_name" | "member_access_expression" | "subscript_expression"));
    if has_interp {
        FactValue::Unknown(Some(truncate(&text(node, src), 64)))
    } else {
        FactValue::String(unwrap_string(node, src))
    }
}

/// Strip the quotes from a string literal.
pub fn unwrap_string(node: Node, src: &str) -> String {
    let t = text(node, src);
    let t = t.trim();
    if t.starts_with("<<<") {
        // heredoc / nowdoc: drop the `<<<EOT` opener line **and** the closing marker line. Keeping the
        // marker used to leak `EOT` into the value, corrupting every identity built from it.
        let body: Vec<&str> = t.lines().skip(1).collect();
        return body
            .split_last()
            .map(|(_, rest)| rest.join("\n"))
            .unwrap_or_default();
    }
    let bytes = t.as_bytes();
    // Both ends must be ASCII quotes before byte-slicing (avoids a panic on a multi-byte character boundary).
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && (bytes[bytes.len() - 1] == b'\'' || bytes[bytes.len() - 1] == b'"')
    {
        let inner = &t[1..t.len() - 1];
        return inner.replace("\\'", "'").replace("\\\"", "\"");
    }
    t.to_string()
}



fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

/// The textual representation of a `FactValue` (fallback for `Unknown` and similar cases).
pub trait StringRepr {
    fn to_string_repr(&self) -> String;
}

impl StringRepr for FactValue {
    fn to_string_repr(&self) -> String {
        match self {
            FactValue::String(s) | FactValue::ClassConst(s) => s.clone(),
            FactValue::Int(i) => i.to_string(),
            FactValue::Float(f) => f.to_string(),
            FactValue::Bool(b) => b.to_string(),
            FactValue::Null => "null".to_string(),
            FactValue::Array(_) => "array".to_string(),
            FactValue::Unknown(v) => v.clone().unwrap_or_else(|| "?".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::php::PhpParser;
    use gt_domain::port::LanguageParser;

    /// Every argument list of the calls to `method`, in source order.
    fn call_args(src: &str, method: &str) -> Vec<Vec<FactValue>> {
        let facts = PhpParser::new().unwrap().parse("t.php", src).unwrap();
        facts
            .call_sites
            .iter()
            .filter(|c| c.method.as_deref() == Some(method))
            .map(|c| c.args.clone())
            .collect()
    }

    /// What can be evaluated statically is what FKB's `require_literal` can use as identity — so each mapping
    /// here decides whether a rule can fire.
    #[test]
    fn eval_expr_maps_statically_known_literals() {
        let src = "<?php
class M {
    public function run($x) {
        $o->m('abc');
        $o->m(123);
        $o->m(true);
        $o->m(null);
        $o->m(Foo::class);
        $o->m($x);
        $o->m(-1);
        $o->m(1.5);
    }
}
";
        let all = call_args(src, "m");
        assert_eq!(all.len(), 8, "expected 8 call sites, got: {all:?}");
        assert!(matches!(all[0][0], FactValue::String(ref s) if s == "abc"));
        assert!(matches!(all[1][0], FactValue::Int(123)));
        assert!(matches!(all[2][0], FactValue::Bool(true)));
        assert!(matches!(all[3][0], FactValue::Null));
        assert!(matches!(all[4][0], FactValue::ClassConst(ref s) if s == "Foo"));
        assert!(
            matches!(all[5][0], FactValue::Unknown(Some(ref s)) if s == "$x"),
            "a variable must keep its name so P7 constant propagation can use it"
        );
        assert!(matches!(all[6][0], FactValue::Int(-1)), "a unary minus must fold into an Int");
        assert!(matches!(all[7][0], FactValue::Float(f) if (f - 1.5).abs() < 1e-9));
    }

    /// Interpolation makes a string statically unknowable (it must never become identity), while arrays keep
    /// their keys — including the positional fallback for unkeyed elements.
    #[test]
    fn eval_expr_keeps_arrays_and_rejects_interpolation() {
        let src = "<?php
class M {
    public function run($x) {
        $o->m(['a' => 1, 2]);
        $o->m(\"pre$x\");
        $o->m('a\\'b');
    }
}
";
        let all = call_args(src, "m");
        assert_eq!(all.len(), 3, "expected 3 call sites, got: {all:?}");
        match &all[0][0] {
            FactValue::Array(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].0, "a");
                assert!(matches!(items[0].1, FactValue::Int(1)));
                assert_eq!(items[1].0, "1", "a key-less element uses its index as the key");
                assert!(matches!(items[1].1, FactValue::Int(2)));
            }
            other => panic!("an array literal must fold into Array: {other:?}"),
        }
        assert!(
            matches!(all[1][0], FactValue::Unknown(Some(_))),
            "a string with interpolation cannot be determined statically and must not become an identity"
        );
        assert!(
            matches!(all[2][0], FactValue::String(ref s) if s == "a'b"),
            "an escaped quote must be restored"
        );
    }

    /// Used to derive array keys from non-string values, so every variant needs a stable spelling.
    #[test]
    fn to_string_repr_covers_every_variant() {
        assert_eq!(FactValue::String("s".into()).to_string_repr(), "s");
        assert_eq!(FactValue::ClassConst("App\\M".into()).to_string_repr(), "App\\M");
        assert_eq!(FactValue::Int(5).to_string_repr(), "5");
        assert_eq!(FactValue::Float(1.5).to_string_repr(), "1.5");
        assert_eq!(FactValue::Bool(true).to_string_repr(), "true");
        assert_eq!(FactValue::Bool(false).to_string_repr(), "false");
        assert_eq!(FactValue::Null.to_string_repr(), "null");
        assert_eq!(FactValue::Array(vec![]).to_string_repr(), "array");
        assert_eq!(FactValue::Unknown(Some("$x".into())).to_string_repr(), "$x");
        assert_eq!(FactValue::Unknown(None).to_string_repr(), "?");
    }

    /// Char-based, not byte-based: truncating a multi-byte identifier must not cut mid-character.
    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("abc", 2), "ab");
        assert_eq!(truncate("abc", 10), "abc", "when it is not too long it is returned verbatim");
        assert_eq!(truncate("中文字符", 2), "中文", "it is truncated by characters and must not cut in the middle of a multi-byte one");
    }

    /// `X::class` is the only class-constant form that becomes a `ClassConst`; any other constant
    /// (`Foo::BAR`) stays `Unknown`, so it can never be mistaken for a type identity.
    #[test]
    fn class_constants_separate_class_from_other_constants() {
        let src = r#"<?php
class M {
    public function run() {
        $o->m(Foo::class);
        $o->m(\App\Foo::class);
        $o->m(self::class);
        $o->m(static::class);
        $o->m(Foo::BAR);
    }
}
"#;
        let all = call_args(src, "m");
        assert_eq!(all.len(), 5, "expected 5 call sites, got: {all:?}");
        assert!(matches!(all[0][0], FactValue::ClassConst(ref s) if s == "Foo"));
        assert!(
            matches!(all[1][0], FactValue::ClassConst(ref s) if s == "App\\Foo"),
            "a leading `\\` must be stripped: {:?}",
            all[1][0]
        );
        assert!(matches!(all[2][0], FactValue::ClassConst(ref s) if s == "self"));
        assert!(matches!(all[3][0], FactValue::ClassConst(ref s) if s == "static"));
        assert!(
            matches!(all[4][0], FactValue::Unknown(Some(ref s)) if s == "Foo::BAR"),
            "a non-class constant must not be used as a type identity: {:?}",
            all[4][0]
        );
    }

    /// PHP 7.4 numeric literal separators (`1_000`) are part of the literal, not of the value.
    #[test]
    fn numeric_literals_ignore_underscore_separators() {
        let src = r#"<?php
class M {
    public function run() {
        $o->m(1_000);
        $o->m(1_0.5);
    }
}
"#;
        let all = call_args(src, "m");
        assert!(matches!(all[0][0], FactValue::Int(1000)), "underscores must be ignored: {:?}", all[0][0]);
        assert!(
            matches!(all[1][0], FactValue::Float(f) if (f - 10.5).abs() < 1e-9),
            "floats ignore underscores as well: {:?}",
            all[1][0]
        );
    }

    /// Boolean literals are case-insensitive: PHP's `TRUE` / `False` are the same constant as `true`.
    #[test]
    fn boolean_literals_are_case_insensitive() {
        let src = r#"<?php
class M {
    public function run() {
        $o->m(TRUE);
        $o->m(False);
    }
}
"#;
        let all = call_args(src, "m");
        assert!(matches!(all[0][0], FactValue::Bool(true)), "{:?}", all[0][0]);
        assert!(matches!(all[1][0], FactValue::Bool(false)), "{:?}", all[1][0]);
    }

    /// Bare constants (`PHP_EOL`) evaluate to their name, and parentheses are transparent
    /// (`(1)` is the literal `1`, not something unknowable).
    #[test]
    fn bare_constants_and_parentheses_are_evaluated() {
        let src = r#"<?php
class M {
    public function run() {
        $o->m(PHP_EOL);
        $o->m((1));
    }
}
"#;
        let all = call_args(src, "m");
        assert!(matches!(all[0][0], FactValue::String(ref s) if s == "PHP_EOL"), "{:?}", all[0][0]);
        assert!(matches!(all[1][0], FactValue::Int(1)), "parentheses must be transparent: {:?}", all[1][0]);
    }

    /// What cannot be determined statically degrades instead of being guessed: a unary expression on a
    /// variable yields `Unknown(None)`, and a computed expression keeps its source text (truncated).
    #[test]
    fn unresolvable_expressions_degrade_to_unknown() {
        let src = r#"<?php
class M {
    public function run($x) {
        $o->m(-$x);
        $o->m(1 + 2);
    }
}
"#;
        let all = call_args(src, "m");
        assert!(
            matches!(all[0][0], FactValue::Unknown(None)),
            "negating a variable cannot be determined statically: {:?}",
            all[0][0]
        );
        assert!(
            matches!(all[1][0], FactValue::Unknown(Some(ref s)) if s == "1 + 2"),
            "a computed expression keeps its source text: {:?}",
            all[1][0]
        );
    }

    /// heredoc / nowdoc: the value is the **body**, so neither the `<<<EOT` opener nor the closing marker
    /// may leak into it (a leaked `EOT` would corrupt every identity built from the string).
    #[test]
    fn heredoc_value_excludes_opener_and_closing_marker() {
        let src = r#"<?php
class M {
    public function run() {
        $o->m(<<<EOT
hello
EOT);
    }
}
"#;
        let all = call_args(src, "m");
        assert_eq!(all.len(), 1, "expected 1 call site, got: {all:?}");
        assert!(
            matches!(all[0][0], FactValue::String(ref s) if s == "hello"),
            "a heredoc value must hold only the body, without `<<<EOT` and the closing marker: {:?}",
            all[0][0]
        );
    }

    /// Array keys are not always strings: an int key and even a variable key must still yield a stable string
    /// key (`to_string_repr`), arrays must nest, and an empty literal must stay an empty Array rather than
    /// vanishing. These are the `eval_array` branches the string-keyed test above never reaches.
    #[test]
    fn array_keys_fall_back_to_the_key_repr_and_arrays_nest() {
        let src = r#"<?php
class M {
    public function run($x) {
        $o->m([1 => 'x']);
        $o->m([$x => 'v']);
        $o->m(['a' => [1, 2]]);
        $o->m([]);
    }
}
"#;
        let all = call_args(src, "m");
        assert_eq!(all.len(), 4, "expected 4 call sites, got: {all:?}");

        match &all[0][0] {
            FactValue::Array(items) => {
                assert_eq!(items[0].0, "1", "an int key keeps its numeric spelling: {items:?}");
                assert!(matches!(items[0].1, FactValue::String(ref s) if s == "x"));
            }
            other => panic!("expected an Array: {other:?}"),
        }
        match &all[1][0] {
            FactValue::Array(items) => {
                assert_eq!(items[0].0, "$x", "a variable key keeps the variable name, never an empty key: {items:?}");
            }
            other => panic!("expected an Array: {other:?}"),
        }
        match &all[2][0] {
            FactValue::Array(items) => match &items[0].1 {
                FactValue::Array(inner) => assert_eq!(inner.len(), 2, "a nested array must be evaluated too"),
                other => panic!("expected a nested Array: {other:?}"),
            },
            other => panic!("expected an Array: {other:?}"),
        }
        match &all[3][0] {
            FactValue::Array(items) => assert!(items.is_empty(), "an empty literal is an empty Array: {items:?}"),
            other => panic!("expected an Array: {other:?}"),
        }
    }

    /// `-1` folds to an Int (covered above); the sibling arm is that a fractional unary must fold to a Float
    /// rather than falling through to `Unknown(None)`.
    #[test]
    fn unary_minus_folds_a_fractional_literal_to_float() {
        let src = r#"<?php
class M {
    public function run() {
        $o->m(-1.5);
    }
}
"#;
        let all = call_args(src, "m");
        assert!(
            matches!(all[0][0], FactValue::Float(f) if (f - (-1.5)).abs() < 1e-9),
            "a unary minus on a fraction must fold into a Float: {:?}",
            all[0][0]
        );
    }

    /// A bare name is a constant reference: the leading `\` is namespace notation, not part of the name, so
    /// `\MAX_LEN` and `MAX_LEN` must resolve to the same string — otherwise the two spellings would become two
    /// identities for one constant.
    #[test]
    fn qualified_name_strips_the_leading_namespace_separator() {
        let src = r#"<?php
class M {
    public function run() {
        $o->m(\MAX_LEN);
        $o->m(MAX_LEN);
    }
}
"#;
        let all = call_args(src, "m");
        assert!(matches!(all[0][0], FactValue::String(ref s) if s == "MAX_LEN"), "{:?}", all[0][0]);
        assert!(matches!(all[1][0], FactValue::String(ref s) if s == "MAX_LEN"), "{:?}", all[1][0]);
    }

    /// `$$z` is a variable nonetheless, so it keeps its text (like `$x`) instead of degrading to
    /// `Unknown(None)` — which would lose even the fact that it is a variable reference.
    #[test]
    fn dynamic_variable_name_keeps_its_text() {
        let src = r#"<?php
class M {
    public function run($z) {
        $o->m($$z);
    }
}
"#;
        let all = call_args(src, "m");
        assert!(
            matches!(all[0][0], FactValue::Unknown(Some(ref s)) if s == "$$z"),
            "a dynamic variable must keep its name: {:?}",
            all[0][0]
        );
    }

    /// The two heredoc-family spellings the existing heredoc test does not cover, pinned as-is:
    /// * an interpolated heredoc currently resolves to a **String still holding the raw `$x`**, unlike an
    ///   interpolated double-quoted string which correctly degrades to `Unknown` (see
    ///   `eval_expr_keeps_arrays_and_rejects_interpolation`) — so it can currently become an identity. Pinned
    ///   rather than adjusted; if it is routed through `eval_encapsed` later, this is the assertion to update.
    /// * nowdoc does not reach the string arm at all and keeps its whole source text, markers included.
    #[test]
    fn interpolated_heredoc_and_nowdoc_pin_their_current_shape() {
        let src = r#"<?php
class M {
    public function run($x) {
        $o->m(<<<EOT
hi $x
EOT);
        $o->m(<<<'EOT'
plain
EOT);
    }
}
"#;
        let all = call_args(src, "m");
        assert_eq!(all.len(), 2, "expected 2 call sites, got: {all:?}");
        assert!(
            matches!(all[0][0], FactValue::String(ref s) if s == "hi $x"),
            "interpolated heredoc: {:?}",
            all[0][0]
        );
        assert!(
            matches!(all[1][0], FactValue::Unknown(Some(_))),
            "nowdoc keeps its whole source text, markers included: {:?}",
            all[1][0]
        );
    }
}
