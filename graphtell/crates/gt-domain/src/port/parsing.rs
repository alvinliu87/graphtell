//! The source-parsing port.
//!
//! A parser adapter translates any language's concrete syntax tree into the language-agnostic [`SyntaxFacts`],
//! making the upper pipeline language-independent.

use crate::error::Result;
use crate::model::{Language, SyntaxFacts};

/// A single-language parser.
///
/// Besides parsing, it declares the language's **namespace notation rules** and **ecosystem conventions**.
/// That knowledge arrives with the language registration, so the kernel knows no concrete language and adding
/// one needs no kernel change.
pub trait LanguageParser: Send + Sync {
    /// The language this parser is responsible for.
    fn language(&self) -> Language;
    /// The language's default source extensions (without the dot).
    fn extensions(&self) -> &'static [&'static str];
    /// Parse a single file into syntax facts.
    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts>;

    /// Namespace separator (there may be several, e.g. PHP's `\` and Python's `.`).
    fn namespace_separator(&self) -> &'static [char];

    /// How this language writes a **variable reference inside a string** — the prefixes that mark one
    /// (PHP `$x` / `{$x}` / `${x}`, JS template `` `${x}` ``, …). Empty means "this language gives no
    /// textual marker", so a reader cannot tell a variable from ordinary text and must not guess.
    fn variable_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Member separator: the symbol between a class and its methods / properties.
    ///
    /// PHP and C++ use `::`, while Java / JS / Python use `.`. **Required, not defaulted**: a default would
    /// mean one language's notation (`::` was the old default) silently leaks into every parser that forgets
    /// to declare its own.
    fn member_separator(&self) -> &'static str;

    /// Join a namespace with an identifier into a fully qualified name.
    fn join_namespace(&self, ns: &str, name: &str) -> String {
        let ns = ns.trim_end_matches(|c| self.namespace_separator().contains(&c));
        match self.namespace_separator().first().copied() {
            _ if ns.is_empty() => name.to_string(),
            Some(sep) => format!("{}{}{}", ns, sep, name),
            // No separator declared for this language: the name cannot be qualified — leave it as written
            // rather than joining with another stack's separator.
            None => name.to_string(),
        }
    }

    /// Join a class with its member into the member's fully qualified name (`App\Foo::bar` / `com.Foo.bar`).
    fn join_member(&self, class_fqn: &str, member: &str) -> String {
        format!("{}{}{}", class_fqn, self.member_separator(), member)
    }

    /// The project / package marker files of this language's ecosystem (for sub-project detection), e.g. `composer.json`.
    fn manifest_files(&self) -> &'static [&'static str] {
        &[]
    }

    /// The dependency and build-output directories of this language's ecosystem (layered on the common exclusions), e.g. `vendor/`.
    fn exclude_dirs(&self) -> &'static [&'static str] {
        &[]
    }

    /// Primitive / builtin type names that must NOT be treated as class references during type inference
    /// (e.g. PHP `int` / `string`, Java `long` / `boolean`, JS `number` / `any`). The kernel knows no
    /// language, so this lives with the parser; the default (empty) means "nothing special", and each
    /// adapter declares its own list **inside its own crate** — no language's vocabulary belongs here.
    fn builtin_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether instance-field access uses **bare identifiers** (no `$` / `@` textual marker), so a bare
    /// receiver (`repo`, `self`, `Order.objects`) is a field / instance access the `variable_type`
    /// resolver should consider.
    ///
    /// PHP injects via `$this->prop` (marked with `$`), so it is `false`. Java / JS / Python use bare
    /// identifiers, so they are `true`. The kernel gates the P7 `VariableType` resolver on this flag
    /// instead of hard-coding `language == java / javascript / python`.
    fn bare_field_receivers(&self) -> bool {
        false
    }
}

/// Parser registry (factory port).
///
/// Adding a language only means registering a new implementation; no upper-layer code changes.
pub trait ParserRegistry: Send + Sync {
    fn parser_for(&self, language: &Language) -> Option<&dyn LanguageParser>;
    fn supported_languages(&self) -> Vec<Language>;

    /// Infer the language from a file extension.
    fn language_for_extension(&self, ext: &str) -> Option<Language> {
        self.supported_languages().into_iter().find(|lang| {
            self.parser_for(lang)
                .map(|p| p.extensions().iter().any(|e| e.eq_ignore_ascii_case(ext)))
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SyntaxFacts;

    // Minimal `LanguageParser` stubs so the trait's default `join_*` methods can be exercised. Only the
    // required (non-defaulted) methods are implemented; the constant defaults are left to the trait.
    struct PhpLikeParser;
    impl LanguageParser for PhpLikeParser {
        fn language(&self) -> Language {
            Language::new("php")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["php"]
        }
        fn parse(&self, _path: &str, _source: &str) -> Result<SyntaxFacts> {
            Ok(SyntaxFacts::default())
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['\\']
        }
        fn member_separator(&self) -> &'static str {
            "::"
        }
    }

    struct JsLikeParser;
    impl LanguageParser for JsLikeParser {
        fn language(&self) -> Language {
            Language::new("javascript")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["js", "ts"]
        }
        fn parse(&self, _path: &str, _source: &str) -> Result<SyntaxFacts> {
            Ok(SyntaxFacts::default())
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['.']
        }
        fn member_separator(&self) -> &'static str {
            "."
        }
    }

    /// A parser that declares **no** namespace separator: `join_namespace` must then leave the name as written
    /// rather than leaking another stack's separator.
    struct BareParser;
    impl LanguageParser for BareParser {
        fn language(&self) -> Language {
            Language::new("unknown")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &[]
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

    #[test]
    fn join_namespace_handles_separator_and_empty_ns() {
        let p = PhpLikeParser;
        assert_eq!(p.join_namespace("app\\services\\order", "StoreOrderDao"), "app\\services\\order\\StoreOrderDao");
        // A trailing separator is stripped before joining, so the result is identical.
        assert_eq!(p.join_namespace("app\\services\\order\\", "StoreOrderDao"), "app\\services\\order\\StoreOrderDao");
        // Empty namespace yields just the name.
        assert_eq!(p.join_namespace("", "Foo"), "Foo");
        assert_eq!(p.join_namespace("app", "Foo"), "app\\Foo");
    }

    /// No separator declared -> the name is returned unchanged (never joined with a foreign separator).
    #[test]
    fn join_namespace_without_separator_returns_name() {
        let p = BareParser;
        assert_eq!(p.join_namespace("ns", "Foo"), "Foo");
    }

    #[test]
    fn join_member_uses_the_languages_member_separator() {
        assert_eq!(PhpLikeParser.join_member("app\\Foo", "bar"), "app\\Foo::bar");
        assert_eq!(JsLikeParser.join_member("com.Foo", "bar"), "com.Foo.bar");
    }

    // A registry over the two stubs above, to exercise the default `language_for_extension`.
    struct StubRegistry {
        php: PhpLikeParser,
        js: JsLikeParser,
    }
    impl ParserRegistry for StubRegistry {
        fn parser_for(&self, language: &Language) -> Option<&dyn LanguageParser> {
            if *language == Language::new("php") {
                Some(&self.php)
            } else if *language == Language::new("javascript") {
                Some(&self.js)
            } else {
                None
            }
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("php"), Language::new("javascript")]
        }
    }

    /// Extension lookup is case-insensitive and stops at the first language whose parser lists the extension.
    #[test]
    fn language_for_extension_is_case_insensitive_and_first_match() {
        let reg = StubRegistry { php: PhpLikeParser, js: JsLikeParser };
        assert_eq!(reg.language_for_extension("php"), Some(Language::new("php")));
        assert_eq!(reg.language_for_extension("PHP"), Some(Language::new("php")), "大小写不敏感");
        assert_eq!(reg.language_for_extension("ts"), Some(Language::new("javascript")), "ts 归属 javascript");
        assert!(reg.language_for_extension("unknownext").is_none(), "未知扩展名返回 None");
    }
}
