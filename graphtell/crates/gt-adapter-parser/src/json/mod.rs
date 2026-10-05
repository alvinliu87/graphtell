//! A minimal JSON parser: it turns uni-app's `pages.json` into `config entry` facts and nothing else.
//!
//! Why JSON is its own language instead of letting the JS parser swallow `.json`:
//! * tree-sitter-typescript cannot parse JSON (it emits half-broken junk facts);
//! * we only care about **route declarations** — collecting every other `.json` (`package.json` /
//!   `tsconfig.json`) entry by entry would pollute the semantic nodes. So this parser **only produces facts
//!   when a `pages` array appears at the top level** and returns empty for all other JSON, which lets it plug
//!   into the existing `kind: config_entry` selector and the `frontend-page` rule at zero cost.
//!
//! Its output is isomorphic to `return [...]` in the backend's `config/*.php`, so the front-end `Page` semantic
//! node and the backend `Route` go through the same Synthesize mechanism, and the route perspective can compare
//! front-end "page <-> endpoint" directly.

use gt_domain::error::Result;
use gt_domain::model::{ConfigEntryFact, FactValue, Language, Span, SyntaxFacts};
use gt_domain::port::LanguageParser;

/// The JSON front-end config parser (serves the `json` language, extension `.json`).
pub struct JsonParser;

impl JsonParser {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl LanguageParser for JsonParser {
    fn language(&self) -> Language {
        Language::new("json")
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["json"]
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &[]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &[]
    }

    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    /// JSON has no classes and therefore no member notation; declared explicitly rather than inheriting a
    /// default, so no language's spelling (`::`) leaks in.
    fn member_separator(&self) -> &'static str {
        "."
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let value: serde_json::Value = match serde_json::from_str(source) {
            Ok(v) => v,
            Err(_) => return Ok(SyntaxFacts::default()),
        };
        let mut facts = SyntaxFacts::default();

        // Only uni-app's pages.json is handled: a top-level `pages` array.
        let Some(obj) = value.as_object() else {
            return Ok(facts);
        };

        // Main-package pages: `pages: ["pages/index/index", ...]`
        if let Some(pages) = obj.get("pages").and_then(|v| v.as_array()) {
            collect_pages(&mut facts, path, source, pages, None);
        }

        if let Some(subs) = obj.get("subPackages").and_then(|v| v.as_array()) {
            for (i, sub) in subs.iter().enumerate() {
                let Some(sub_obj) = sub.as_object() else {
                    continue;
                };
                let root = sub_obj.get("root").and_then(|v| v.as_str()).unwrap_or("");
                let Some(arr) = sub_obj.get("pages").and_then(|v| v.as_array()) else {
                    continue;
                };
                collect_pages(&mut facts, path, source, arr, Some((i, root)));
            }
        }

        Ok(facts)
    }
}

/// Turn a group of page routes into config entries; `sub` = `Some((i, root))` means sub-package handling.
fn collect_pages(
    facts: &mut SyntaxFacts,
    path: &str,
    source: &str,
    pages: &[serde_json::Value],
    sub: Option<(usize, &str)>,
) {
    let _ = path;
    for (j, p) in pages.iter().enumerate() {
        let Some(page) = p.as_str() else {
            continue;
        };
        let route = match sub {
            Some((_, root)) if !root.is_empty() => format!(
                "{}/{}",
                root.trim_end_matches('/'),
                page.trim_start_matches('/')
            ),
            _ => page.to_string(),
        };
        let route = ensure_leading_slash(&route);
        let key_path = match sub {
            Some((i, _)) => format!("subPackages.{i}.pages.{j}"),
            None => format!("pages.{j}"),
        };
        let line = line_of(source, page);
        facts.config_entries.push(ConfigEntryFact {
            key_path,
            value: FactValue::String(route),
            span: Span {
                start_line: line,
                end_line: line,
                start_byte: 0,
                end_byte: 0,
            },
        });
    }
}

/// Route identities always start with `/`, aligned with the normalised identity of navigation calls (so
/// `pages.json` and `uni.navigateTo({ url })` converge on the same `Page` node).
fn ensure_leading_slash(s: &str) -> String {
    if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    }
}

/// Locate the first line where a page string occurs in the source text (so a node can cite a clickable source).
fn line_of(source: &str, needle: &str) -> u32 {
    let mut line = 1u32;
    for (i, ch) in source.char_indices() {
        if ch == '\n' {
            line += 1;
        }
        if source[i..].starts_with(needle) {
            return line;
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_src(src: &str) -> SyntaxFacts {
        JsonParser::new()
            .unwrap()
            .parse("pages.json", src)
            .expect("parsing should succeed")
    }

    #[test]
    fn emits_main_package_pages() {
        let facts = parse_src(r#"{ "pages": ["pages/index/index", "pages/detail/detail"] }"#);
        assert_eq!(facts.config_entries.len(), 2);
        assert_eq!(facts.config_entries[0].key_path, "pages.0");
        assert_eq!(
            facts.config_entries[0].value,
            FactValue::String("/pages/index/index".into())
        );
        assert_eq!(facts.config_entries[1].key_path, "pages.1");
        assert_eq!(
            facts.config_entries[1].value,
            FactValue::String("/pages/detail/detail".into())
        );
    }

    #[test]
    fn subpackage_root_is_prepended() {
        let facts = parse_src(
            r#"{ "pages": ["pages/home/home"], "subPackages": [ { "root": "pagesA", "pages": ["list/list"] } ] }"#,
        );
        // 1 main package + 1 sub-package
        assert_eq!(facts.config_entries.len(), 2);
        let sub = facts
            .config_entries
            .iter()
            .find(|e| e.key_path == "subPackages.0.pages.0")
            .expect("should produce sub-package page entries");
        assert_eq!(sub.value, FactValue::String("/pagesA/list/list".into()));
    }

    #[test]
    fn non_pages_json_is_ignored() {
        // Files like package.json must not pollute semantic nodes
        let facts = parse_src(r#"{ "name": "x", "dependencies": { "axios": "^1" } }"#);
        assert!(
            facts.config_entries.is_empty(),
            "a non-pages.json file should produce no config entries, got: {:?}",
            facts.config_entries
        );
    }

    #[test]
    fn malformed_json_is_safe() {
        let facts = parse_src("{ not valid json");
        assert!(facts.config_entries.is_empty());
    }

    // ---- route identity normalisation ----

    /// Route identities always start with `/`, so `pages.json` and `uni.navigateTo({ url })` converge on the
    /// same `Page` node.
    #[test]
    fn ensure_leading_slash_normalises_route_identity() {
        assert_eq!(ensure_leading_slash("pages/index/index"), "/pages/index/index");
        assert_eq!(ensure_leading_slash("/pages/index/index"), "/pages/index/index", "已有斜杠不重复加");
        assert_eq!(ensure_leading_slash(""), "/");
    }

    /// The span line is what makes a `Page` node clickable in the UI; the **first** occurrence wins and a miss
    /// degrades to the last line rather than 0 (which would point at nothing).
    #[test]
    fn line_of_reports_the_first_occurrence_line() {
        let src = "{\n  \"pages\": [\n    \"pages/index/index\"\n  ]\n}\n";
        assert_eq!(line_of(src, "\"pages/index/index\""), 3);
        assert_eq!(line_of("abc\nx\nabc", "abc"), 1, "多处出现时取第一次");
        assert_eq!(line_of(src, "nope"), 6, "找不到时退化为最后一行，而不是 0");
        assert_eq!(line_of("a\nb", ""), 1);
    }

    #[test]
    fn page_entries_cite_their_source_line() {
        let facts = parse_src("{\n  \"pages\": [\n    \"pages/index/index\"\n  ]\n}\n");
        assert_eq!(facts.config_entries.len(), 1);
        assert_eq!(
            facts.config_entries[0].span.start_line, 3,
            "应指向该页面字符串所在行（可点击溯源）"
        );
    }

    // ---- sub-package handling ----

    /// `root` / `page` are normalised before joining, so neither a trailing nor a leading slash can produce
    /// `pagesA//list/list` or drop the root.
    #[test]
    fn subpackage_root_and_page_slashes_are_normalised() {
        let facts = parse_src(
            r#"{ "subPackages": [ { "root": "pagesA/", "pages": ["/list/list"] } ] }"#,
        );
        assert_eq!(facts.config_entries.len(), 1);
        assert_eq!(
            facts.config_entries[0].value,
            FactValue::String("/pagesA/list/list".into())
        );
    }

    /// A sub-package with an empty / absent `root` must keep the bare route instead of prefixing an empty
    /// segment (which would create a phantom page).
    #[test]
    fn subpackage_without_root_keeps_the_bare_route() {
        let facts = parse_src(r#"{ "subPackages": [ { "root": "", "pages": ["list/list"] } ] }"#);
        assert_eq!(facts.config_entries.len(), 1);
        assert_eq!(
            facts.config_entries[0].value,
            FactValue::String("/list/list".into())
        );
    }

    /// The sub-package **index** is part of the key path, so two sub-packages must not collapse into one slot.
    #[test]
    fn subpackages_are_indexed_in_their_key_path() {
        let facts = parse_src(
            r#"{ "subPackages": [ { "root": "A", "pages": ["a/a"] }, { "root": "B", "pages": ["b/b"] } ] }"#,
        );
        let paths: Vec<&str> = facts
            .config_entries
            .iter()
            .map(|e| e.key_path.as_str())
            .collect();
        assert_eq!(paths, vec!["subPackages.0.pages.0", "subPackages.1.pages.0"]);
    }

    /// Non-string entries in the `pages` array are skipped rather than coerced (a number would become a bogus
    /// route identity).
    #[test]
    fn non_string_page_entries_are_skipped() {
        let facts = parse_src(r#"{ "pages": ["pages/a/a", 1, null, true] }"#);
        assert_eq!(facts.config_entries.len(), 1, "只有字符串条目应成为页面");
        assert_eq!(facts.config_entries[0].key_path, "pages.0");
    }
}
