//! 源码解析端口。
//!
//! 解析器适配器把任意语言的具体语法树翻译成语言无关的 [`SyntaxFacts`]，
//! 使上层流水线与语言无关。

use crate::error::Result;
use crate::model::{Language, SyntaxFacts};

/// 单语言解析器。
///
/// 除语法解析外，还声明该语言的**命名空间书写规则**与**生态约定**。
/// 后者原本散落在流水线各处（分隔符写死 `\\`、标记文件写死 `composer.json`、
/// 排除目录写死 `vendor/`），使得新增一门语言必须改内核。补齐这几个方法后，
/// 这些知识随语言注册进来，内核不再认识任何具体语言。
pub trait LanguageParser: Send + Sync {
    /// 该解析器负责的语言。
    fn language(&self) -> Language;
    /// 该语言默认的源码扩展名（不含点）。
    fn extensions(&self) -> &'static [&'static str];
    /// 解析单个文件，产出语法事实。
    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts>;

    /// 命名空间分隔符（可能有多个，如 PHP 的 `\`、Python 的 `.`）。
    fn namespace_separator(&self) -> &'static [char];

    /// 成员分隔符：类与其方法/属性之间的符号。
    ///
    /// PHP 与 C++ 是 `::`，Java / JS / Python 是 `.`。内核此前到处硬编码 `::`。
    fn member_separator(&self) -> &'static str {
        "::"
    }

    /// 拼接命名空间与标识符，产出全限定名。
    fn join_namespace(&self, ns: &str, name: &str) -> String {
        let sep = self.namespace_separator().first().copied().unwrap_or('\\');
        let ns = ns.trim_end_matches(|c| self.namespace_separator().contains(&c));
        if ns.is_empty() {
            name.to_string()
        } else {
            format!("{}{}{}", ns, sep, name)
        }
    }

    /// 拼接类与其成员，产出成员的完全限定名（`App\Foo::bar` / `com.Foo.bar`）。
    fn join_member(&self, class_fqn: &str, member: &str) -> String {
        format!("{}{}{}", class_fqn, self.member_separator(), member)
    }

    /// 该语言生态的工程/包标记文件（用于子工程识别），如 `composer.json`。
    fn manifest_files(&self) -> &'static [&'static str] {
        &[]
    }

    /// 该语言生态的依赖与构建输出目录（叠加在通用排除规则之上），如 `vendor/`。
    fn exclude_dirs(&self) -> &'static [&'static str] {
        &[]
    }
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
