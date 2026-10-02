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
