//! 解析器注册中心：语言 → 解析器的映射。
//!
//! 新增语言只需在此注册一个 [`LanguageParser`] 实现，无需改动上层任何代码
//! （开闭原则）。

use std::collections::HashMap;

use em_domain::error::{DomainError, Result};
use em_domain::model::Language;
use em_domain::port::{LanguageParser, ParserRegistry};

use crate::php::PhpParser;

/// 默认注册中心。
pub struct DefaultParserRegistry {
    parsers: HashMap<String, Box<dyn LanguageParser>>,
}

impl DefaultParserRegistry {
    pub fn new() -> Self {
        let mut parsers: HashMap<String, Box<dyn LanguageParser>> = HashMap::new();
        // 目标：兼容所有主流技术栈；当前先落地 PHP。
        if let Ok(p) = PhpParser::new() {
            parsers.insert(Language::PHP.to_string(), Box::new(p));
        }
        Self { parsers }
    }

    /// 注册一个新语言解析器（供扩展/测试使用）。
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

/// 便捷构造：按语言名取解析器，未知语言返回明确错误。
pub fn require_parser<'a>(
    registry: &'a dyn ParserRegistry,
    language: &Language,
) -> Result<&'a dyn LanguageParser> {
    registry.parser_for(language).ok_or_else(|| {
        DomainError::Unsupported(format!("暂不支持的语言: {}", language))
    })
}
