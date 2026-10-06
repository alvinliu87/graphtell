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
