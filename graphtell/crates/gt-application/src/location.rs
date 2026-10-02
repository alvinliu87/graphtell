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
