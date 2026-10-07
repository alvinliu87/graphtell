//! Parser registry: the language -> parser mapping.
//!
//! Adding a language only means registering one [`LanguageParser`] implementation here; no upper-layer code
//! changes (open-closed principle).

use std::collections::HashMap;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::Language;
use gt_domain::port::{LanguageParser, ParserRegistry};

use crate::java::JavaParser;
use crate::js::JsFrontendParser;
use crate::json::JsonParser;
use crate::php::PhpParser;
use crate::python::PythonParser;
use crate::rust::RustParser;

/// The default registry.
pub struct DefaultParserRegistry {
    parsers: HashMap<String, Box<dyn LanguageParser>>,
}

impl DefaultParserRegistry {
    pub fn new() -> Self {
        let mut parsers: HashMap<String, Box<dyn LanguageParser>> = HashMap::new();
        if let Ok(p) = PhpParser::new() {
            parsers.insert(Language::PHP.to_string(), Box::new(p));
        }
        if let Ok(p) = JavaParser::new() {
            parsers.insert(Language::JAVA.to_string(), Box::new(p));
        }
        if let Ok(p) = PythonParser::new() {
            parsers.insert(Language::PYTHON.to_string(), Box::new(p));
        }
        if let Ok(p) = JsFrontendParser::new() {
            parsers.insert(Language::JAVASCRIPT.to_string(), Box::new(p));
        }
        if let Ok(p) = JsFrontendParser::new() {
            parsers.insert(Language::TYPESCRIPT.to_string(), Box::new(p));
        }
        if let Ok(p) = JsonParser::new() {
            parsers.insert(Language::new("json").to_string(), Box::new(p));
        }
        if let Ok(p) = RustParser::new() {
            parsers.insert(Language::RUST.to_string(), Box::new(p));
        }
        Self { parsers }
    }

    /// Register a new language parser (for extensions / tests).
    pub fn register(&mut self, parser: Box<dyn LanguageParser>) {
        self.parsers.insert(parser.language().to_string(), parser);
    }
}

impl Default for DefaultParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParserRegistry for DefaultParserRegistry {
    fn parser_for(&self, language: &Language) -> Option<&dyn LanguageParser> {
        self.parsers.get(language.as_str()).map(|p| p.as_ref())
    }

    fn supported_languages(&self) -> Vec<Language> {
        let mut v: Vec<Language> = self
            .parsers
            .keys()
            .map(|k| Language::new(k.clone()))
            .collect();
        v.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        v
    }
}

/// Convenience constructor: get a parser by language name, returning a clear error for unknown languages.
pub fn require_parser<'a>(
    registry: &'a dyn ParserRegistry,
    language: &Language,
) -> Result<&'a dyn LanguageParser> {
    registry.parser_for(language).ok_or_else(|| {
        DomainError::Unsupported(format!("Language not supported yet: {}", language))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::SyntaxFacts;

    struct StubParser {
        lang: String,
        exts: &'static [&'static str],
    }

    impl LanguageParser for StubParser {
        fn language(&self) -> Language {
            Language::new(self.lang.clone())
        }
        fn extensions(&self) -> &'static [&'static str] {
            self.exts
        }
        fn parse(&self, _path: &str, _source: &str) -> Result<SyntaxFacts> {
            Ok(SyntaxFacts::default())
        }
        fn namespace_separator(&self) -> &'static [char] {
            &[]
        }
        fn member_separator(&self) -> &'static str {
            "."
        }
    }

    /// `new()` registers each parser **only if it constructs** (`if let Ok(p)`), so a parser that fails to build
    /// disappears silently — this is the guard that a language never quietly goes missing.
    #[test]
    fn registry_registers_every_expected_language() {
        let reg = DefaultParserRegistry::new();
        let langs: Vec<String> = reg
            .supported_languages()
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        for want in ["php", "java", "python", "javascript", "typescript", "json", "rust"] {
            assert!(langs.iter().any(|l| l == want), "missing language `{want}`: {langs:?}");
        }
        assert_eq!(langs.len(), 7, "no extra or duplicated language may be registered: {langs:?}");
        let mut sorted = langs.clone();
        sorted.sort();
        assert_eq!(langs, sorted, "supported_languages must be sorted (the order is passed up to the enumeration)");
    }

    #[test]
    fn parser_for_returns_none_for_unknown_language() {
        let reg = DefaultParserRegistry::new();
        assert!(reg.parser_for(&Language::new("cobol")).is_none());
    }

    #[test]
    fn require_parser_reports_unsupported_language() {
        let reg = DefaultParserRegistry::new();
        assert!(require_parser(&reg, &Language::new(Language::PHP)).is_ok());
        let err = require_parser(&reg, &Language::new("cobol"))
            .err()
            .expect("an unknown language must return Err");
        assert!(
            matches!(err, DomainError::Unsupported(_)),
            "an unknown language must report Unsupported, not a generic error"
        );
        assert!(format!("{err}").contains("cobol"), "the error message must carry the language name: {err}");
    }

    /// Registering is keyed by language: a new language is added, and re-registering an existing one
    /// **replaces** it (that is what makes the registry extensible without touching upper layers).
    #[test]
    fn register_adds_a_language_and_overrides_an_existing_one() {
        let mut reg = DefaultParserRegistry::new();
        reg.register(Box::new(StubParser { lang: "cobol".into(), exts: &[".cbl"] }));
        assert_eq!(
            reg.parser_for(&Language::new("cobol"))
                .and_then(|p| p.extensions().first().copied()),
            Some(".cbl")
        );

        reg.register(Box::new(StubParser { lang: "php".into(), exts: &[".stub"] }));
        let same = reg.parser_for(&Language::new(Language::PHP));
        assert_eq!(
            same.and_then(|p| p.extensions().first().copied()),
            Some(".stub"),
            "registering the same language twice must override, not be ignored"
        );
    }

    /// TypeScript is served by the **frontend** parser (same tree-sitter grammar family), so `ts` / `tsx` must
    /// resolve — a language that is declared but has no parser is invisible to the whole pipeline.
    #[test]
    fn typescript_is_served_by_the_frontend_parser() {
        let reg = DefaultParserRegistry::new();
        let ts = reg
            .parser_for(&Language::new(Language::TYPESCRIPT))
            .expect("typescript must have a parser");
        assert!(ts.extensions().contains(&"ts"), "the extensions must include ts: {:?}", ts.extensions());
        assert!(ts.extensions().contains(&"tsx"));
        let js = reg
            .parser_for(&Language::new(Language::JAVASCRIPT))
            .expect("javascript must have a parser");
        assert!(js.extensions().contains(&"js"));
    }

    /// `Default::default()` must delegate to `new()` so both constructors agree on the language set —
    /// otherwise `DefaultParserRegistry::default()` (used widely by callers) could silently diverge.
    #[test]
    fn default_delegates_to_new() {
        let a = DefaultParserRegistry::new();
        let b = DefaultParserRegistry::default();
        assert_eq!(
            a.supported_languages(),
            b.supported_languages(),
            "Default::default() must register the same language set as new()"
        );
    }

    /// `parser_for` returns a parser for every registered language — guards the
    /// `map(|p| p.as_ref())` conversion and that lookups are not silently cross-wired.
    ///
    /// Note: TypeScript is served by the **frontend** parser, whose `language()` reports `javascript`
    /// (the same grammar family), so it deliberately does *not* equal the lookup key `typescript`.
    #[test]
    fn parser_for_returns_a_parser_for_each_language() {
        let reg = DefaultParserRegistry::new();
        for want in ["php", "java", "python", "javascript", "json"] {
            let p = reg
                .parser_for(&Language::new(want))
                .unwrap_or_else(|| panic!("{want} must be registered"));
            assert_eq!(
                p.language().as_str(),
                want,
                "the parser returned by parser_for({want}) must report the same language"
            );
        }
        // typescript resolves to the frontend parser (reports "javascript") but still serves ts/tsx.
        let ts = reg
            .parser_for(&Language::new(Language::TYPESCRIPT))
            .expect("typescript must have a parser");
        assert_eq!(
            ts.language().as_str(),
            "javascript",
            "typescript is served by the frontend parser, so its language() should be javascript"
        );
        assert!(ts.extensions().contains(&"ts"));
        assert!(ts.extensions().contains(&"tsx"));
    }

    /// `supported_languages` reflects registrations: a brand-new language appears, and re-registering an
    /// existing one does **not** create a duplicate entry (the map is keyed by language, so override must
    /// replace, not append).
    #[test]
    fn supported_languages_reflects_registration_without_duplicates() {
        let mut reg = DefaultParserRegistry::new();
        let base_count = reg.supported_languages().len();

        reg.register(Box::new(StubParser { lang: "cobol".into(), exts: &[".cbl"] }));
        let with_cobol = reg.supported_languages();
        assert_eq!(with_cobol.len(), base_count + 1, "adding a language grows the list by one");
        assert!(with_cobol.iter().any(|l| l.as_str() == "cobol"));

        reg.register(Box::new(StubParser { lang: "php".into(), exts: &[".stub"] }));
        let after_override = reg.supported_languages();
        assert_eq!(
            after_override.len(),
            base_count + 1,
            "overriding the same language must not produce duplicate entries"
        );
        assert_eq!(
            after_override.iter().filter(|l| l.as_str() == "php").count(),
            1,
            "php must appear exactly once in supported_languages"
        );
    }

    /// `language_for_extension` is the **default trait method** that routes every concrete file to its parser
    /// (`parser_for_file` in the pipeline falls back to the sub-project language when it misses) — so a silent
    /// miss here means "the file was parsed into nothing". It had no coverage against the real registry.
    #[test]
    fn language_for_extension_routes_files_to_their_parser() {
        let reg = DefaultParserRegistry::new();
        let lang_of = |ext: &str| reg.language_for_extension(ext).map(|l| l.as_str().to_string());

        assert_eq!(lang_of("php").as_deref(), Some("php"));
        assert_eq!(lang_of("PHP").as_deref(), Some("php"), "extension matching is case-insensitive");
        assert_eq!(lang_of("java").as_deref(), Some("java"));
        assert_eq!(lang_of("py").as_deref(), Some("python"));
        assert_eq!(lang_of("pyi").as_deref(), Some("python"));
        assert_eq!(lang_of("json").as_deref(), Some("json"));
        assert_eq!(lang_of("rs").as_deref(), Some("rust"));

        // A miss must be a miss, not a guess.
        assert!(lang_of("cobol").is_none(), "an unknown extension must not fall back to some language");
        assert!(lang_of("").is_none());
        assert!(
            lang_of(".php").is_none(),
            "extensions are compared without a dot — callers pass `Path::extension()`, so a dotted string must not match"
        );

        // `.ts` / `.tsx` / `.vue` currently resolve to the **javascript** key: `supported_languages()` is
        // iterated in sorted order and both the `javascript` and `typescript` keys hold the frontend parser
        // with the same extension list, so the alphabetically first wins. The picked parser is the same either
        // way — but the *language label* differs, so it is pinned rather than left to drift.
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "vue"] {
            assert_eq!(
                lang_of(ext).as_deref(),
                Some("javascript"),
                "`{ext}` is owned by the frontend parser; first sorted match is `javascript`"
            );
        }
    }

    struct CobolParser;
    impl LanguageParser for CobolParser {
        fn language(&self) -> Language {
            Language::new("cobol")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["cbl"]
        }
        fn parse(&self, _path: &str, _source: &str) -> Result<SyntaxFacts> {
            Ok(SyntaxFacts::default())
        }
        fn namespace_separator(&self) -> &'static [char] {
            &[]
        }
        fn member_separator(&self) -> &'static str {
            "."
        }
    }

    /// A whole registry speaking the `ParserRegistry` trait — the openness this module exists for (adding a
    /// language must not touch upper layers).
    struct CobolRegistry {
        inner: CobolParser,
    }
    impl ParserRegistry for CobolRegistry {
        fn parser_for(&self, language: &Language) -> Option<&dyn LanguageParser> {
            (language.as_str() == "cobol").then_some(&self.inner as &dyn LanguageParser)
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("cobol")]
        }
    }

    /// `require_parser` takes `&dyn ParserRegistry`, not the concrete default — every existing test only ever
    /// exercised it through `DefaultParserRegistry`, so the extension point itself was unverified.
    ///
    /// Also pinned: language lookup is **exact-case**. `Language::new` does not fold case, so an uppercase name
    /// finds no parser even though *extension* matching is case-insensitive — an asymmetry worth knowing before
    /// normalising one of them.
    #[test]
    fn require_parser_works_through_the_trait_and_lookups_are_exact_case() {
        let reg = CobolRegistry { inner: CobolParser };
        let any: &dyn ParserRegistry = &reg;

        let p = require_parser(any, &Language::new("cobol")).expect("the trait-based registry must be usable");
        assert_eq!(p.language().as_str(), "cobol");
        assert_eq!(p.extensions(), &["cbl"]);
        assert!(require_parser(any, &Language::new("php")).is_err());

        let default = DefaultParserRegistry::new();
        assert!(
            default.parser_for(&Language::new("PHP")).is_none(),
            "a language key is looked up by exact string — `Language::new` does not fold case"
        );
        assert!(default.parser_for(&Language::new(Language::PHP)).is_some());
    }

    /// `require_parser` hands back the concrete parser for a supported language, not merely an `Ok`.
    #[test]
    fn require_parser_returns_the_concrete_parser() {
        let reg = DefaultParserRegistry::new();
        let p = require_parser(&reg, &Language::new(Language::JAVA)).unwrap();
        assert_eq!(p.language().as_str(), "java");
    }
}
