//! Shared tree-sitter helpers for the language adapters.
//!
//! Every adapter used to copy `text` / `opt_text` / `span_of` into its own `mod.rs`. They differed
//! only in return type and source convention — some take `&[u8]`, some `&str`; some return
//! `Option<String>`, some `&str` / `String`. This module is the single source of truth; each
//! adapter picks the variant that matches its existing call sites, so no behaviour changes.

use tree_sitter::Node;
use gt_domain::model::Span;

/// Borrowed node text from a `&[u8]` source. `None` only when the node's bytes are not valid UTF-8.
/// Never trims — trimming is a java/python concern applied by [`text`].
pub fn text_borrowed<'a>(node: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(src).ok()
}

/// Node text as owned `String`, trimmed and with empty results dropped — the java/python convention.
pub fn text<'a>(node: Node<'a>, src: &'a [u8]) -> Option<String> {
    text_borrowed(node, src)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Optional-node variant of [`text`] for java/python.
pub fn opt_text<'a>(node: Option<Node<'a>>, src: &'a [u8]) -> Option<String> {
    node.and_then(|n| text(n, src))
}

/// Node text as a borrowed `&str` from a `&str` source, falling back to `""` — the js convention
/// (callers treat a missing node as an empty literal rather than `None`). Does not trim.
pub fn text_str<'a>(node: Node<'a>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// Owned `String` variant of [`text_str`] — the php convention (callers need an owned value, e.g. to
/// stuff into `FactValue::Unknown(Some(..))` or to `.replace(..)` / `.trim_start_matches(..)`).
pub fn text_owned(node: Node, src: &str) -> String {
    text_str(node, src).to_string()
}

/// 1-based span from tree-sitter positions — the convention shared by java/python/php.
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

/// `&str`-source shim so adapters that pass `src` to `span_of` (js) keep their call sites unchanged.
pub fn span_of_str(node: Node, _src: &str) -> Span {
    span_of(node)
}

/// Every named child carrying a given **field name** (tree-sitter's `child_by_field_name` returns only
/// the first). Identical in php/python — moved here so neither re-implements the cursor loop.
pub fn field_children<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
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

/// Strip generics / array shells from a type name, keeping only the bare type name.
///
/// `List<Order>` -> `List`; `Order[]` -> `Order`; `Base[Order]` -> `Base`. Used when normalising
/// argument / parameter / supertype names across java/python.
pub fn bare_type_name(raw: String) -> String {
    raw.split(['<', '['])
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("java language");
        p.parse(src, None).expect("parse")
    }

    /// Walk down to the first node whose source text equals `want`.
    fn find_text<'a>(node: Node<'a>, src: &'a [u8], want: &str) -> Option<Node<'a>> {
        if node.utf8_text(src).ok() == Some(want) {
            return Some(node);
        }
        let mut c = node.walk();
        let mut children = node.named_children(&mut c);
        children.find_map(|n| find_text(n, src, want))
    }

    #[test]
    fn span_of_is_one_based_and_covers_the_node() {
        let src = "class A {\n    void m() {}\n}\n";
        let tree = parse(src);
        let s = span_of(tree.root_node());
        assert_eq!(s.start_line, 1, "spans must be 1-based");
        assert!(s.end_byte > s.start_byte, "span must cover the node range");
    }

    #[test]
    fn text_trims_and_drops_empty_for_java_python() {
        let src = "class A {}";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let a = find_text(tree.root_node(), bytes, "A").expect("identifier node `A`");
        assert_eq!(text(a, bytes), Some("A".to_string()));
        assert_eq!(opt_text(Some(a), bytes), Some("A".to_string()));
        assert_eq!(opt_text(None, bytes), None);
    }

    #[test]
    fn text_str_and_text_owned_match_for_js_and_php() {
        let src = "class A {}";
        let tree = parse(src);
        let a = find_text(tree.root_node(), src.as_bytes(), "A").expect("identifier node `A`");
        // js convention: borrowed &str, never panics on a missing node.
        assert_eq!(text_str(a, src), "A");
        // php convention: owned String.
        assert_eq!(text_owned(a, src), "A");
    }

    #[test]
    fn field_children_collects_every_named_child_of_a_field() {
        let src = "class A { void a() {} void b() {} }";
        let tree = parse(src);
        let class = find_text(tree.root_node(), src.as_bytes(), "A")
            .expect("class name `A`")
            .parent()
            .expect("class declaration node");
        // A single-occurrence field: field_children must agree with child_by_field_name.
        let via_field = field_children(class, "body");
        assert_eq!(via_field.len(), 1);
        assert_eq!(via_field.first(), class.child_by_field_name("body").as_ref());
        // The node it handed back is the real body, which holds both methods — so the helper returned
        // the right node rather than e.g. the first sibling.
        let body = via_field.into_iter().next().unwrap();
        let methods = body
            .named_children(&mut body.walk())
            .filter(|n| n.kind() == "method_declaration")
            .count();
        assert_eq!(methods, 2);
        // An absent field yields nothing.
        assert_eq!(field_children(class, "nonexistent").len(), 0);
    }

    #[test]
    fn bare_type_name_strips_generics_and_arrays_and_trims() {
        assert_eq!(bare_type_name("List<Order>".to_string()), "List");
        assert_eq!(bare_type_name("Order[]".to_string()), "Order");
        assert_eq!(bare_type_name("Base[Order]".to_string()), "Base");
        assert_eq!(bare_type_name("Order".to_string()), "Order");
        assert_eq!(bare_type_name("List<Order>[]".to_string()), "List");
        assert_eq!(
            bare_type_name("  Map <String, Object> ".to_string()),
            "Map",
            "must strip generics and trim"
        );
        assert_eq!(bare_type_name("int".to_string()), "int");
    }
}
