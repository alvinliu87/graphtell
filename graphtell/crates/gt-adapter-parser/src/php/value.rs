//! Static evaluation of PHP expressions into [`FactValue`].
//!
//! Only what can be determined statically is evaluated; the rest is recorded as `Unknown(var)`, keeping the
//! variable name for P7 constant propagation.

use gt_domain::model::{FactValue, Span};
use tree_sitter::Node;

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
        // heredoc / nowdoc: take everything after the first line up to the closing marker
        return t.lines().skip(1).collect::<Vec<_>>().join("\n");
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

pub fn text(node: Node, src: &str) -> String {
    src.get(node.start_byte()..node.end_byte())
        .unwrap_or("")
        .to_string()
}

pub fn span_of(node: Node) -> Span {
    let s = node.start_position();
    let e = node.end_position();
    Span {
        start_line: s.row as u32 + 1,
        end_line: e.row as u32 + 1,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
    }
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
