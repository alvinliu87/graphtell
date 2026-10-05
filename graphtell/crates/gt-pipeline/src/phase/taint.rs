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
    use crate::context::PipelineContext;
    use crate::workspace::CallRecord;
    use gt_domain::model::syntax::VariableAssignFact;
    use gt_domain::model::{
        Language, Project, ProjectConfig, ProjectId, ProjectStatus, Span, TaintSink,
    };
    use std::collections::HashMap;

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

    // ===== Below: coverage for `run()`'s backbone — backward trace, arg marker, keyword, spec resolution =====

    fn taint_spec() -> TaintSpec {
        TaintSpec {
            raw_sql_sinks: vec![TaintSink {
                method: "query".into(),
                receiver: Some("Db".into()),
            }],
            where_interp_sinks: vec!["where".into()],
            request_sources: vec!["input(".into(), "request()".into(), "->param(".into()],
        }
    }

    /// Backward tracing along the same-function assignment chain.
    #[test]
    fn reaches_request_follows_assignment_chain() {
        let spec = taint_spec();
        let p = php_prefixes();
        let mut idx: HashMap<(&str, &str), Vec<&str>> = HashMap::new();

        // Direct request source.
        idx.insert(("f", "id"), vec!["input('id')"]);
        assert!(reaches_request(&spec, &idx, "f", "id", &p));

        // A config read is NOT a request source — conservative miss, not a false critical.
        idx.insert(("f", "cfg"), vec!["Config::get('x')"]);
        assert!(!reaches_request(&spec, &idx, "f", "cfg", &p));

        // Transitive: `$b = $a; $a = input('x')`.
        idx.insert(("f", "a"), vec!["input('x')"]);
        idx.insert(("f", "b"), vec!["$a"]);
        assert!(reaches_request(&spec, &idx, "f", "b", &p));

        // A parameter (no assignment record) is treated as "no request source seen".
        assert!(!reaches_request(&spec, &HashMap::new(), "f", "param", &p));
    }

    #[test]
    fn reaches_request_breaks_cycles_and_respects_depth() {
        let spec = taint_spec();
        let p = php_prefixes();
        let mut idx: HashMap<(&str, &str), Vec<&str>> = HashMap::new();

        // Cycle `$a = $b; $b = $a` must terminate (no infinite loop) and report false.
        idx.insert(("f", "a"), vec!["$b"]);
        idx.insert(("f", "b"), vec!["$a"]);
        assert!(!reaches_request(&spec, &idx, "f", "a", &p));

        // `$this` is the object itself, never user input — must not be traced as a variable.
        idx.insert(("f", "x"), vec!["$this->db"]);
        assert!(!reaches_request(&spec, &idx, "f", "x", &p));

        // Depth guard: a chain longer than TRACE_DEPTH(4) hops is not fully followed.
        idx.insert(("f", "v0"), vec!["input('x')"]);
        idx.insert(("f", "v1"), vec!["$v0"]);
        idx.insert(("f", "v2"), vec!["$v1"]);
        idx.insert(("f", "v3"), vec!["$v2"]);
        idx.insert(("f", "v4"), vec!["$v3"]);
        idx.insert(("f", "v5"), vec!["$v4"]);
        assert!(!reaches_request(&spec, &idx, "f", "v5", &p));
    }

    /// `arg_has_user_var` distinguishes an interpolated / bare variable from a namespace separator or plain SQL.
    #[test]
    fn arg_has_user_var_marks_variables_not_separators() {
        let p = php_prefixes();
        assert!(arg_has_user_var(&FactValue::String("a ${x} b".into()), &p));
        assert!(arg_has_user_var(&FactValue::Unknown(Some("$x".into())), &p));
        // `Db::` namespace separator (no `{`) must not be mistaken for a variable.
        assert!(!arg_has_user_var(&FactValue::String("Db::query".into()), &p));
        // A plain SQL literal carries no variable.
        assert!(!arg_has_user_var(&FactValue::Unknown(Some("SELECT 1".into())), &p));
        assert!(!arg_has_user_var(&FactValue::String("SELECT 1".into()), &p));
        assert!(!arg_has_user_var(&FactValue::Int(1), &p));
    }

    /// Whole-word SQL keyword after stripping variable references (the BaseDao `$where` false-positive fix).
    #[test]
    fn contains_sql_keyword_is_whole_word_after_stripping() {
        let p = php_prefixes();
        assert!(contains_sql_keyword("WHERE x = 1", &p));
        // A variable name merely containing a keyword must not match.
        assert!(!contains_sql_keyword("$where", &p));
        // The real keyword survives once the variable name is stripped.
        assert!(contains_sql_keyword("$orderBy . ' LIMIT 1'", &p));
        // Substring inside another word must not match.
        assert!(!contains_sql_keyword("WHEREAS something", &p));
        assert!(!contains_sql_keyword("$foo . $bar", &p));
    }

    /// `spec_for` prefers a sub-project's own taint vocabulary, then the global default, then none.
    #[test]
    fn spec_for_prefers_sub_then_default_then_none() {
        let mut ctx = tctx();
        let sub_spec = TaintSpec {
            raw_sql_sinks: vec![TaintSink {
                method: "custom".into(),
                receiver: None,
            }],
            ..Default::default()
        };
        let default_spec = TaintSpec {
            raw_sql_sinks: vec![TaintSink {
                method: "query".into(),
                receiver: Some("Db".into()),
            }],
            ..Default::default()
        };
        ctx.taint.insert(7, sub_spec);
        ctx.taint_default = Some(default_spec);

        assert_eq!(
            spec_for(&ctx, Some(SubProjectId::new(7)))
                .unwrap()
                .raw_sql_sinks[0]
                .method,
            "custom"
        );
        // Missing sub falls back to the default.
        assert_eq!(
            spec_for(&ctx, Some(SubProjectId::new(99)))
                .unwrap()
                .raw_sql_sinks[0]
                .method,
            "query"
        );
        // `None` sub also uses the default.
        assert_eq!(
            spec_for(&ctx, None).unwrap().raw_sql_sinks[0].method,
            "query"
        );

        // Both missing -> the phase judges nothing.
        let empty = tctx();
        assert!(spec_for(&empty, Some(SubProjectId::new(5))).is_none());
        assert!(spec_for(&empty, None).is_none());
    }

    /// Minimal `PipelineContext` for the `spec_for` unit test.
    fn tctx() -> PipelineContext {
        PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: std::path::PathBuf::from("/t"),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        })
    }

    // ===== `run()` end-to-end: the decision backbone itself, previously uncovered =====

    /// A context wired the way a real PHP run is: FKB-declared vocabulary + the parser-declared `$` marker.
    fn php_ctx() -> PipelineContext {
        let mut ctx = tctx();
        ctx.taint_default = Some(taint_spec());
        ctx.lang_policy_default.variable_prefixes = php_prefixes();
        ctx
    }

    /// Register a call site on node `at`, split into receiver / method as the parser would.
    fn push_call(
        ctx: &mut PipelineContext,
        at: i64,
        owner_fqn: &str,
        callee: &str,
        receiver: Option<&str>,
        method: &str,
        args: Vec<FactValue>,
        sub: Option<i64>,
    ) {
        ctx.ws.calls.push(CallRecord {
            node: NodeId(at),
            owner: NodeId(at),
            owner_fqn: owner_fqn.to_string(),
            owner_class: None,
            callee: callee.to_string(),
            receiver: receiver.map(str::to_string),
            method: Some(method.to_string()),
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span::default(),
            file: "app/Svc.php".to_string(),
            sub: sub.map(SubProjectId::new),
            language: Language::new(Language::PHP),
        });
    }

    /// One assignment fact: the link backward tracing walks.
    fn assign(ctx: &mut PipelineContext, owner_fqn: &str, var: &str, rhs: &str) {
        ctx.ws.variable_assignments.push(VariableAssignFact {
            var: var.to_string(),
            rhs: rhs.to_string(),
            owner_fqn: owner_fqn.to_string(),
            file: "app/Svc.php".to_string(),
            span: Span::default(),
        });
    }

    fn kinds(ctx: &PipelineContext, at: i64) -> Vec<String> {
        ctx.ws
            .annotations_of(NodeId(at))
            .iter()
            .map(|a| a.kind.clone())
            .collect()
    }

    /// A raw-SQL sink whose variable really comes from the request: the confirmed (`tainted_raw`) path,
    /// with the full annotation shape downstream rules read.
    #[test]
    fn run_confirms_raw_sql_traced_to_a_request_source() {
        let mut ctx = php_ctx();
        assign(
            &mut ctx,
            "App\\Svc::run",
            "table",
            "request()->param('table')",
        );
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![FactValue::Unknown(Some(
                "SHOW COLUMNS FROM `{$table}`".into(),
            ))],
            None,
        );

        run(&mut ctx);

        assert_eq!(kinds(&ctx, 1), vec!["tainted_raw".to_string()]);
        let ann = &ctx.ws.annotations_of(NodeId(1))[0];
        assert_eq!(ann.channel, AnnotationChannel("Taint".to_string()));
        assert_eq!(ann.subkind.as_deref(), Some("SqlInjection"));
        assert_eq!(ann.phase, Phase("Taint".to_string()));
        assert!(
            (ann.confidence - 0.9).abs() < f32::EPSILON,
            "置信度应为 0.9"
        );
        assert_eq!(ann.evidence["callee"], "Db::query");
        assert_eq!(ann.evidence["sql"], "SHOW COLUMNS FROM `{$table}`");
    }

    /// A bare variable (`Db::query($sql)`) needs no embedding for a raw sink, and is confirmed once its
    /// assignment traces back to input.
    #[test]
    fn run_confirms_bare_variable_when_its_assignment_comes_from_request() {
        let mut ctx = php_ctx();
        assign(&mut ctx, "App\\Svc::run", "sql", "input('sql')");
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![FactValue::Unknown(Some("$sql".into()))],
            None,
        );

        run(&mut ctx);

        assert_eq!(kinds(&ctx, 1), vec!["tainted_raw".to_string()]);
    }

    /// The measured CRMEB case: `$execSql` comes from a local file shipped with the release package, so it must
    /// be demoted to `..._unknown` rather than reported as a confirmed critical.
    #[test]
    fn run_demotes_raw_sql_of_unproven_source_to_unknown() {
        let mut ctx = php_ctx();
        assign(
            &mut ctx,
            "App\\Svc::run",
            "sql",
            "file_get_contents(dirname(__DIR__) . '/sql/install.sql')",
        );
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![FactValue::Unknown(Some("$sql".into()))],
            None,
        );

        run(&mut ctx);

        assert_eq!(
            kinds(&ctx, 1),
            vec!["tainted_raw_unknown".to_string()],
            "来源不可追溯时只能降级为 unknown，不能当作已确认的 critical"
        );
    }

    /// The where family: only "a variable embedded in the condition string" is judged; a bare variable is left
    /// to Tier-2 instead of being false-positived here. Both branches of the same sink in one call to `run`.
    #[test]
    fn run_flags_embedded_where_condition_and_skips_bare_variable() {
        let mut ctx = php_ctx();
        assign(&mut ctx, "App\\Svc::run", "roles", "input('roles')");
        // Embedded, and its source is the request -> confirmed.
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "where",
            None,
            "where",
            vec![FactValue::Unknown(Some(
                "CONCAT(',',roles,',') LIKE '%,$roles,%'".into(),
            ))],
            None,
        );
        // Embedded, but built from config -> demoted.
        assign(&mut ctx, "App\\Svc::other", "kw", "Config::get('kw')");
        push_call(
            &mut ctx,
            2,
            "App\\Svc::other",
            "where",
            None,
            "where",
            vec![FactValue::Unknown(Some("name LIKE '%$kw%'".into()))],
            None,
        );
        // A bare variable is not "embedded": no annotation at all.
        assign(&mut ctx, "App\\Svc::third", "cond", "input('cond')");
        push_call(
            &mut ctx,
            3,
            "App\\Svc::third",
            "where",
            None,
            "where",
            vec![FactValue::Unknown(Some("$cond".into()))],
            None,
        );

        run(&mut ctx);

        assert_eq!(kinds(&ctx, 1), vec!["tainted_where".to_string()]);
        assert_eq!(kinds(&ctx, 2), vec!["tainted_where_unknown".to_string()]);
        assert!(
            kinds(&ctx, 3).is_empty(),
            "裸变量交给 Tier-2，本阶段不误报"
        );
    }

    /// The two "cannot judge" gates: no declared vocabulary, and no declared variable marker. Both must stay
    /// silent instead of borrowing another stack's knowledge.
    #[test]
    fn run_judges_nothing_without_declared_vocabulary_or_marker() {
        let mut ctx = php_ctx();
        assign(&mut ctx, "App\\Svc::run", "table", "input('table')");
        // A stack that declares no SQL vocabulary at all.
        ctx.taint_default = None;
        // ...and one whose parser declares no variable marker.
        ctx.lang_policy_default.variable_prefixes.clear();
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![FactValue::Unknown(Some(
                "SHOW COLUMNS FROM `{$table}`".into(),
            ))],
            None,
        );

        run(&mut ctx);

        assert!(
            kinds(&ctx, 1).is_empty(),
            "没有 sink 词汇 / 变量标记时不应判断，更不应借用别的栈的词汇"
        );
    }

    /// Non-sink callees and argless sink calls must never be annotated.
    #[test]
    fn run_ignores_non_sink_calls_and_sinks_without_arguments() {
        let mut ctx = php_ctx();
        assign(&mut ctx, "App\\Svc::run", "table", "input('table')");
        // `Db::name` is a declared sink's sibling, not a sink itself.
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "Db::name",
            Some("Db"),
            "name",
            vec![FactValue::Unknown(Some("$table".into()))],
            None,
        );
        // A declared sink called with no argument at all.
        push_call(
            &mut ctx,
            2,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![],
            None,
        );
        // A declared sink whose argument carries no variable reference.
        push_call(
            &mut ctx,
            3,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![FactValue::Unknown(Some("SELECT * FROM user".into()))],
            None,
        );

        run(&mut ctx);

        assert!(kinds(&ctx, 1).is_empty(), "非 sink 调用不应标注");
        assert!(kinds(&ctx, 2).is_empty(), "无参数的 sink 不应标注");
        assert!(kinds(&ctx, 3).is_empty(), "参数中没有变量就不是注入");
    }

    /// A sub-project's own vocabulary **replaces** the global default (it is not merged with it), so a stack
    /// that declares `execute` never inherits the default's `Db::query`.
    #[test]
    fn run_uses_the_sub_projects_own_vocabulary_not_the_default() {
        let mut ctx = php_ctx();
        ctx.taint.insert(
            7,
            TaintSpec {
                raw_sql_sinks: vec![TaintSink {
                    method: "execute".into(),
                    receiver: None,
                }],
                request_sources: vec!["input(".into()],
                ..Default::default()
            },
        );
        assign(&mut ctx, "App\\Svc::run", "sql", "input('sql')");
        // sub 7 declares `execute`.
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "Db::execute",
            Some("Db"),
            "execute",
            vec![FactValue::Unknown(Some("$sql".into()))],
            Some(7),
        );
        // `Db::query` only exists in the global default, which this sub does not fall back to.
        push_call(
            &mut ctx,
            2,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![FactValue::Unknown(Some("$sql".into()))],
            Some(7),
        );

        run(&mut ctx);

        assert_eq!(kinds(&ctx, 1), vec!["tainted_raw".to_string()]);
        assert!(
            kinds(&ctx, 2).is_empty(),
            "子项目有自己的词汇时不应再回落到默认词汇"
        );
    }

    /// Documented fallback: an argument that clearly interpolates something but yields **no resolvable variable
    /// name** (`"${x}"`) leaves backward tracing nothing to walk, so the pre-P10 criterion is kept — flagged,
    /// not demoted to `..._unknown`.
    #[test]
    fn run_keeps_the_original_criterion_when_no_variable_name_resolves() {
        let mut ctx = php_ctx();
        push_call(
            &mut ctx,
            1,
            "App\\Svc::run",
            "Db::query",
            Some("Db"),
            "query",
            vec![FactValue::String("DROP TABLE ${x}".into())],
            None,
        );

        run(&mut ctx);

        assert_eq!(kinds(&ctx, 1), vec!["tainted_raw".to_string()]);
    }

    // ===== remaining pure leaves =====

    /// `var_names_in` feeds the whole backward trace: it must resolve names without the marker, de-duplicate,
    /// and never treat `$this` as user input.
    #[test]
    fn var_names_in_resolves_dedups_and_skips_this() {
        let p = php_prefixes();
        assert_eq!(var_names_in("$sql", &p), vec!["sql"]);
        assert_eq!(
            var_names_in("FROM `{$table}` WHERE x = {$field}", &p),
            vec!["table", "field"]
        );
        assert_eq!(
            var_names_in("$a . $b . $a", &p),
            vec!["a", "b"],
            "同一变量只应出现一次"
        );
        assert!(
            var_names_in("$this->alias . '.uid'", &p).is_empty(),
            "$this 是对象自身，不是用户输入"
        );
        assert!(var_names_in("no variables here", &p).is_empty());
        // The `${x}` form yields no resolvable name — that is the fallback exercised above.
        assert!(var_names_in("${x}", &p).is_empty());
    }

    /// `arg_text` is what lands in the annotation's evidence: only textual facts carry SQL text.
    #[test]
    fn arg_text_reads_string_and_unknown_only() {
        assert_eq!(arg_text(&FactValue::String("SELECT 1".into())), "SELECT 1");
        assert_eq!(arg_text(&FactValue::Unknown(Some("$x".into()))), "$x");
        assert_eq!(arg_text(&FactValue::Unknown(None)), "");
        assert_eq!(arg_text(&FactValue::Int(1)), "");
    }
}
