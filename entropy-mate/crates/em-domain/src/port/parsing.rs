//! 源码解析端口。
//!
//! 解析器适配器把任意语言的具体语法树翻译成语言无关的 [`SyntaxFacts`]，
//! 使上层流水线与语言无关。

use crate::error::Result;
use crate::model::{Language, SyntaxFacts};

/// 单语言解析器。
pub trait LanguageParser: Send + Sync {
    /// 该解析器负责的语言。
    fn language(&self) -> Language;
    /// 该语言默认的源码扩展名（不含点）。
    fn extensions(&self) -> &'static [&'static str];
    /// 解析单个文件，产出语法事实。
    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts>;
}

/// 解析器注册中心（工厂端口）。
///
/// 新增语言只需注册一个新实现，无需改动任何上层代码。
pub trait ParserRegistry: Send + Sync {
    fn parser_for(&self, language: &Language) -> Option<&dyn LanguageParser>;
    fn supported_languages(&self) -> Vec<Language>;

    /// 按文件扩展名推断语言。
    fn language_for_extension(&self, ext: &str) -> Option<Language> {
        self.supported_languages().into_iter().find(|lang| {
            self.parser_for(lang)
                .map(|p| p.extensions().iter().any(|e| e.eq_ignore_ascii_case(ext)))
                .unwrap_or(false)
        })
    }
}
