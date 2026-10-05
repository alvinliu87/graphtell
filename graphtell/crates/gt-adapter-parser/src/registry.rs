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
        for want in ["php", "java", "python", "javascript", "typescript", "json"] {
            assert!(langs.iter().any(|l| l == want), "缺少语言 `{want}`: {langs:?}");
        }
        let mut sorted = langs.clone();
        sorted.sort();
        assert_eq!(langs, sorted, "supported_languages 必须有序（顺序会传到上层枚举）");
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
            .expect("未知语言应返回 Err");
        assert!(
            matches!(err, DomainError::Unsupported(_)),
            "未知语言必须报 Unsupported，而不是泛化错误"
        );
        assert!(format!("{err}").contains("cobol"), "错误信息应带上语言名: {err}");
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
            "同语言重复注册应覆盖，而不是被忽略"
        );
    }

    /// TypeScript is served by the **frontend** parser (same tree-sitter grammar family), so `ts` / `tsx` must
    /// resolve — a language that is declared but has no parser is invisible to the whole pipeline.
    #[test]
    fn typescript_is_served_by_the_frontend_parser() {
        let reg = DefaultParserRegistry::new();
        let ts = reg
            .parser_for(&Language::new(Language::TYPESCRIPT))
            .expect("typescript 应有解析器");
        assert!(ts.extensions().contains(&"ts"), "扩展名应含 ts: {:?}", ts.extensions());
        assert!(ts.extensions().contains(&"tsx"));
        let js = reg
            .parser_for(&Language::new(Language::JAVASCRIPT))
            .expect("javascript 应有解析器");
        assert!(js.extensions().contains(&"js"));
    }
}
