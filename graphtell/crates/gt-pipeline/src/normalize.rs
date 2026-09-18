//! identity 归一化与值变换。
//!
//! **归一化是 Synthesize 阶段幂等合并的关键**：
//! `store_order` / `eb_store_order` / `store_orders` 三条不同来源的写法
//! 必须归一化成同一个 `identity`，否则会建出三个 Table 节点，
//! 导致 fan_in 从 200 变成 67+66+67，影响面分析失真、死表检测误判。

use gt_domain::model::{NormalizeStep, TransformSpec};

/// 依次应用归一化步骤。
pub fn apply_normalize(input: &str, steps: &[NormalizeStep]) -> String {
    let mut s = input.trim().to_string();
    for step in steps {
        s = match step {
            NormalizeStep::StripPrefix(prefixes) => strip_prefixes(&s, prefixes),
            NormalizeStep::Lower => s.to_ascii_lowercase(),
            NormalizeStep::Upper => s.to_ascii_uppercase(),
            NormalizeStep::LeadingSlash => leading_slash(&s),
            NormalizeStep::Singularize => singularize(&s),
            NormalizeStep::SnakePlural => snake_plural(&s),
            NormalizeStep::StripNamespace => strip_namespace(&s),
            NormalizeStep::Trim => s.trim().to_string(),
            NormalizeStep::Replace { from, to } => s.replace(from, to),
        };
    }
    s
}

pub fn apply_transform(input: &str, t: &TransformSpec) -> String {
    let mut s = input.to_string();
    if t.strip_namespace.unwrap_or(false) {
        s = strip_namespace(&s);
    }
    if t.snake.unwrap_or(false) {
        s = to_snake(&s);
    }
    if t.snake_plural.unwrap_or(false) {
        s = snake_plural(&s);
    }
    if t.class_to_topic.unwrap_or(false) {
        s = class_to_topic(&s);
    }
    if t.lower.unwrap_or(false) {
        s = s.to_ascii_lowercase();
    }
    if t.upper.unwrap_or(false) {
        s = s.to_ascii_uppercase();
    }
    s
}

/// 规则里写空的 `strip_prefix: []` 表示「使用当前工程探测到的表前缀」。
///
/// 这样通用框架 FKB 不写死具体前缀（如 CRMEB 的 `eb_`），前缀由 P3 从
/// `config/database.php` 自动探测后填入 `workspace.table_prefixes`，再回退到这里。
pub fn apply_table_prefix_steps(
    steps: &[NormalizeStep],
    table_prefixes: &[String],
) -> Vec<NormalizeStep> {
    steps
        .iter()
        .map(|s| match s {
            NormalizeStep::StripPrefix(p) if p.is_empty() => {
                NormalizeStep::StripPrefix(table_prefixes.to_vec())
            }
            other => other.clone(),
        })
        .collect()
}

/// 反复剥离前缀（应对 `eb_eb_store_order` 这类二次前缀）。
pub fn strip_prefixes(s: &str, prefixes: &[String]) -> String {
    let mut s = s.to_string();
    let mut changed = true;
    while changed {
        changed = false;
        for p in prefixes {
            if !p.is_empty() && s.starts_with(p.as_str()) && s.len() > p.len() {
                s = s[p.len()..].to_string();
                changed = true;
            }
        }
    }
    s
}

pub fn leading_slash(s: &str) -> String {
    if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{}", s)
    }
}

pub fn strip_namespace(s: &str) -> String {
    s.rsplit(['\\', '/', ':']).next().unwrap_or(s).to_string()
}

/// `CamelCase` → `snake_case`。
pub fn to_snake(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_lower = i > 0 && chars[i - 1].is_lowercase();
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();
            if i > 0 && (prev_lower || next_lower) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(*c);
        }
    }
    out.trim_matches('_').to_string()
}

/// `StoreOrder` → `store_orders`（TP6 Model 的默认表名约定）。
pub fn snake_plural(s: &str) -> String {
    let base = strip_namespace(s);
    pluralize(&to_snake(&base))
}

pub fn singularize(s: &str) -> String {
    if s.ends_with("ies") && s.len() > 3 {
        format!("{}y", &s[..s.len() - 3])
    } else if s.ends_with("ses") && s.len() > 3 {
        s[..s.len() - 2].to_string()
    } else if s.ends_with('s') && !s.ends_with("ss") && s.len() > 1 {
        s[..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

pub fn pluralize(s: &str) -> String {
    if s.is_empty() {
        return s.to_string();
    }
    if s.ends_with('y') && !matches!(s.chars().nth(s.len() - 2), Some('a' | 'e' | 'i' | 'o' | 'u')) {
        format!("{}ies", &s[..s.len() - 1])
    } else if s.ends_with(['s', 'x', 'z']) || s.ends_with("ch") || s.ends_with("sh") {
        format!("{}es", s)
    } else if s.ends_with('s') {
        s.to_string()
    } else {
        format!("{}s", s)
    }
}

/// `app\job\OrderJob` → `order_job`（队列 topic 约定）。
pub fn class_to_topic(s: &str) -> String {
    to_snake(&strip_namespace(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_table_identity() {
        let steps = vec![
            NormalizeStep::StripPrefix(vec!["eb_".into()]),
            NormalizeStep::Singularize,
            NormalizeStep::StripPrefix(vec!["eb_".into()]),
        ];
        assert_eq!(apply_normalize("eb_store_order", &steps), "store_order");
        assert_eq!(apply_normalize("store_orders", &steps), "store_order");
        assert_eq!(apply_normalize("eb_eb_store_order", &steps), "store_order");
    }

    #[test]
    fn model_convention_table() {
        assert_eq!(snake_plural("app\\model\\StoreOrder"), "store_orders");
        assert_eq!(class_to_topic("app\\job\\OrderJob"), "order_job");
    }
}
