#![allow(unused_imports)]
use super::*;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use gt_domain::error::Result;
use gt_domain::model::{
    Action, Detector, FactValue, FrameworkKnowledge, GuardAttachSpec, KnowledgeScope, Language,
    NormalizeStep, Phase, RootSource, RouteCallSpec, RouteGuardSpec, RouteMatchBy, Rule,
    SubProjectId, SynthesizedKind,
};
use gt_domain::port::{
    AdapterFact, FileSystem, KnowledgeProvider, LanguageParser, ParserRegistry, TechStackRegistry,
};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::context::PipelineContext;
use crate::engine::{callee_matches, path_matches};
use crate::normalize::strip_prefixes;
use crate::phase::facts::{self, expand_provided, lock_has, manifest_has};
use crate::workspace::{CallRecord, RouteGroup, RouteGuard, RouteGuardScope};

pub(crate) struct ArraySyntax {
    /// Opening bracket char (e.g. `[` for PHP array syntax).
    open: char,
    close: char,
    /// Separates a key from its value (`=>` in PHP, `:` in JSON-ish syntaxes).
    pair_separator: String,
    /// Starts a line comment; empty means "no comment syntax declared".
    comment: String,
    /// Suffix marking a class reference (`::class` in PHP; empty where a bare identifier is used).
    class_suffix: String,
    /// This language's namespace separators, used to strip a leading one (`\App\X` -> `App\X`).
    separators: Vec<char>,
}

impl ArraySyntax {
    pub(crate) fn from_params(params: &Value, separators: &[char]) -> Option<Self> {
        let single = |k: &str| {
            params
                .get(k)
                .and_then(Value::as_str)
                .and_then(|s| s.chars().next())
        };
        let open = single("open")?;
        let close = single("close")?;
        let strs = |k: &str| {
            params
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let exts: Vec<String> = params
            .get("extensions")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default();
        if exts.is_empty() {
            return None;
        }
        Some(Self {
            open,
            close,
            pair_separator: strs("separator"),
            comment: strs("comment"),
            class_suffix: strs("class_suffix"),
            separators: separators.to_vec(),
        })
    }

    /// Index of the `close` that balances the `open` at index 0 of `s`.
    fn matching_close(&self, s: &str) -> Option<usize> {
        let mut depth = 0i32;
        for (i, b) in s.as_bytes().iter().enumerate() {
            if *b == self.open as u8 {
                depth += 1;
            } else if *b == self.close as u8 {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    }
}

/// Extract the middleware class-name list from file text.
///
/// * when `key` is given (e.g. `middleware`): take the `key => [ ... ]` array block;
///   note `key` may appear in a namespace (e.g. `app\...\middleware\Foo`), so require `=>` immediately after the key
///   (skip quotes / whitespace) to count as an assignment key, avoiding hitting a namespace.
/// * otherwise: take the first `[ ... ]` array of the whole file (i.e. `return [A::class, ...];` form).
/// Each line takes `X::class` (strip `//` comments, trailing comma, leading `\`), get the normalized FQN.
/// The **syntax of the declared array**, all of it stack knowledge (`[ … ]` / `=>` / `//` / `::class`
/// for PHP) and therefore declared by FKB — the kernel knows none of these characters.
pub(crate) fn extract_middleware_classes(text: &str, key: Option<&str>, syn: &ArraySyntax) -> Vec<String> {
    let block = match key {
        Some(k) => {
            // Find "<key><pair_separator><open>": iterate all hits, take the one with the separator
            // immediately after the key.
            let mut from = 0;
            let mut found: Option<usize> = None;
            while let Some(rel) = text[from..].find(k) {
                let abs = from + rel;
                let rest = &text[abs + k.len()..];
                let after = rest
                    .trim_start_matches(|c: char| c == '\'' || c == '"' || c.is_whitespace());
                if syn.pair_separator.is_empty() || after.starts_with(&syn.pair_separator) {
                    found = Some(abs);
                    break;
                }
                from = abs + k.len();
            }
            match found {
                Some(idx) => {
                    let after = &text[idx + k.len()..];
                    let after = after
                        .trim_start()
                        .strip_prefix(syn.pair_separator.as_str())
                        .unwrap_or(after);
                    match after.find(syn.open) {
                        Some(open) => {
                            let rest = &after[open..];
                            match syn.matching_close(rest) {
                                Some(close) => rest[1..close].to_string(),
                                None => return Vec::new(),
                            }
                        }
                        None => return Vec::new(),
                    }
                }
                None => return Vec::new(),
            }
        }
        None => match text.find(syn.open) {
            Some(open) => match syn.matching_close(&text[open..]) {
                Some(close) => text[open + 1..open + close].to_string(),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        },
    };
    let mut out = Vec::new();
    for line in block.lines() {
        let line = if syn.comment.is_empty() {
            line
        } else {
            line.split(&syn.comment).next().unwrap_or(line)
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A `key <separator> value` entry: only the value side names a class (a bare array entry has
        // no separator and is taken whole).
        let value = match line.split_once(&syn.pair_separator) {
            Some((_, v)) if !syn.pair_separator.is_empty() => v.trim(),
            _ => line,
        };
        let value = value
            .trim_end_matches(',')
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .trim();
        if value.is_empty() {
            continue;
        }
        let c = if syn.class_suffix.is_empty() {
            value
        } else {
            match value.strip_suffix(&syn.class_suffix) {
                Some(c) => c,
                None => continue,
            }
        };
        let c = c.trim().trim_start_matches(|c| syn.separators.contains(&c)).trim();
        if !c.is_empty() {
            out.push(c.to_string());
        }
    }
    out
}

/// Path suffix match (align at end), supports `*` wildcard segment (matches exactly one path segment).
///
/// For `declared_middleware`'s `paths`: declaring `{app_root}/*/config/route.php` hits
/// `server/{app_root}/adminapi/config/route.php` (no matter how many dir layers under project_root), unlike `engine::path_matches`
/// which requires the pattern to match from the start — auto-route projects' app dirs often hide in `server/app/<app>` subdirs.
/// Without `*` degenerates to plain suffix equality.
pub(crate) fn declared_mw_path_matches(pattern: &str, path: &str) -> bool {
    let pseg: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let aseg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if aseg.len() < pseg.len() {
        return false;
    }
    let suffix = &aseg[aseg.len() - pseg.len()..];
    pseg.iter()
        .zip(suffix)
        .all(|(p, a)| *p == "*" || p == a)
}

/// Scan text files of the given extension (skip dependency dirs).
pub(crate) fn scan_text_files(root: &Path, exts: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                "vendor" | "node_modules" | "target" | ".git" | "dist" | "build" | "runtime"
            ) {
                walker.skip_current_dir();
            }
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !exts.contains(&ext.as_str()) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, text));
        }
        if out.len() > 400 {
            break;
        }
    }
    out
}

/// Parse `CREATE TABLE`: take table name and column names (by paren pairing, avoid truncation by `int(11)`).
pub(crate) fn parse_create_tables(sql: &str) -> Vec<(String, Vec<String>)> {
    let re = regex::Regex::new(
        r#"(?is)CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?[`"]?([\w.]+)[`"]?\s*\("#,
    )
    .unwrap();
    let mut out = Vec::new();
    for m in re.find_iter(sql) {
        let caps = re.captures(&sql[m.start()..]).unwrap();
        let table = match caps.get(1) {
            Some(t) => t.as_str().trim_matches(|c| c == '`' || c == '"').to_string(),
            None => continue,
        };
        // Paren pairing from `(`
        let bytes = sql.as_bytes();
        let open = m.start() + m.as_str().len() - 1;
        let mut depth = 0i32;
        let mut close = None;
        let mut i = open;
        let mut in_quote: Option<u8> = None;
        while i < bytes.len() {
            let b = bytes[i];
            match in_quote {
                Some(q) => {
                    if b == q {
                        in_quote = None;
                    }
                }
                None => match b {
                    b'`' | b'"' | b'\'' => in_quote = Some(b),
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                },
            }
            i += 1;
        }
        let Some(close) = close else { continue };
        let body = &sql[open + 1..close];
        out.push((table, split_columns(body)));
    }
    out
}

/// Split the first-level commas of a `CREATE TABLE` field list.
pub(crate) fn split_columns(body: &str) -> Vec<String> {
    let mut columns = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut in_quote: Option<u8> = None;
    for b in body.bytes() {
        match in_quote {
            Some(q) => {
                current.push(b as char);
                if b == q {
                    in_quote = None;
                }
            }
            None => match b {
                b'`' | b'"' | b'\'' => {
                    current.push(b as char);
                    in_quote = Some(b);
                }
                b'(' => {
                    depth += 1;
                    current.push('(');
                }
                b')' => {
                    depth -= 1;
                    current.push(')');
                }
                b',' if depth == 0 => {
                    push_column(&mut columns, &current);
                    current.clear();
                }
                _ => current.push(b as char),
            },
        }
    }
    push_column(&mut columns, &current);
    columns
}

pub(crate) fn push_column(out: &mut Vec<String>, raw: &str) {
    // A `--` / `#` comment carries no comma, so it lands in the **same segment** as the field that follows
    // it. Judging the segment as a whole would skip that real field along with the comment, so the
    // definition is taken from the first line that actually is one.
    let line = raw
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with("--") && !l.starts_with('#'))
        .unwrap_or("")
        .trim_end_matches(',');
    if line.is_empty() {
        return;
    }
    let upper = line.to_ascii_uppercase();
    // Constraint lines are not columns — but only when the keyword ends at a **word boundary**:
    // `unique_id` / `check_time` / `index_sort` / `foreign_id` are ordinary column names that merely start
    // with a keyword, and dropping them costs the column its `Column` node (hence any PII annotation).
    let is_constraint = |kw: &str| {
        upper.starts_with(kw)
            && upper[kw.len()..]
                .chars()
                .next()
                .map(|c| !c.is_alphanumeric() && c != '_')
                .unwrap_or(true)
    };
    for kw in [
        "PRIMARY KEY",
        "KEY ",
        "UNIQUE",
        "INDEX",
        "CONSTRAINT",
        "FOREIGN",
        "FULLTEXT",
        "CHECK",
        "SPATIAL",
    ] {
        if is_constraint(kw) {
            return;
        }
    }
    if let Some(first) = line.split_whitespace().next() {
        let col = first.trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']');
        if !col.is_empty() && col.len() < 64 && col.chars().all(|c| c.is_alphanumeric() || c == '_') {
            out.push(col.to_string());
        }
    }
}

/// For P5 to judge whether a config-entry file matches (incl. `{app_root}` placeholder expansion).
pub fn expand(path: &str, app_root: &str) -> String {
    path.replace("{app_root}", app_root)
}

/// Resolve the project's app-root directory name, falling back to `app` when `root_rules` has not
/// produced an `app_root` fact (or it is not a string). Used to expand the `{app_root}` placeholder
/// in loader `path` / `paths` declarations — the app directory is a project decision, never hard-coded.
pub(crate) fn app_root_of(ctx: &PipelineContext, sub: &gt_domain::model::SubProject) -> String {
    ctx.ws
        .get_fact(sub.id, "app_root")
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("app")
        .to_string()
}

/// For test use.
pub fn _sub_id(id: SubProjectId) -> SubProjectId {
    id
}

/// Textualize a `FactValue`.
pub(crate) trait LossyText {
    fn to_string_lossy(&self) -> String;
}

impl LossyText for gt_domain::model::FactValue {
    fn to_string_lossy(&self) -> String {
        match self {
            gt_domain::model::FactValue::String(s) | gt_domain::model::FactValue::ClassConst(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }
}

/// Rule dedup (by id).
pub(crate) fn dedup_rules(rules: Vec<Rule>) -> Vec<Rule> {
    let mut out: Vec<Rule> = Vec::new();
    for r in rules {
        if !out.iter().any(|x| x.id == r.id) {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PipelineContext;
    use gt_domain::model::{
        FactValue, Language, Phase, Project, ProjectId, ProjectStatus, Rule, Selector, SubProject,
        SubProjectId,
    };
    use serde_json::{json, Value};
    use std::path::Path;

    // ------------------------------------------------------- ArraySyntax::from_params

    /// The array-syntax declaration is *stack* knowledge; without `extensions` (or the bracket chars) the
    /// kernel has no syntax to apply and must refuse rather than guess — see `matching_close` / `extract_*`.
    #[test]
    fn array_syntax_from_params_requires_extensions_and_brackets() {
        let valid = json!({
            "open": "[", "close": "]", "extensions": ["php"],
            "separator": "=>", "comment": "//", "class_suffix": "::class"
        });
        assert!(
            ArraySyntax::from_params(&valid, &['\\']).is_some(),
            "a complete declaration is accepted"
        );
        assert!(
            ArraySyntax::from_params(&json!({"open":"[","close":"]"}), &['\\']).is_none(),
            "without `extensions` the syntax is undefined -> None"
        );
        assert!(
            ArraySyntax::from_params(&json!({"extensions":["php"],"close":"]"}), &['\\']).is_none(),
            "missing `open` bracket -> None"
        );
        assert!(
            ArraySyntax::from_params(&json!({"extensions":["php"],"open":"["}), &['\\']).is_none(),
            "missing `close` bracket -> None"
        );
    }

    // ------------------------------------------------------- extract_middleware_classes

    fn php_syn() -> ArraySyntax {
        ArraySyntax {
            open: '[',
            close: ']',
            pair_separator: "=>".into(),
            comment: "//".into(),
            class_suffix: "::class".into(),
            separators: vec!['\\'],
        }
    }

    /// The substring "middleware" also appears inside a *namespace* literal; only the assignment
    /// `middleware => [...]` (separator immediately after the key) must be taken — the namespace literal
    /// must be ignored, otherwise the wrong block's classes are recognised.
    #[test]
    fn extract_middleware_classes_reads_key_block_not_namespace() {
        let text = r#"
            return [
                'app\http\middleware\Foo',
                'middleware' => [
                    \App\Http\Middleware\Auth::class,
                    \App\Http\Middleware\Check::class,
                ],
            ];
        "#;
        let got = extract_middleware_classes(text, Some("middleware"), &php_syn());
        assert_eq!(
            got,
            vec!["App\\Http\\Middleware\\Auth", "App\\Http\\Middleware\\Check"],
            "the assignment key wins; the namespace literal is ignored"
        );
    }

    /// Without a key, the first `[ ... ]` array of the file is used (`return [A::class, ...];` form).
    /// Classes are parsed line-by-line, so each entry must be on its own line.
    #[test]
    fn extract_middleware_classes_takes_first_array_without_key() {
        let text = "return [\n    \\App\\X\\Auth::class,\n    \\App\\X\\Check::class\n];";
        let got = extract_middleware_classes(text, None, &php_syn());
        assert_eq!(got, vec!["App\\X\\Auth", "App\\X\\Check"]);
    }

    /// Trailing comments, surrounding quotes, trailing commas, and the `key => value` form (value side
    /// names the class) are all handled.
    #[test]
    fn extract_middleware_classes_strips_comments_and_quirks() {
        let text = r#"return [
            \App\X\Auth::class, // trailing comment
            'quoted' => \App\X\Check::class,
        ];"#;
        let got = extract_middleware_classes(text, None, &php_syn());
        assert_eq!(
            got,
            vec!["App\\X\\Auth", "App\\X\\Check"],
            "comments / quotes / trailing commas stripped; key=>value keeps the value side"
        );
    }

    /// Two failure modes that must yield *no* classes rather than a partial / wrong list.
    #[test]
    fn extract_middleware_classes_empty_on_absent_key_or_unbalanced() {
        let missing = extract_middleware_classes(
            r#"return [ \App\X\Auth::class ];"#,
            Some("absent"),
            &php_syn(),
        );
        assert!(missing.is_empty(), "an absent key yields no classes");
        let unbalanced = extract_middleware_classes(
            r#"middleware => [ \App\X\Auth::class"#,
            Some("middleware"),
            &php_syn(),
        );
        assert!(
            unbalanced.is_empty(),
            "an unbalanced bracket yields no classes (no silent truncation)"
        );
    }

    // ------------------------------------------------------- declared_mw_path_matches (extra negatives)

    /// `*` matches **exactly one** path segment, so a shorter path cannot satisfy a 3-segment pattern, and
    /// the wildcard segment matching does not excuse a mismatching tail.
    #[test]
    fn declared_mw_path_matches_star_is_one_segment_only() {
        assert!(
            !declared_mw_path_matches("app/*/Middleware", "app/Middleware"),
            "`*` matches exactly one segment; a 2-seg path cannot fill a 3-seg pattern"
        );
        assert!(
            !declared_mw_path_matches("app/*/Middleware", "app/Http/Guard"),
            "the wildcard matches but the tail must equal too"
        );
    }

    // ------------------------------------------------------- parse_create_tables / split_columns / push_column

    /// Constraints (`PRIMARY KEY`, `UNIQUE KEY` …) are dropped, and a comma *inside* a type paren
    /// (`decimal(10,2)`) must not split the column.
    #[test]
    fn parse_create_tables_drops_constraints_and_nested_parens() {
        let sql = r#"
            CREATE TABLE `user` (
                id int(11) NOT NULL,
                score decimal(10,2),
                name varchar(255),
                PRIMARY KEY (id),
                UNIQUE KEY uk_name (name)
            ) ENGINE=InnoDB;
        "#;
        let got = parse_create_tables(sql);
        assert_eq!(got.len(), 1);
        let (table, cols) = &got[0];
        assert_eq!(table, "user");
        assert_eq!(
            cols,
            &["id", "score", "name"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "constraint lines are dropped; comma inside decimal(10,2) does not split the column"
        );
    }

    /// Columns whose names merely *begin* with a constraint keyword (`unique_id`, `index_sort`,
    /// `check_time`, `foreign_id`) must survive the word-boundary filter — dropping them loses the `Column`
    /// node and any PII annotation.
    #[test]
    fn parse_create_tables_keeps_keyword_prefix_columns() {
        let sql = "CREATE TABLE t ( unique_id int, index_sort int, check_time int, foreign_id int, normal int )";
        let (_, cols) = &parse_create_tables(sql)[0];
        for keep in ["unique_id", "index_sort", "check_time", "foreign_id", "normal"] {
            assert!(
                cols.contains(&keep.to_string()),
                "column '{keep}' must be kept, not dropped as a constraint"
            );
        }
    }

    /// Multiple statements parse, `IF NOT EXISTS` / backticks are accepted, and identifiers ≥ 64 chars are
    /// dropped (they cannot be real column names and would otherwise pollute the graph).
    #[test]
    fn parse_create_tables_multiple_tables_and_skips_long_columns() {
        let long = "x".repeat(70);
        let sql = format!("CREATE TABLE a ( id int ); CREATE TABLE IF NOT EXISTS `b` ( ok int, {long} int );");
        let got = parse_create_tables(&sql);
        assert_eq!(got.len(), 2, "both statements are parsed");
        assert_eq!(got[0].0, "a");
        let b_cols = &got[1].1;
        assert!(b_cols.contains(&"ok".to_string()), "normal column kept");
        assert!(
            !b_cols.iter().any(|c| c.len() >= 64),
            "identifiers >= 64 chars are dropped"
        );
    }

    /// `split_columns` only breaks on top-level commas; a comma inside nested parens stays together.
    #[test]
    fn split_columns_splits_only_top_level_commas() {
        assert_eq!(
            split_columns("id int(11), name varchar(255), addr varchar(100)"),
            vec!["id", "name", "addr"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            split_columns("a decimal(10,2), b int"),
            vec!["a", "b"].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "comma inside decimal(10,2) must not split"
        );
    }

    /// `push_column` drops constraint lines and comment-only lines, but keeps columns that merely *start*
    /// with a constraint keyword (the word-boundary rule).
    #[test]
    fn push_column_drops_constraints_keeps_word_boundary_columns() {
        let mut out = vec![];
        push_column(&mut out, "  id int(11)");
        assert_eq!(out, vec!["id".to_string()]);

        out.clear();
        push_column(&mut out, "UNIQUE KEY uk (id)");
        assert!(out.is_empty(), "a constraint line is dropped");

        out.clear();
        push_column(&mut out, "unique_id int");
        assert_eq!(
            out,
            vec!["unique_id".to_string()],
            "a column starting with a keyword but not at a word boundary is kept"
        );

        out.clear();
        push_column(&mut out, "-- pure comment");
        assert!(out.is_empty(), "comment-only lines yield nothing");
    }

    // ------------------------------------------------------- dedup_rules

    fn rule(id: &str) -> Rule {
        Rule {
            id: id.into(),
            phase: Phase::default(),
            selector: Selector::Node {
                node_kind: None,
                r#where: vec![],
            },
            binding: vec![],
            confidence: 0.9,
            languages: None,
        }
    }

    /// Duplicate rule ids are collapsed, preserving the first occurrence's position.
    #[test]
    fn dedup_rules_keeps_first_occurrence_by_id() {
        let rules = vec![rule("a"), rule("b"), rule("a"), rule("c")];
        let deduped = dedup_rules(rules);
        let ids: Vec<&str> = deduped.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "duplicates dropped, order preserved");
    }

    // ------------------------------------------------------- app_root_of

    /// The app-root directory is a *project* decision: when no `app_root` fact exists the kernel defaults
    /// to `app` (never a hard-coded framework name), and a declared fact overrides the default.
    #[test]
    fn app_root_of_falls_back_to_app_then_reads_fact() {
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "app".into(),
            root_path: Path::new("/p/app").to_path_buf(),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: Vec::new(),
            facts: Value::Null,
        };
        let ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        assert_eq!(
            app_root_of(&ctx, &sub),
            "app",
            "without an app_root fact the kernel defaults to 'app', never a hard-coded name"
        );

        let mut ctx2 = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx2.ws.set_fact(SubProjectId(1), "app_root", json!({ "value": "application" }));
        assert_eq!(
            app_root_of(&ctx2, &sub),
            "application",
            "the declared app_root fact must override the default"
        );
    }

    // ------------------------------------------------------- LossyText

    /// `String` / `ClassConst` yield their text; any other variant falls back to Debug formatting.
    #[test]
    fn lossy_text_stringifies_string_and_class_const() {
        assert_eq!(FactValue::String("x".into()).to_string_lossy(), "x");
        assert_eq!(FactValue::ClassConst("y".into()).to_string_lossy(), "y");
        assert_eq!(
            FactValue::Bool(true).to_string_lossy(),
            "Bool(true)",
            "non-string variants fall back to Debug formatting"
        );
    }

    // ------------------------------------------------------- scan_text_files

    /// Only files of the requested extension outside dependency dirs are scanned; the 400-file cap bounds cost.
    #[test]
    fn scan_text_files_reads_exts_and_skips_deps() {
        let base = std::env::temp_dir().join(format!("gt_helper_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("vendor")).unwrap();
        std::fs::write(base.join("a.php"), "<?php // hi").unwrap();
        std::fs::write(base.join("b.txt"), "skip me").unwrap();
        std::fs::write(base.join("vendor").join("c.php"), "should be skipped").unwrap();
        let got = scan_text_files(&base, &["php"]);
        assert_eq!(got.len(), 1, "only .php files outside dependency dirs are scanned");
        assert_eq!(got[0].0, "a.php");
        assert!(got[0].1.contains("hi"));
        let _ = std::fs::remove_dir_all(&base);
    }
}

