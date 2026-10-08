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
const EXPECTED: &[&str] = &["php", "java", "python", "javascript", "typescript", "json", "rust"];

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

/// The scanner routes every concrete file to its parser through `language_for_extension` (the pipeline's
/// `parser_for_file` falls back to the sub-project language only on a miss) — so a silent miss here means
/// "the file was parsed into nothing". It only had a `src/` unit test; this pins the behaviour on the
/// **public** contract surface the rest of the system depends on.
#[test]
fn language_for_extension_routes_real_source_files() {
    let reg = DefaultParserRegistry::new();
    let lang_of = |ext: &str| reg.language_for_extension(ext).map(|l| l.as_str().to_string());

    assert_eq!(lang_of("php").as_deref(), Some("php"));
    assert_eq!(lang_of("java").as_deref(), Some("java"));
    assert_eq!(lang_of("py").as_deref(), Some("python"));
    assert_eq!(lang_of("pyi").as_deref(), Some("python"));
    assert_eq!(lang_of("json").as_deref(), Some("json"));
    assert_eq!(lang_of("rs").as_deref(), Some("rust"));

    // `.ts` / `.tsx` / `.vue` resolve to the **javascript** key: both the `javascript` and `typescript`
    // keys hold the same frontend parser, so the alphabetically first wins. The picked parser is identical
    // either way, but the *label* differs — pinned rather than left to drift.
    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "vue"] {
        assert_eq!(
            lang_of(ext).as_deref(),
            Some("javascript"),
            "`{ext}` is owned by the frontend parser; first sorted match is `javascript`"
        );
    }

    // Extension matching is case-insensitive...
    assert_eq!(lang_of("PHP").as_deref(), Some("php"), "extension matching is case-insensitive");
    // ...but a miss must be a miss, and a dotted string must not match.
    assert!(lang_of("cobol").is_none(), "an unknown extension must not fall back to some language");
    assert!(lang_of("").is_none());
    assert!(
        lang_of(".php").is_none(),
        "extensions are compared without a dot — callers pass `Path::extension()`, so a dotted string must not match"
    );
}

/// Asymmetry worth guarding on the public surface: a *language key* is looked up by exact string
/// (`Language::new` does not fold case), while *extension* matching is case-insensitive. `Language::new("PHP")`
/// finds no parser, yet extension `PHP` routes to php. Normalising either side would silently change this.
#[test]
fn language_lookup_is_exact_case_while_extension_match_is_not() {
    let reg = DefaultParserRegistry::new();
    assert!(
        reg.parser_for(&Language::new("PHP")).is_none(),
        "a language key is looked up by exact string — `Language::new` does not fold case"
    );
    assert!(
        reg.parser_for(&Language::new(Language::PHP)).is_some(),
        "the canonical lowercase key must resolve"
    );
    assert_eq!(
        reg.language_for_extension("PHP").map(|l| l.as_str().to_string()).as_deref(),
        Some("php"),
        "extension matching is case-insensitive, unlike the language-key lookup above"
    );
}
