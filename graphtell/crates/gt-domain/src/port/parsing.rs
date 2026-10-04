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

    /// Member separator: the symbol between a class and its methods / properties.
    ///
    /// PHP and C++ use `::`, while Java / JS / Python use `.`.
    fn member_separator(&self) -> &'static str {
        "::"
    }

    /// Join a namespace with an identifier into a fully qualified name.
    fn join_namespace(&self, ns: &str, name: &str) -> String {
        let sep = self.namespace_separator().first().copied().unwrap_or('\\');
        let ns = ns.trim_end_matches(|c| self.namespace_separator().contains(&c));
        if ns.is_empty() {
            name.to_string()
        } else {
            format!("{}{}{}", ns, sep, name)
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
    /// adapter declares its own — see [`PHP_BUILTIN_TYPES`] for the PHP set.
    fn builtin_types(&self) -> &'static [&'static str] {
        &[]
    }
}

/// PHP's primitive / builtin type names (used by [`LanguageParser::builtin_types`] for PHP and as the
/// `NamespacePolicy` fallback when no parser is wired up). Declared once here so the kernel, the
/// `NamespacePolicy` fallback, and the PHP parser adapter all share the same list.
pub const PHP_BUILTIN_TYPES: &[&str] = &[
    "int", "integer", "string", "bool", "boolean", "float", "double", "array", "void", "mixed",
    "object", "callable", "iterable", "null", "false", "true", "self", "static", "parent", "never",
];

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
