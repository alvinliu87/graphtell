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

/// The endings after which English forms the plural with `-es` rather than `-s`.
///
/// Shared by [`pluralize`] and [`singularize`] so the two stay exact mirrors: a plural form the one
/// produces must be recognised by the other, otherwise identities stop round-tripping.
fn ends_with_sibilant(s: &str) -> bool {
    s.ends_with(['s', 'x', 'z']) || s.ends_with("ch") || s.ends_with("sh")
}

/// Whether English doubles the final `z` before `-es`: it does when a vowel precedes it
/// (`quiz` -> `quizzes`), and does not when a consonant does (`waltz` -> `waltzes`, `quartz` -> `quartzes`).
fn doubles_final_z(s: &str) -> bool {
    s.ends_with('z')
        && matches!(
            s.chars().rev().nth(1).map(|c| c.to_ascii_lowercase()),
            Some('a' | 'e' | 'i' | 'o' | 'u')
        )
}

/// Whether the word already looks plural, so [`pluralize`] stays **idempotent**.
///
/// Without this, a Model class whose name is already plural degenerated: `UserRights` -> `user_rights`
/// -> `user_rightses`, because `pluralize` only ever added an ending and never recognised one.
fn is_plural(s: &str) -> bool {
    if s.ends_with("ies") {
        return true;
    }
    if s.ends_with("es") && s.len() > 3 {
        let stem = &s[..s.len() - 2];
        if ends_with_sibilant(stem) {
            return true;
        }
    }
    // A plain `-s` plural. `bus` / `status` are singular despite ending in `s`, and `address` ends in
    // `ss` — none of them is a plural, so they must still be pluralised.
    s.ends_with('s') && !s.ends_with("ss") && !s.ends_with("us") && !s.ends_with("is")
}

pub fn singularize(s: &str) -> String {
    if s.ends_with("ies") && s.len() > 3 {
        return format!("{}y", &s[..s.len() - 3]);
    }
    // Mirror of the `z`-doubling in `pluralize`: `quizzes` -> `quiz` (not `quizz`).
    if s.ends_with("zzes") && s.len() > 4 {
        return s[..s.len() - 3].to_string();
    }
    // `-es` is only a plural ending where `pluralize` would have added one (after `s` / `x` / `z` /
    // `ch` / `sh`). Without this mirror rule, `box -> boxes -> boxe`.
    if s.ends_with("es") && s.len() > 3 {
        let stem = &s[..s.len() - 2];
        if ends_with_sibilant(stem) {
            return stem.to_string();
        }
    }
    if s.ends_with('s') && !s.ends_with("ss") && s.len() > 1 {
        return s[..s.len() - 1].to_string();
    }
    s.to_string()
}

pub fn pluralize(s: &str) -> String {
    if s.is_empty() {
        return s.to_string();
    }
    // Already plural: adding another ending produced `user_rightses`.
    if is_plural(s) {
        return s.to_string();
    }
    // `chars().rev().nth(1)` is the previous character (and `None` for a one-character input, which must not
    // be reached as `s.len() - 2` — that underflows for `"y"`).
    if s.ends_with('y') && !matches!(s.chars().rev().nth(1), Some('a' | 'e' | 'i' | 'o' | 'u')) {
        format!("{}ies", &s[..s.len() - 1])
    } else if ends_with_sibilant(s) {
        if doubles_final_z(s) {
            format!("{}zes", s)
        } else {
            format!("{}es", s)
        }
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

    // ===== Below: the gaps the original 4 tests left open — `apply_transform`, the prefix machinery,
    // the URL / path helpers and the plural / singular rules. =====

    fn prefixes(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The input is trimmed once up front, then every step runs **in the declared order** — which is why
    /// `lower` before `strip_prefix` and the other way round give different results.
    #[test]
    fn apply_normalize_trims_first_and_applies_steps_in_order() {
        assert_eq!(apply_normalize("  /api/v1  ", &[NormalizeStep::Trim]), "/api/v1");
        assert_eq!(apply_normalize("  X  ", &[]), "X", "it trims first even when there are no steps");

        let lower_then_strip = vec![
            NormalizeStep::Lower,
            NormalizeStep::StripPrefix(prefixes(&["eb_"])),
        ];
        assert_eq!(apply_normalize("EB_Store", &lower_then_strip), "store");
        let strip_then_lower = vec![
            NormalizeStep::StripPrefix(prefixes(&["eb_"])),
            NormalizeStep::Lower,
        ];
        assert_eq!(
            apply_normalize("EB_Store", &strip_then_lower),
            "eb_store",
            "the case-sensitive prefix strip only takes effect after lowering first"
        );
    }

    #[test]
    fn apply_normalize_covers_the_path_and_url_steps() {
        assert_eq!(
            apply_normalize("a-b", &[NormalizeStep::Replace { from: "-".into(), to: "_".into() }]),
            "a_b"
        );
        assert_eq!(apply_normalize("api/v1", &[NormalizeStep::LeadingSlash]), "/api/v1");
        // A `?id=1` on a navigation URL must not split the route identity.
        assert_eq!(
            apply_normalize("/v2/invoice/detail/:id?page=1", &[
                NormalizeStep::StripQuery,
                NormalizeStep::ParamWildcard,
            ]),
            "/v2/invoice/detail/:*"
        );
        assert_eq!(apply_normalize("StoreOrder", &[NormalizeStep::SnakePlural]), "store_orders");
        assert_eq!(apply_normalize("app\\job\\OrderJob", &[NormalizeStep::StripNamespace]), "OrderJob");
    }

    /// An empty `strip_prefix: []` means "whatever prefix P3 detected for this project" — so a generic
    /// framework FKB never hard-codes a concrete prefix like CRMEB's `eb_`.
    #[test]
    fn apply_table_prefix_steps_fills_in_the_detected_prefix() {
        let steps = vec![
            NormalizeStep::StripPrefix(vec![]),
            NormalizeStep::Singularize,
            NormalizeStep::ShortName,
        ];
        let resolved = apply_table_prefix_steps(&steps, &prefixes(&["eb_"]));
        assert_eq!(
            resolved[0],
            NormalizeStep::StripPrefix(prefixes(&["eb_"])),
            "an empty list must be replaced by the detected prefix"
        );
        // Steps of other kinds pass through untouched.
        assert_eq!(resolved[1], NormalizeStep::Singularize);
        assert_eq!(resolved[2], NormalizeStep::ShortName);

        // End to end: the detected prefix is applied.
        assert_eq!(apply_normalize("eb_store_orders", &resolved), "store_order");
        // Declaring no prefix at all strips nothing.
        let none = apply_table_prefix_steps(&steps, &[]);
        assert_eq!(apply_normalize("eb_store_orders", &none), "eb_store_order");
        // An explicitly declared prefix is never overwritten by the detected one.
        let explicit = vec![NormalizeStep::StripPrefix(prefixes(&["other_"]))];
        assert_eq!(
            apply_table_prefix_steps(&explicit, &prefixes(&["eb_"]))[0],
            NormalizeStep::StripPrefix(prefixes(&["other_"]))
        );
    }

    #[test]
    fn strip_prefixes_strips_repeatedly_but_never_empties_the_value() {
        // Doubled prefixes are stripped repeatedly.
        assert_eq!(strip_prefixes("eb_eb_store_order", &prefixes(&["eb_"])), "store_order");
        // A value that *is* only the prefix is left alone (`s.len() > p.len()`).
        assert_eq!(strip_prefixes("eb_", &prefixes(&["eb_"])), "eb_");
        // Empty / absent prefixes do nothing.
        assert_eq!(strip_prefixes("eb_x", &prefixes(&[""])), "eb_x");
        assert_eq!(strip_prefixes("eb_x", &[]), "eb_x");
        assert_eq!(strip_prefixes("store", &prefixes(&["eb_"])), "store");
        // Several prefixes are all tried, repeatedly.
        assert_eq!(strip_prefixes("v2_eb_x", &prefixes(&["v2_", "eb_"])), "x");
    }

    #[test]
    fn leading_slash_is_idempotent() {
        assert_eq!(leading_slash("api/v1"), "/api/v1");
        assert_eq!(leading_slash("/api/v1"), "/api/v1");
        // An empty path becomes the root rather than staying empty.
        assert_eq!(leading_slash(""), "/");
    }

    #[test]
    fn strip_query_keeps_only_the_path() {
        assert_eq!(strip_query("/a/b?id=1&x=2"), "/a/b");
        assert_eq!(strip_query("/a/b"), "/a/b");
        // Only the first `?` splits.
        assert_eq!(strip_query("/a?b?c"), "/a");
        assert_eq!(strip_query("?id=1"), "");
        assert_eq!(strip_query(""), "");
    }

    #[test]
    fn param_wildcard_folds_every_parameter_segment() {
        assert_eq!(param_wildcard("/v2/invoice/detail/:id"), "/v2/invoice/detail/:*");
        // Several parameters in one path all fold onto the same wildcard.
        assert_eq!(param_wildcard("/a/:x/b/:y"), "/a/:*/b/:*");
        assert_eq!(param_wildcard("/a/b"), "/a/b");
        assert_eq!(param_wildcard(""), "");
    }

    /// `short_name` recognises exactly one more separator than `strip_namespace`: the dot.
    #[test]
    fn strip_namespace_and_short_name_differ_by_the_dot() {
        assert_eq!(strip_namespace("app\\job\\OrderJob"), "OrderJob");
        assert_eq!(
            strip_namespace("app.tasks.send_email"),
            "app.tasks.send_email",
            "strip_namespace deliberately does not split on `.` (that would cut Java package names apart)"
        );
        assert_eq!(short_name("app.tasks.send_email"), "send_email");
        assert_eq!(short_name("app\\service\\OrderService"), "OrderService");
        assert_eq!(short_name("send_email"), "send_email");
    }

    #[test]
    fn to_snake_handles_acronyms_digits_and_underscores() {
        // A run of capitals only breaks before the last one that starts a lower-case run.
        assert_eq!(to_snake("HTTPServer"), "http_server");
        assert_eq!(to_snake("Order2Item"), "order2_item");
        // Leading / trailing underscores are trimmed.
        assert_eq!(to_snake("_SaveInfo_"), "save_info");
        assert_eq!(to_snake("_"), "");
        // Already snake-case is unchanged, and so is the empty input.
        assert_eq!(to_snake("store_order"), "store_order");
        assert_eq!(to_snake(""), "");
        // A digit before a capital does not produce a double underscore.
        assert_eq!(to_snake("V2Order"), "v2_order");
    }

    #[test]
    fn pluralize_and_singularize_cover_the_common_forms() {
        assert_eq!(pluralize("store_order"), "store_orders");
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("box"), "boxes");
        assert_eq!(pluralize("class"), "classes");
        assert_eq!(pluralize("dish"), "dishes");
        assert_eq!(pluralize(""), "");
        // A one-character `y` has no preceding character: it must not underflow (regression).
        assert_eq!(pluralize("y"), "ies");

        assert_eq!(singularize("categories"), "category");
        assert_eq!(singularize("classes"), "class");
        assert_eq!(singularize("users"), "user");
        // `ss` is not a plural ending, and a bare `s` must not be emptied.
        assert_eq!(singularize("address"), "address");
        assert_eq!(singularize("s"), "s");

        // Round-trip: pluralize -> singularize returns the original for the forms the rules cover.
        for w in ["store_order", "category", "user", "class"] {
            assert_eq!(singularize(&pluralize(w)), w, "{w} must round-trip");
        }
    }

    /// `pluralize` used to only ever **add** an ending, so an already-plural Model class name degenerated
    /// (`UserRights` -> `user_rights` -> `user_rightses`). It must be idempotent.
    #[test]
    fn pluralize_is_idempotent() {
        for w in ["store_orders", "categories", "boxes", "dishes", "quizzes", "classes"] {
            assert_eq!(pluralize(w), w, "{w} is already plural, so no suffix must be added");
            assert_eq!(pluralize(&pluralize(w)), pluralize(w), "{w} must be idempotent");
        }
        // ...while genuinely singular words still get pluralised.
        assert_eq!(pluralize("store_order"), "store_orders");
        assert_eq!(pluralize("box"), "boxes");
        // `bus` / `status` end in `s` but are singular — they are not mistaken for plurals.
        assert_eq!(pluralize("bus"), "buses");
        assert_eq!(pluralize("status"), "statuses");
        assert_eq!(pluralize("address"), "addresses");
    }

    /// `singularize` mirrors `pluralize`: every `-es` ending the latter produces must be recognised.
    #[test]
    fn singularize_recognises_every_es_ending() {
        assert_eq!(singularize("boxes"), "box");
        assert_eq!(singularize("dishes"), "dish");
        assert_eq!(singularize("quizzes"), "quiz");
        assert_eq!(singularize("statuses"), "status");
        assert_eq!(singularize("classes"), "class");
        assert_eq!(singularize("categories"), "category");
        // ...and a plain `-s` ending still falls through to the generic rule.
        assert_eq!(singularize("store_orders"), "store_order");

        // Round-trip now holds for every form the rules cover.
        for w in ["store_order", "category", "user", "class", "box", "dish", "quiz", "status"] {
            assert_eq!(singularize(&pluralize(w)), w, "{w} must round-trip");
        }
    }

    /// A final `z` doubles when a vowel precedes it and stays single when a consonant does — the rule that
    /// separates `quiz` from `waltz`.
    #[test]
    fn pluralize_doubles_a_final_z_only_after_a_vowel() {
        assert_eq!(pluralize("quiz"), "quizzes");
        assert_eq!(pluralize("fez"), "fezzes");
        // A consonant before the `z`: no doubling.
        assert_eq!(pluralize("waltz"), "waltzes");
        assert_eq!(pluralize("quartz"), "quartzes");
        // The other sibilants never double.
        assert_eq!(pluralize("box"), "boxes");
        assert_eq!(pluralize("bus"), "buses");
        assert_eq!(pluralize("dish"), "dishes");
        // ...and a lone `z` has nothing before it to double.
        assert_eq!(pluralize("z"), "zes");
        // Singularising undoes the doubling exactly.
        assert_eq!(singularize("quizzes"), "quiz");
        assert_eq!(singularize("fezzes"), "fez");
        assert_eq!(singularize("waltzes"), "waltz");
        for w in ["quiz", "fez", "waltz", "quartz"] {
            assert_eq!(singularize(&pluralize(w)), w, "{w} must round-trip");
        }
    }

    #[test]
    fn snake_plural_and_class_to_topic_follow_their_conventions() {
        assert_eq!(snake_plural("Category"), "categories");
        assert_eq!(snake_plural("app\\model\\Box"), "boxes");
        // `class_to_topic` snake-cases but never pluralises.
        assert_eq!(class_to_topic("app\\job\\OrderJob"), "order_job");
        assert_eq!(class_to_topic("app\\job\\OrderJobs"), "order_jobs");
    }

    /// `apply_normalize` still has two unexercised `NormalizeStep` arms: `Upper` and `ShortName`. `Upper` is
    /// the case-insensitive route fold; `ShortName` collapses a fully-qualified name to its last (dot-split)
    /// segment — distinct from `StripNamespace`, which refuses to split on `.` (so it would not merge a Java
    /// package). Both must run through `apply_normalize` itself, not just the standalone helper.
    #[test]
    fn apply_normalize_covers_upper_and_short_name() {
        // `Upper` is the last-resort case-insensitive fold.
        assert_eq!(apply_normalize("api/v1", &[NormalizeStep::Upper]), "API/V1");
        // Declared order wins: `Upper` following `Lower` overrides it.
        assert_eq!(
            apply_normalize("Ab", &[NormalizeStep::Lower, NormalizeStep::Upper]),
            "AB"
        );

        // `ShortName` splits on `.` — a Java / Python FQN collapses to its leaf, which is what merges a
        // restored FQN with the short name used at registration time.
        assert_eq!(
            apply_normalize("app.tasks.send_email", &[NormalizeStep::ShortName]),
            "send_email"
        );
        // Combined with `Lower` to show the leaf is normalised too.
        assert_eq!(
            apply_normalize("app.tasks.SendOrder", &[NormalizeStep::ShortName, NormalizeStep::Lower]),
            "sendorder"
        );
    }

    /// `apply_transform` has a **fixed** order (not the declaration order of the YAML): namespace, snake,
    /// snake-plural, class-to-topic, lower, upper.
    #[test]
    fn apply_transform_runs_a_fixed_order() {
        assert_eq!(apply_transform("app\\Admin\\Store", &TransformSpec::default()), "app\\Admin\\Store");
        assert_eq!(
            apply_transform(
                "app\\Admin\\Store",
                &TransformSpec { strip_namespace: Some(true), snake: Some(true), ..Default::default() }
            ),
            "store"
        );
        assert_eq!(
            apply_transform("StoreOrder", &TransformSpec { snake_plural: Some(true), ..Default::default() }),
            "store_orders"
        );
        assert_eq!(
            apply_transform("app\\job\\OrderJob", &TransformSpec { class_to_topic: Some(true), ..Default::default() }),
            "order_job"
        );
        // `upper` runs last, so it wins over `lower`.
        assert_eq!(
            apply_transform("Ab", &TransformSpec { lower: Some(true), upper: Some(true), ..Default::default() }),
            "AB"
        );
        // The full chain, in the fixed order: strip -> snake -> snake_plural -> topic -> lower -> upper.
        assert_eq!(
            apply_transform(
                "app\\job\\OrderJob",
                &TransformSpec {
                    strip_namespace: Some(true),
                    snake: Some(true),
                    snake_plural: Some(true),
                    class_to_topic: Some(true),
                    lower: Some(true),
                    upper: Some(true),
                }
            ),
            "ORDER_JOBS"
        );
    }
}
