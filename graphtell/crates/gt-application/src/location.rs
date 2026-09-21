//! 节点 → `path:line` 的统一解析。
//!
//! # 为什么需要它
//!
//! 合成节点（`Table` / `HttpContract` / `Event` …）**必然来自多处共现**：
//! 一张表由 SQL 建表、Model 的 `$table`、各调用点共同定义，所以它没有单一的
//! `file_id`，而是把全部位置记在 `properties.locations` 里。
//!
//! 语法节点（`Method` / `Class` …）则直接有 `file_id` + `span`。
//!
//! 违规与召回都要给出 `path:line`，走两条路很容易出现"有的跳转得了、有的跳不了"，
//! 因此收敛成一处。

use std::path::Path;

use gt_domain::model::Node;

/// 解析节点位置，返回 `(path, line)`。
///
/// * `files` —— `file_id -> 绝对路径`（由 `GraphQuery::file_paths` 批量取回）。
/// * `root` —— 工程根路径；`locations` 里存的是**相对路径**时用它拼成绝对路径，
///   以便前端直接拼 `vscode://file/...` 跳转。
pub fn node_location(
    node: &Node,
    files: &std::collections::HashMap<i64, String>,
    root: Option<&Path>,
) -> (Option<String>, Option<u32>) {
    // 1) 语法节点：file_id + span 最直接
    //
    // `source_files.path` 存的是**相对工程根**的路径，与合成节点 `locations` 里的
    // `file` 同口径；两处都必须按同一个规则拼成绝对路径，否则前端会出现
    // "一半能跳转、一半不能" 的割裂感。
    if let Some(fid) = node.file_id {
        if let Some(raw) = files.get(&fid.get()) {
            let line = if node.span.start_line > 0 { Some(node.span.start_line) } else { None };
            return (Some(absolute(raw, root)), line);
        }
    }

    // 2) 合成节点：取 `properties.locations` 的第一处
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

/// 相对路径拼成绝对路径；已是绝对路径则原样返回。
fn absolute(raw: &str, root: Option<&Path>) -> String {
    match root {
        Some(root) if Path::new(raw).is_relative() => {
            root.join(raw).to_string_lossy().to_string()
        }
        _ => raw.to_string(),
    }
}
