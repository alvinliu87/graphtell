//! Parser registry completeness — the one piece of `gt-adapter-parser` that had no test.
//!
//! If a `parsers.insert(...)` line is dropped, or a parser's `new()` starts returning `Err`, that language
//! silently disappears from `parser_for`. The scanner still detects the language and the sub-project is still
//! created, but the whole pipeline yields **zero facts** for it — a high-impact, silent failure. These tests
//! pin the registry to the language set the rest of the system assumes is always available.

use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_parser::require_parser;
use gt_domain::model::Language;
use gt_domain::port::ParserRegistry;

/// Languages the pipeline / inbound adapters assume are always available.
const EXPECTED: &[&str] = &["php", "java", "python", "javascript", "typescript", "json"];

#[test]
fn registry_exposes_every_required_language() {
    let reg = DefaultParserRegistry::new();
    for lang in EXPECTED {
        assert!(
            reg.parser_for(&Language::new(*lang)).is_some(),
            "language {lang} must have a parser registered (otherwise it silently produces no facts in the pipeline)"
        );
    }
}

#[test]
fn supported_languages_contains_all_expected_and_is_sorted() {
    let reg = DefaultParserRegistry::new();
    let langs = reg.supported_languages();
    assert!(!langs.is_empty());

    // Every expected language is reported, and nothing duplicates.
    for lang in EXPECTED {
        assert!(
            langs.iter().any(|l| l.as_str() == *lang),
            "supported_languages must contain {lang}"
        );
    }
    // The returned list is already sorted (the trait guarantees an order).
    let mut sorted = langs.clone();
    sorted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(langs, sorted, "supported_languages must be sorted");
}

#[test]
fn unknown_language_is_not_registered() {
    let reg = DefaultParserRegistry::new();
    assert!(reg.parser_for(&Language::new("go")).is_none());
    assert!(reg.parser_for(&Language::new("rust")).is_none());
    assert!(reg.parser_for(&Language::new("cobol")).is_none());
}

#[test]
fn require_parser_errors_for_unknown_language() {
    let reg = DefaultParserRegistry::new();
    assert!(
        require_parser(&reg, &Language::new("cobol")).is_err(),
        "require_parser must return a clear Unsupported error for an unregistered language"
    );
}
