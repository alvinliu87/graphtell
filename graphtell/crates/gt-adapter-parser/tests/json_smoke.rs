//! JSON parsing smoke test: verify that syntax fact extraction is correct through the public registry.
//!
//! Mirrors `php_smoke.rs` / `java_smoke.rs` / `js_smoke.rs`: the unit tests in `src/json.rs` call
//! `JsonParser::new()` directly, so nothing pinned the **registry wiring** — that a `.json` file lands on
//! `JsonParser` at all (`DefaultParserRegistry` maps the `json` language), and that the route facts the
//! `frontend-page` rule consumes come out normalised on that path.
//!
//! The pipeline half (`pages.json` -> `Page` semantic node, and `uni.navigateTo` merging onto the same node)
//! is pinned end-to-end by `gt-pipeline/tests/link_sample.rs` against the committed `frontend-backend-link`
//! sample; this file is the parser-level counterpart.

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{Language, SyntaxFacts};
use gt_domain::port::ParserRegistry;

fn parse(path: &str, src: &str) -> SyntaxFacts {
    let reg = DefaultParserRegistry::new();
    let p = reg
        .parser_for(&Language::new("json"))
        .expect("the json parser must be registered");
    p.parse(path, src).expect("parse ok")
}

/// uni-app's `pages.json` -> `config entry` facts whose values are **normalised route identities** (leading
/// slash, sub-package root joined) — the shape the `kind: config_entry` selector and the `frontend-page`
/// Synthesize rule read. The span line is what makes a `Page` node clickable.
#[test]
fn pages_json_becomes_normalised_route_config_entries() {
    let src = "{\n  \"pages\": [\"pages/index/index\"],\n  \"subPackages\": [ { \"root\": \"pagesA/\", \"pages\": [\"/list/list\"] } ]\n}\n";
    let facts = parse("pages.json", src);

    let got: Vec<(&str, &str, u32)> = facts
        .config_entries
        .iter()
        .map(|e| {
            (
                e.key_path.as_str(),
                match &e.value {
                    gt_domain::model::FactValue::String(s) => s.as_str(),
                    _ => "<non-string>",
                },
                e.span.start_line,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("pages.0", "/pages/index/index", 2),
            ("subPackages.0.pages.0", "/pagesA/list/list", 3),
        ],
        "a main-package page and a sub-package page must both become normalised routes, on their own source line"
    );
}

/// The reason JSON is a language of its own: every other `.json` in a front-end project (`package.json` /
/// `tsconfig.json` / …) must produce **nothing**, or those entries would pollute the semantic nodes.
#[test]
fn non_pages_json_produces_no_config_entries() {
    for (path, src) in [
        ("package.json", r#"{ "name": "x", "dependencies": { "axios": "^1" } }"#),
        ("tsconfig.json", r#"{ "compilerOptions": { "strict": true } }"#),
        ("manifest.json", r#"{ "mp-weixin": { "appid": "wx1" } }"#),
    ] {
        let facts = parse(path, src);
        assert!(
            facts.config_entries.is_empty(),
            "`{path}` must produce no config entries, got: {:?}",
            facts.config_entries
        );
    }
}
