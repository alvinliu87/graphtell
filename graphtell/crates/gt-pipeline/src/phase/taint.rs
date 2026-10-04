//! P9 Taint: detect, at the **call-site** level, SQL-injection where "user data flows into a SQL string".
//!
//! # Motivation
//!
//! FKB already tags `Db::query` / `whereRaw` / `Db::raw` call sites with a `Taint(sink)` annotation in P4,
//! but that is only the fact "a dangerous sink exists" — it does not judge whether the argument really comes from user input, so
//! `raw-sql-sink` can only emit a `warning`, worded "needs confirmation", producing no blocking-level signal.
//!
//! A real injection needs only one **local** fact: the SQL-string argument of a dangerous SQL call directly interpolates
//! a variable from the request / outside. That fact is already preserved at parse time — the `parser` stores an interpolated
//! string as `FactValue::Unknown(Some(original text))` (the interpolation source of `encapsed_string`), and string concatenation
//! as `Unknown(Some(the whole concatenated text))`. So we do **not** need inter-procedural taint analysis; we only read the
//! argument text at the call site, and it is framework-agnostic (only the callee name + argument interpolation matters).
//!
//! # Forms of injection covered
//!
//! * Raw SQL execution: `Db::query("...{$t}...")` / `Db::execute(...)` / `Db::raw(...)` /
//!   `->whereRaw("...$v...")` / `->whereExp(...)` — argument 0 is the SQL text, an interpolated variable is the injection.
//!   A bare variable (e.g. `Db::query($sql)`, concatenated elsewhere) is also rated high-risk.
//! * where-condition interpolation: `->where("CONCAT(',',roles,',') LIKE '%,$roles,%'")` — a write that crams a variable
//!   straight into the condition string. Argument 0 being a string containing interpolation is the injection (bare variables are left to Tier-2).
//!
//! # P10 backward tracing: is the variable really from the request
//!
//! Judging only the call-site argument text ("does it contain `$var`") floods false positives: `Db::execute($execSql)`,
//! a whole-variable-pass-in write, hits every time, yet `$execSql` actually comes from a local file shipped with the version package
//! (measured: 34 of CRMEB's 40 `sql-injection-raw` cases were this kind).
//!
//! So here we trace backward along the assignment chain **within the same function**: variable -> its assignment RHS -> other
//! variables referenced on the RHS -> …; only when any link in the chain shows a request source (`request()->param()` / `input()` /
//! `$_GET` etc.) is it judged tainted. Scope is limited to the function, no inter-procedure (that is Tier-2's job).
//!
//! A source that cannot be traced (e.g. the variable is a function parameter) is treated as "no request source seen" — better a
//! false negative than an unexplainable critical; false negatives are explicitly exposed by `rules_silent` (ran but 0 hits).

use std::collections::{HashMap, HashSet};

use gt_domain::model::{
    AnnotationChannel, FactValue, MergeStrategy, NewAnnotation, NodeId, Phase, SubProjectId,
    TaintSpec,
};
use serde_json::json;

use crate::context::PipelineContext;

/// The taint vocabulary to judge a call site with: the sub-project's own, else the global fallback.
///
/// `None` means this stack declares nothing, so P9 judges nothing — rather than applying another stack's
/// ORM names (which is what the old hard-coded `language == php` gate did).
fn spec_for(ctx: &PipelineContext, sub: Option<SubProjectId>) -> Option<&TaintSpec> {
    sub.and_then(|s| ctx.taint.get(&s.get()))
        .or(ctx.taint_default.as_ref())
}

/// Raw SQL execution callees (argument 0 is the SQL text) — **declared by FKB**, not built in.
fn is_raw_sql_sink(spec: &TaintSpec, receiver: &Option<String>, method: &str) -> bool {
    spec.raw_sql_sinks.iter().any(|s| {
        s.method == method
            && match s.receiver.as_deref() {
                // A receiver given in the declaration must match exactly (ThinkPHP's `Db::query`);
                // omitted = "raw SQL however it is reached" (`whereRaw` / `raw`).
                Some(r) => receiver.as_deref() == Some(r),
                None => true,
            }
    })
}

/// In the where family, "condition-string interpolation" is the injection form (whereLike uses parameterised escaping, skip it).
fn is_where_interp_sink(spec: &TaintSpec, method: &str) -> bool {
    spec.where_interp_sinks.iter().any(|m| m == method)
}

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Taint".to_string());

    let mut raw_count = 0usize;
    let mut where_count = 0usize;
    // Count of "source unknown" (warning) besides the confirmed-from-request (critical).
    let mut raw_unknown_count = 0usize;
    let mut where_unknown_count = 0usize;

    // Assignment index: (function FQN, variable name) -> all assignment RHS source of that variable within this function.
    // Built up front like `ctx.ws.calls`, to avoid borrow conflicts in the loop.
    let mut index: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for a in &ctx.ws.variable_assignments {
        index
            .entry((a.owner_fqn.as_str(), a.var.as_str()))
            .or_default()
            .push(a.rhs.as_str());
    }

    // Collect call sites to annotate first, to avoid a conflict between `ctx.ws.calls`'s immutable borrow and `annotate`'s mutable borrow.
    let mut targets: Vec<(NodeId, String, String, String)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        // Which callees are SQL sinks, and which expressions read user input, is FKB knowledge — a stack
        // that declares none is skipped instead of being judged with another stack's vocabulary.
        let Some(spec) = spec_for(ctx, call.sub) else {
            continue;
        };
        // How this language writes a variable reference comes from its parser (PHP `$`, JS `${` …).
        // With no marker there is nothing to look for, so the phase cannot judge this call site.
        let prefixes = &ctx.lang_policy_for_sub(call.sub).variable_prefixes;
        if prefixes.is_empty() {
            continue;
        }
        let method = match &call.method {
            Some(m) => m.as_str(),
            None => continue,
        };
        let is_raw = is_raw_sql_sink(spec, &call.receiver, method);
        let is_where = is_where_interp_sink(spec, method);
        if !is_raw && !is_where {
            continue;
        }

        // The SQL string is usually argument 0.
        let sql_arg = match call.args.first() {
            Some(a) => a,
            None => continue,
        };
        if !arg_has_user_var(sql_arg, prefixes) {
            continue;
        }

        // The where family only recognises "a variable embedded in a string / SQL expression" (a bare variable like `->where($cond)`
        // needs Tier-2 inter-procedural argument-source tracing, this phase does not false-positive on it); raw SQL execution classes count bare variables as high-risk too.
        if is_where && !arg_is_embedded(sql_arg, prefixes) {
            continue;
        }

        let vars = var_names_in(&arg_text(sql_arg), prefixes);
        let proven = vars
            .iter()
            .any(|v| reaches_request(spec, &index, call.owner_fqn.as_str(), v, prefixes));
        // When the argument has no resolvable variable (e.g. forms other than `$this->alias . '.uid'`), keep the original criterion.
        let unknown = !vars.is_empty() && !proven;

        let (kind, counter) = if is_raw {
            if unknown {
                ("tainted_raw_unknown", &mut raw_unknown_count)
            } else {
                ("tainted_raw", &mut raw_count)
            }
        } else if unknown {
            ("tainted_where_unknown", &mut where_unknown_count)
        } else {
            ("tainted_where", &mut where_count)
        };
        *counter += 1;
        targets.push((
            call.node,
            kind.to_string(),
            call.callee.clone(),
            arg_text(sql_arg),
        ));
    }

    for (node_id, kind, callee, sql) in targets {
        ctx.ws.annotate(NewAnnotation {
            node_id,
            channel: AnnotationChannel("Taint".to_string()),
            kind,
            subkind: Some("SqlInjection".to_string()),
            confidence: 0.9,
            evidence: json!({
                "callee": callee,
                "sql": sql,
            }),
            phase: phase.clone(),
            merge: MergeStrategy::Coexist,
        });
    }

    tracing::info!(
        "P9 taint done: {} raw-SQL injections ({} of unknown source), {} where-condition-interpolation injections ({} of unknown source)",
        raw_count,
        raw_unknown_count,
        where_count,
        where_unknown_count
    );
}

/// The maximum depth of backward tracing (guards against cycles like `$a = $b; $b = $a;` and over-long chains).
const TRACE_DEPTH: u8 = 4;

/// Whether a variable (within the **same function**, along the assignment chain) ultimately comes from request input.
///
/// `request_sources` is declared by FKB (framework / superglobal parameter-taking forms). The list is
/// deliberately conservative, and `$request` counts too — it is a Request object when injected. Better a
/// miss (caught by `rules_silent`) than misjudging a config read as user input.
fn reaches_request(
    spec: &TaintSpec,
    index: &HashMap<(&str, &str), Vec<&str>>,
    owner_fqn: &str,
    var: &str,
    prefixes: &[String],
) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    trace_var(spec, index, owner_fqn, var, TRACE_DEPTH, &mut seen, prefixes)
}

fn trace_var(
    spec: &TaintSpec,
    index: &HashMap<(&str, &str), Vec<&str>>,
    owner_fqn: &str,
    var: &str,
    depth: u8,
    seen: &mut HashSet<String>,
    prefixes: &[String],
) -> bool {
    if depth == 0 || !seen.insert(var.to_string()) {
        return false;
    }
    let Some(rhss) = index.get(&(owner_fqn, var)) else {
        // No assignment record: a parameter / global / untraceable — treated as "no request source seen".
        return false;
    };
    for rhs in rhss {
        let lower = rhs.to_ascii_lowercase();
        if spec.request_sources.iter().any(|s| lower.contains(s)) {
            return true;
        }
        for next in var_names_in(rhs, prefixes) {
            if trace_var(spec, index, owner_fqn, &next, depth - 1, seen, prefixes) {
                return true;
            }
        }
    }
    false
}

/// Extract variable names from text (`$sql` / `{$sql}`), without the language's variable prefix, skipping `$this`.
fn var_names_in(text: &str, prefixes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let Some(p) = prefixes.iter().find(|p| text[i..].starts_with(p.as_str())) else {
            i += 1;
            continue;
        };
        let start = i + p.len();
        let rest = &text[start..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            let len = name.len();
            // `$this` is the object itself, never user input.
            if name != "this" && !out.iter().any(|n| *n == name) {
                out.push(name);
            }
            i = start + len;
            continue;
        }
        i += 1;
    }
    out
}

/// Whether the argument text contains a "user variable reference" (`$var` / `{$var}`).
///
/// Only obvious variable references are recognised, to avoid misjudging `$` appearing in other contexts (e.g. `Db::` namespace separator,
/// a literal inside `Env::get`).
fn arg_has_user_var(fv: &FactValue, prefixes: &[String]) -> bool {
    match fv {
        // A literal string that still carries an interpolation marker (`"${x}"` on PHP).
        FactValue::String(s) => prefixes.iter().any(|p| s.contains(&format!("{p}{{"))),
        FactValue::Unknown(Some(t)) => text_has_var(t, prefixes),
        _ => false,
    }
}

/// Whether a variable is "embedded in a string / SQL expression" (rather than a bare variable passed as its own argument).
fn arg_is_embedded(fv: &FactValue, prefixes: &[String]) -> bool {
    let t = match fv {
        FactValue::String(s) => s.as_str(),
        FactValue::Unknown(Some(t)) => t.as_str(),
        _ => return false,
    };
    text_has_var(t, prefixes)
        && (t.contains('\'') || t.contains('"') || contains_sql_keyword(t, prefixes))
}

/// Whether the text contains a "variable reference" (`$var` / `{$var}`); `$this` does not count.
///
/// `$this` is the object itself, never user input. Counting it would make a **parameterised** write like
/// `->where($this->alias . '.uid', $uid)` be judged as injection (the condition string does "have a variable", but that variable is a
/// property whose value is a bound parameter) — measured on CRMEB, this was the entire source of the 29 residual `sql-injection-where-interp` cases.
fn text_has_var(t: &str, prefixes: &[String]) -> bool {
    let mut i = 0;
    while i < t.len() {
        let Some(p) = prefixes.iter().find(|p| t[i..].starts_with(p.as_str())) else {
            i += 1;
            continue;
        };
        let after = &t[i + p.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            if name != "this" {
                return true;
            }
            i += p.len() + name.len();
            continue;
        }
        // The `{$var}` form.
        if after.starts_with('{') {
            return true;
        }
        i += 1;
    }
    false
}

fn contains_sql_keyword(t: &str, prefixes: &[String]) -> bool {
    const KW: &[&str] = &[
        "SELECT", "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "TRUNCATE", "REPLACE",
        "WHERE", "FROM", "SHOW", "SET", "INTO", "VALUES", "JOIN", "ORDER", "GROUP", "HAVING",
        "LIKE", "LIMIT", "CONCAT", "AND", "OR", "NOT", "IN", "BY",
    ];
    // Strip variable references (`$where`, `{$table}`) first: a variable name is just an identifier, not SQL syntax,
    // otherwise `$where` would be misjudged as embedding a WHERE clause because its name contains "where" (root cause of the BaseDao.php:580 false positive).
    let cleaned = strip_var_refs(t, prefixes);
    let up = cleaned.to_uppercase();
    // Keywords must appear as whole words (compare exactly after splitting on non-alphanumerics), to avoid substring false hits.
    KW.iter()
        .any(|k| up.split(|c: char| !c.is_ascii_alphabetic()).any(|w| w == *k))
}

/// Remove variable references (`$var` / `{$var}`) from text, replaced with a space placeholder.
fn strip_var_refs(t: &str, prefixes: &[String]) -> String {
    let mut out = String::with_capacity(t.len());
    let mut i = 0;
    let chars: Vec<char> = t.chars().collect();
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let Some(p) = prefixes.iter().find(|p| rest.starts_with(p.as_str())) else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        // Skip the variable name itself.
        let mut j = i + p.chars().count();
        while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        // Strip the whole `{$var}` interpolation when the prefix sat inside braces.
        if i > 0 && chars[i - 1] == '{' && j < chars.len() && chars[j] == '}' {
            j += 1;
            out.pop(); // drop the already-emitted `{`
        }
        out.push(' ');
        i = j;
    }
    out
}

fn arg_text(fv: &FactValue) -> String {
    match fv {
        FactValue::String(s) => s.clone(),
        FactValue::Unknown(Some(t)) => t.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variable marker is **this language's** (PHP `$`), injected like every other notation rule — not
    /// built into the scanner.
    fn php_prefixes() -> Vec<String> {
        vec!["$".to_string()]
    }

    #[test]
    fn detects_var_in_interpolated_string() {
        let p = php_prefixes();
        assert!(text_has_var("CONCAT(',',roles,',') LIKE '%,$roles,%'", &p));
        assert!(text_has_var(
            "SHOW FULL COLUMNS FROM `{$table}` WHERE Field = '{$field}'",
            &p
        ));
        assert!(text_has_var(
            "\"DROP TABLE `\" . $info->table_name . \"`",
            &p
        ));
        assert!(text_has_var("$findSql", &p));
    }

    #[test]
    fn ignores_namespace_and_literals() {
        let p = php_prefixes();
        assert!(!text_has_var("Db::query", &p));
        assert!(!text_has_var("Env::get('database.prefix')", &p));
        assert!(!text_has_var("status", &p));
    }

    /// A language that declares **no** variable marker yields nothing: the phase cannot guess one. This is
    /// what replaces the old `language == php` gate.
    #[test]
    fn no_declared_marker_finds_no_variable() {
        let p: Vec<String> = Vec::new();
        assert!(!text_has_var("CONCAT(',',roles,',') LIKE '%,$roles,%'", &p));
        assert!(var_names_in("$sql", &p).is_empty());
    }

    #[test]
    fn embedded_recognizes_string_but_not_bare_var() {
        let p = php_prefixes();
        let embedded = FactValue::Unknown(Some(
            "CONCAT(',',roles,',') LIKE '%,$roles,%'".to_string(),
        ));
        assert!(arg_is_embedded(&embedded, &p));
        let bare = FactValue::Unknown(Some("$cond".to_string()));
        assert!(!arg_is_embedded(&bare, &p));
    }

    /// Regression: the array-parameterised write `->where($where)` must not be misjudged because the variable name contains a SQL keyword
    /// (BaseDao.php:580, `$where` uppercased contains "WHERE").
    #[test]
    fn bare_var_named_like_keyword_is_not_embedded() {
        let p = php_prefixes();
        for name in ["$where", "$order", "$limit", "$group", "$values", "$map"] {
            let bare = FactValue::Unknown(Some(name.to_string()));
            assert!(
                !arg_is_embedded(&bare, &p),
                "{name} must not be judged as embedded SQL"
            );
        }
        // After the variable name is stripped, the real SQL fragment can still be matched by keyword / quote
        let kw_only_in_var = FactValue::Unknown(Some("$orderBy . ' LIMIT 1'".to_string()));
        assert!(arg_is_embedded(&kw_only_in_var, &p));
    }

    #[test]
    fn strip_var_refs_removes_vars_and_interpolation() {
        let p = php_prefixes();
        assert_eq!(strip_var_refs("$where", &p), " ");
        assert_eq!(strip_var_refs("{$table}", &p), " ");
        assert_eq!(strip_var_refs("a.$order.b", &p), "a. .b");
        assert_eq!(strip_var_refs("LIKE '%$kw%'", &p), "LIKE '% %'");
    }

    /// Sinks and request sources are declared data: an empty declaration (`TaintSpec::default()`) must not
    /// recognise ThinkPHP's `Db::query` — that is the whole point of moving the vocabulary to FKB.
    #[test]
    fn sinks_come_from_the_declaration_not_the_kernel() {
        let empty = TaintSpec::default();
        assert!(!is_raw_sql_sink(
            &empty,
            &Some("Db".to_string()),
            "query"
        ));
        assert!(!is_where_interp_sink(&empty, "whereRaw"));

        let php = TaintSpec {
            raw_sql_sinks: vec![
                gt_domain::model::TaintSink {
                    method: "query".into(),
                    receiver: Some("Db".into()),
                },
                gt_domain::model::TaintSink {
                    method: "whereRaw".into(),
                    receiver: None,
                },
            ],
            where_interp_sinks: vec!["where".into(), "whereOr".into()],
            request_sources: vec!["$_get".into(), "->param(".into()],
        };
        assert!(is_raw_sql_sink(&php, &Some("Db".to_string()), "query"));
        // A receiver-bound sink must not fire for another receiver: `->query()` is not `Db::query`.
        assert!(!is_raw_sql_sink(&php, &Some("Model".to_string()), "query"));
        // A sink declared without a receiver fires for any receiver.
        assert!(is_raw_sql_sink(&php, &Some("Model".to_string()), "whereRaw"));
        assert!(is_where_interp_sink(&php, "where"));
    }
}
