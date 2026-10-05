//! Unified resolution of a node -> `path:line`.
//!
//! # Why it is needed
//!
//! A synthetic node (`Table` / `HttpContract` / `Event` …) **necessarily co-occurs in several places**: one table is
//! defined jointly by the SQL `CREATE TABLE`, the Model's `$table`, and each call site, so it has no single
//! `file_id` — instead all of its locations are recorded in `properties.locations`.
//!
//! A syntax node (`Method` / `Class` …) has a direct `file_id` + `span`.
//!
//! Both violations and recall need to produce `path:line`, and taking two different paths easily ends in "some
//! jumps work, some do not", so it is converged into one place.

use std::path::Path;

use gt_domain::model::Node;

/// Resolve a node's location, returning `(path, line)`.
///
/// * `files` — `file_id -> absolute path` (fetched in bulk by `GraphQuery::file_paths`).
/// * `root` — the project root path; used to turn the **relative paths** stored in `locations` into absolute ones
///   so the frontend can build a `vscode://file/...` jump directly.
pub fn node_location(
    node: &Node,
    files: &std::collections::HashMap<i64, String>,
    root: Option<&Path>,
) -> (Option<String>, Option<u32>) {
    if let Some(fid) = node.file_id {
        if let Some(raw) = files.get(&fid.get()) {
            let line = if node.span.start_line > 0 { Some(node.span.start_line) } else { None };
            return (Some(absolute(raw, root)), line);
        }
    }

    // 2) A synthetic node: take the first entry of `properties.locations`
    let Some(locations) = node.properties.get("locations").and_then(|v| v.as_array()) else {
        return (None, None);
    };
    let Some(first) = locations.first() else {
        return (None, None);
    };
    let raw = first.get("file").and_then(|v| v.as_str()).unwrap_or_default();
    if raw.is_empty() {
        return (None, first.get("line").and_then(|v| v.as_u64()).map(|v| v as u32));
    }
    let line = first.get("line").and_then(|v| v.as_u64()).map(|v| v as u32);
    (Some(absolute(raw, root)), line)
}

/// Turn a relative path into an absolute one; an already absolute path is returned as-is.
fn absolute(raw: &str, root: Option<&Path>) -> String {
    match root {
        Some(root) if Path::new(raw).is_relative() => {
            root.join(raw).to_string_lossy().to_string()
        }
        _ => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::{
        FileId, IdentityKey, Language, Node, NodeId, NodeKind, Phase, ProjectId, Span,
    };
    use std::collections::HashMap;
    use std::path::Path;

    fn tnode(id: i64, kind: &str, name: &str, fqn: Option<&str>, identity: Option<&str>) -> Node {
        Node {
            id: NodeId::new(id),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            kind: NodeKind::new(kind),
            name: name.to_string(),
            fqn: fqn.map(|s| s.to_string()),
            identity: identity.map(IdentityKey::fqn),
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: serde_json::json!({}),
        }
    }

    // ---- absolute() ----

    #[test]
    fn absolute_keeps_already_absolute_path() {
        assert_eq!(
            absolute("/abs/path/file.php", Some(Path::new("/root"))),
            "/abs/path/file.php"
        );
    }

    #[test]
    fn absolute_joins_relative_path_under_root() {
        assert_eq!(
            absolute("app/Order.php", Some(Path::new("/root"))),
            "/root/app/Order.php"
        );
    }

    #[test]
    fn absolute_without_root_returns_raw() {
        assert_eq!(absolute("app/Order.php", None), "app/Order.php");
    }

    // ---- node_location() ----

    #[test]
    fn syntax_node_uses_file_id_and_span_line() {
        let mut n = tnode(1, "Method", "createOrder", None, None);
        n.file_id = Some(FileId::new(7));
        n.span = Span {
            start_line: 42,
            ..Span::default()
        };
        let files = HashMap::from([(7i64, "app/Order.php".to_string())]);
        let (path, line) = node_location(&n, &files, Some(Path::new("/root")));
        assert_eq!(path, Some("/root/app/Order.php".to_string()));
        assert_eq!(line, Some(42));
    }

    #[test]
    fn missing_file_id_falls_back_to_synthetic_locations() {
        let mut n = tnode(2, "Table", "store_order", None, Some("Table:store_order"));
        n.properties = serde_json::json!({ "locations": [{ "file": "sql/order.sql", "line": 10 }] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, Some("/root/sql/order.sql".to_string()));
        assert_eq!(line, Some(10));
    }

    /// A present-but-unknown file_id must not hide the synthetic locations (otherwise jumps to a real definition
    /// silently disappear when the file map is incomplete).
    #[test]
    fn file_id_present_but_unknown_still_uses_synthetic() {
        let mut n = tnode(3, "Table", "t", None, Some("Table:t"));
        n.file_id = Some(FileId::new(99));
        n.properties = serde_json::json!({ "locations": [{ "file": "/abs/t.sql", "line": 5 }] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        // already-absolute path is returned as-is
        assert_eq!(path, Some("/abs/t.sql".to_string()));
        assert_eq!(line, Some(5));
    }

    #[test]
    fn no_file_id_and_no_locations_is_none() {
        let n = tnode(4, "Table", "x", None, Some("Table:x"));
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, None);
        assert_eq!(line, None);
    }

    /// A synthetic location with no `file` but a `line` must degrade to line-only (path None) rather than erroring.
    #[test]
    fn empty_locations_file_falls_back_to_line_only() {
        let mut n = tnode(5, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": [{ "line": 7 }] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, None);
        assert_eq!(line, Some(7));
    }

    /// A syntax node with `start_line == 0` resolves to a real path but no line — the `> 0` guard in the file_id
    /// branch must not surface a bogus line 0 in the frontend jump.
    #[test]
    fn syntax_node_with_zero_start_line_has_path_but_no_line() {
        let mut n = tnode(6, "Method", "f", None, None);
        n.file_id = Some(FileId::new(7));
        // Span::default() has start_line == 0; do not override it.
        let files = HashMap::from([(7i64, "app/Order.php".to_string())]);
        let (path, line) = node_location(&n, &files, Some(Path::new("/root")));
        assert_eq!(path, Some("/root/app/Order.php".to_string()));
        assert_eq!(line, None, "start_line=0 不应暴露 line");
    }

    /// Malformed `locations` (here a string, not an array) must degrade to `(None, None)` rather than panic/assume.
    #[test]
    fn non_array_locations_is_none() {
        let mut n = tnode(7, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": "oops" });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, None);
        assert_eq!(line, None);
    }

    #[test]
    fn empty_locations_array_is_none() {
        let mut n = tnode(8, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": [] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, None);
        assert_eq!(line, None);
    }

    /// Only the *first* synthetic location is used (the doc contract). A bug that merged locations would send the
    /// jump to the wrong file.
    #[test]
    fn synthetic_uses_only_first_location() {
        let mut n = tnode(9, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({
            "locations": [
                { "file": "sql/a.sql", "line": 1 },
                { "file": "sql/b.sql", "line": 2 }
            ]
        });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, Some("/root/sql/a.sql".to_string()));
        assert_eq!(line, Some(1), "只取第一条 location");
    }
}
