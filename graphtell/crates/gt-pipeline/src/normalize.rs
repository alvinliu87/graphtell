//! Identity normalisation and value transformation.
//!
//! **Normalisation is the key to idempotent merging in the Synthesize phase**: the three spellings
//! `store_order` / `eb_store_order` / `store_orders`, coming from three different sources, must normalise into the
//! same `identity`; otherwise three Table nodes get built, fan_in turns from 200 into 67+66+67, impact analysis is
//! distorted and dead-table detection misjudges.

use gt_domain::model::{NormalizeStep, TransformSpec};

/// Apply the normalisation steps in order.
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
            NormalizeStep::ShortName => short_name(&s),
            NormalizeStep::ParamWildcard => param_wildcard(&s),
            NormalizeStep::StripQuery => strip_query(&s),
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

/// An empty `strip_prefix: []` in a rule means "use the table prefix detected for the current project".
///
/// That way a generic framework FKB does not hard-code a concrete prefix (e.g. CRMEB's `eb_`); the prefix is
/// detected automatically by P3 from `config/database.php`, filled into `workspace.table_prefixes`, and falls back
/// to here.
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

/// Strip prefixes repeatedly (to handle a doubled prefix like `eb_eb_store_order`).
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

/// Drop the query string starting at `?`, keeping only the path (a `?id=1` on a page-navigation URL does not affect the route identity).
pub fn strip_query(s: &str) -> String {
    match s.split_once('?') {
        Some((path, _)) => path.to_string(),
        None => s.to_string(),
    }
}

/// Fold path-parameter segments into `:*`: `/v2/invoice/detail/:id` is equivalent to `/v2/invoice/detail/:param`.
pub fn param_wildcard(s: &str) -> String {
    s.split('/')
        .map(|seg| if seg.starts_with(':') { ":*" } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn strip_namespace(s: &str) -> String {
    s.rsplit(['\\', '/', ':']).next().unwrap_or(s).to_string()
}

/// Take the last segment of a dot-separated path (recognising one more separator, `.`, than [`strip_namespace`]).
///
/// `app.tasks.send_email` -> `send_email`; a short name is unchanged (`send_email`).
/// Used to merge the fully qualified name and the short name of "the same entity" into one identity.
pub fn short_name(s: &str) -> String {
    s.rsplit(['\\', '/', ':', '.']).next().unwrap_or(s).to_string()
}

/// `CamelCase` -> `snake_case`.
///
/// Handled per **alphanumeric segment**: an upper-case letter right after a separator (`\` / `/` / `:`) gets **no
/// extra underscore** — `app\admin\Store` becomes `app\admin\store`, not `app\admin\_store`.
/// Without that, URL derivation for auto-routes sprouts a spurious `_`-prefixed segment (`RULE /admin/_store/index`).
pub fn to_snake(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_alnum = i > 0 && chars[i - 1].is_alphanumeric();
            let prev_lower = i > 0 && chars[i - 1].is_lowercase();
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();
            if prev_alnum && (prev_lower || next_lower) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(*c);
        }
    }
    out.trim_matches('_').to_string()
}

/// `StoreOrder` -> `store_orders` (the TP6 Model default table-name convention).
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

/// `app\job\OrderJob` -> `order_job` (the queue topic convention).
pub fn class_to_topic(s: &str) -> String {
    to_snake(&strip_namespace(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_takes_last_dotted_segment() {
        // Python task names: the fully qualified name restored via `import` must merge with the short name used at registration time
        assert_eq!(short_name("app.tasks.send_email"), "send_email");
        assert_eq!(short_name("send_email"), "send_email");
        // The same applies to already-separated forms
        assert_eq!(short_name("app\\service\\OrderService"), "OrderService");
    }

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

    #[test]
    fn snake_keeps_segments() {
        // No `_` before an upper-case letter after a separator: otherwise auto-routes derive `/admin/_store/index`
        assert_eq!(to_snake("app\\admin\\controller\\Store"), "app\\admin\\controller\\store");
        assert_eq!(to_snake("AgentLevel::delete"), "agent_level::delete");
        assert_eq!(to_snake("SaveInfo"), "save_info");
    }
}
