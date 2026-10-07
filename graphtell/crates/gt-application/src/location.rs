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

    /// A degenerate empty stored path joins to the root itself (with a trailing separator on Unix); pin this so a
    /// future change that special-cases empty input stays intentional.
    #[test]
    fn absolute_joins_empty_relative_path_to_root() {
        assert_eq!(absolute("", Some(Path::new("/root"))), "/root/");
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
        assert_eq!(line, None, "start_line=0 must not expose line");
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
        assert_eq!(line, Some(1), "only the first location is taken");
    }

    /// A syntax node whose `file_id` resolves, with no project root, yields the raw (relative) path — the `absolute`
    /// `None`-root branch must be reached through the syntax path, not only unit-tested directly.
    #[test]
    fn syntax_node_with_root_none_returns_raw_path() {
        let mut n = tnode(10, "Method", "f", None, None);
        n.file_id = Some(FileId::new(7));
        n.span = Span { start_line: 42, ..Span::default() };
        let files = HashMap::from([(7i64, "app/Order.php".to_string())]);
        let (path, line) = node_location(&n, &files, None);
        assert_eq!(path, Some("app/Order.php".to_string()), "with no root no absolute path is joined");
        assert_eq!(line, Some(42));
    }

    /// A node that has a resolvable `file_id` must use it even when `properties.locations` is also present: the
    /// file_id branch returns early, so a regression that reorders the checks would silently switch to the synthetic
    /// definition and send the jump to the wrong file.
    #[test]
    fn resolvable_file_id_wins_over_synthetic_locations() {
        let mut n = tnode(11, "Method", "f", None, None);
        n.file_id = Some(FileId::new(7));
        n.span = Span { start_line: 42, ..Span::default() };
        n.properties = serde_json::json!({ "locations": [{ "file": "sql/a.sql", "line": 99 }] });
        let files = HashMap::from([(7i64, "app/Order.php".to_string())]);
        let (path, line) = node_location(&n, &files, Some(Path::new("/root")));
        assert_eq!(path, Some("/root/app/Order.php".to_string()), "file_id takes precedence over locations");
        assert_eq!(line, Some(42));
    }

    /// A synthetic location carrying a `file` but no `line` must yield `(Some(path), None)` — not error out and not
    /// invent a line.
    #[test]
    fn synthetic_location_with_file_but_no_line() {
        let mut n = tnode(12, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": [{ "file": "sql/a.sql" }] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, Some("/root/sql/a.sql".to_string()));
        assert_eq!(line, None, "a missing line must not be exposed as 0");
    }

    /// A synthetic node in a project with no known root yields the relative path as-is (still usable for display).
    #[test]
    fn synthetic_relative_path_with_no_root() {
        let mut n = tnode(13, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": [{ "file": "sql/a.sql", "line": 3 }] });
        let (path, line) = node_location(&n, &HashMap::new(), None);
        assert_eq!(path, Some("sql/a.sql".to_string()));
        assert_eq!(line, Some(3));
    }

    /// A synthetic location whose `file` is a non-string (e.g. a numeric id from a malformed probe) must not be
    /// coerced into a bogus path: `as_str().unwrap_or_default()` yields `""`, so it degrades to line-only `(None, line)`.
    #[test]
    fn synthetic_location_file_non_string_degrades_to_line_only() {
        let mut n = tnode(14, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": [{ "file": 123, "line": 5 }] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, None, "non-string `file` must not become a path");
        assert_eq!(line, Some(5));
    }

    /// The first synthetic location entry may carry neither `file` nor `line` (a stale `{kind:"x"}` probe); both
    /// lookups miss, so the result is `(None, None)` — not an empty-string path.
    #[test]
    fn synthetic_first_location_has_neither_file_nor_line() {
        let mut n = tnode(15, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": [{ "kind": "definition" }] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, None);
        assert_eq!(line, None);
    }

    /// A synthetic location whose `line` is a non-integer (e.g. a string from a malformed probe) must not be
    /// coerced into a bogus line; `as_u64()` returns `None`, so the line is `None`.
    #[test]
    fn synthetic_location_line_non_numeric_yields_none_line() {
        let mut n = tnode(16, "Table", "x", None, Some("Table:x"));
        n.properties = serde_json::json!({ "locations": [{ "file": "sql/a.sql", "line": "ten" }] });
        let (path, line) = node_location(&n, &HashMap::new(), Some(Path::new("/root")));
        assert_eq!(path, Some("/root/sql/a.sql".to_string()));
        assert_eq!(line, None, "non-integer `line` must not be exposed");
    }
}
