//! `gt-adapter-techstack` — cross-language tech-stack knowledge.
//!
//! Two pieces of outbound-port knowledge live here, both kept out of the kernel so the prepare phase
//! stays language-agnostic:
//!
//! * `MarkerProvider` — the bootstrap table of "which manifest file marks which language / role". Ingest
//!   must decide a sub-project's language **before** any FKB is loaded, so this cannot live in FKB.
//! * `TechStackAdapter` — one module per language (`php`, `js`, …) turning each ecosystem's concrete
//!   conventions (PSR-4, PHP `return [...]` config, Laravel·ThinkPHP migrations, `package.json` deps, …)
//!   into the kernel's language-agnostic representation. The kernel only knows the `TechStackAdapter` port.

use gt_domain::model::Language;
use gt_domain::port::{Marker, MarkerProvider};

/// The PHP tech-stack adapter (PSR-4 / PHP `return [...]` config / Laravel·ThinkPHP migrations).
pub mod php;
pub use php::PhpTechStackAdapter;

/// The Java tech-stack adapter (Maven `pom.xml` / Gradle `build.gradle` dependency resolution).
pub mod java;
pub use java::JavaTechStackAdapter;

/// The Python tech-stack adapter (requirements / pyproject / Pipfile + Django locale directories).
pub mod python;
pub use python::PythonTechStackAdapter;

/// The Rust tech-stack adapter (Cargo.toml / Cargo.lock dependency resolution).
pub mod rust;
pub use rust::RustTechStackAdapter;

/// The JavaScript / TypeScript tech-stack adapter.
pub mod js;
pub use js::JsTechStackAdapter;

/// The Ruby tech-stack adapter (Bundler / Rails conventions, declared minimally for sub-project detection).
pub mod ruby;
pub use ruby::RubyTechStackAdapter;

/// The .NET tech-stack adapter (NuGet / MSBuild conventions, declared minimally for sub-project detection).
pub mod dotnet;
pub use dotnet::DotnetTechStackAdapter;

/// The default sub-project detection markers, one entry per ecosystem.
pub struct DefaultMarkerProvider;

impl DefaultMarkerProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DefaultMarkerProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkerProvider for DefaultMarkerProvider {
    /// Marker file -> (language, role).
    ///
    /// `role` is the **default** system tier; Ingest may still refine it from the directory name
    /// (e.g. `frontend:admin`), so this is only the bootstrap tier, not the final role.
    fn markers(&self) -> Vec<Marker> {
        vec![
            Marker {
                file: "composer.json".into(),
                language: Language::new(Language::PHP),
                role: "backend".into(),
            },
            Marker {
                file: "package.json".into(),
                language: Language::new(Language::JAVASCRIPT),
                role: "frontend".into(),
            },
            Marker {
                file: "pom.xml".into(),
                language: Language::new(Language::JAVA),
                role: "backend".into(),
            },
            Marker {
                file: "build.gradle".into(),
                language: Language::new(Language::JAVA),
                role: "backend".into(),
            },
            // The Kotlin-DSL spelling. Both the parser (`manifest_files`) and `JavaTechStackAdapter`
            // already accept `build.gradle.kts`; listing only the Groovy spelling left a Kotlin-DSL-only
            // project at language `unknown`, which silently disabled every language-gated step.
            Marker {
                file: "build.gradle.kts".into(),
                language: Language::new(Language::JAVA),
                role: "backend".into(),
            },
            Marker {
                file: "Cargo.toml".into(),
                language: Language::new(Language::RUST),
                role: "backend".into(),
            },
            Marker {
                file: "go.mod".into(),
                language: Language::new("go"),
                role: "backend".into(),
            },
            Marker {
                file: "pyproject.toml".into(),
                language: Language::new(Language::PYTHON),
                role: "backend".into(),
            },
            // A Python project is not required to adopt `pyproject.toml`: most Django / Flask apps in the wild
            // still ship only `requirements.txt` (or `setup.py` / `Pipfile`). Listing only `pyproject.toml`
            // left such sub-projects at language `unknown`, which silently disabled every language-gated step
            // — including the notation rules (`NamespacePolicy`) that P7 needs to split a qualified name.
            Marker {
                file: "requirements.txt".into(),
                language: Language::new(Language::PYTHON),
                role: "backend".into(),
            },
            Marker {
                file: "setup.py".into(),
                language: Language::new(Language::PYTHON),
                role: "backend".into(),
            },
            Marker {
                file: "Pipfile".into(),
                language: Language::new(Language::PYTHON),
                role: "backend".into(),
            },
            // Ruby / Rails: `Gemfile` (and `Gemfile.lock`) are stable basenames; a `.gemspec` is a glob
            // (`*.gemspec`) matched by the `*`-prefix marker convention.
            Marker {
                file: "Gemfile".into(),
                language: Language::new(Language::RUBY),
                role: "backend".into(),
            },
            Marker {
                file: "Gemfile.lock".into(),
                language: Language::new(Language::RUBY),
                role: "backend".into(),
            },
            Marker {
                file: "*.gemspec".into(),
                language: Language::new(Language::RUBY),
                role: "backend".into(),
            },
            // .NET: `*.csproj` / `*.sln` are globs (no stable basename), matched by the `*`-prefix convention.
            Marker {
                file: "*.csproj".into(),
                language: Language::new(Language::CSHARP),
                role: "backend".into(),
            },
            Marker {
                file: "*.sln".into(),
                language: Language::new(Language::CSHARP),
                role: "backend".into(),
            },
        ]
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::port::TechStackAdapter;

    /// `go.mod` / `Cargo.toml` must stay in the table: `gt-pipeline/tests/unsupported_language.rs` pins that a
    /// language which is recognised but has **no parser** still produces a `NoParserForLanguage` diagnostic —
    /// dropping those markers would silently make that test vacuous (and rust/go projects undetected).
    #[test]
    fn covers_parserless_languages_too() {
        let markers = DefaultMarkerProvider::new().markers();
        let lookup = |file: &str| {
            markers
                .iter()
                .find(|m| m.file == file)
                .map(|m| (m.language.as_str().to_string(), m.role.clone()))
        };
        assert_eq!(
            lookup("go.mod"),
            Some(("go".to_string(), "backend".to_string()))
        );
        assert_eq!(
            lookup("Cargo.toml"),
            Some(("rust".to_string(), "backend".to_string()))
        );
    }

    #[test]
    fn maps_each_ecosystem_marker() {
        let markers = DefaultMarkerProvider::new().markers();
        let expected = [
            ("composer.json", "php", "backend"),
            ("package.json", "javascript", "frontend"),
            ("pom.xml", "java", "backend"),
            ("build.gradle", "java", "backend"),
            ("build.gradle.kts", "java", "backend"),
            ("Cargo.toml", "rust", "backend"),
            ("go.mod", "go", "backend"),
            ("pyproject.toml", "python", "backend"),
            // Same language, several ecosystems: a project that ships only `requirements.txt` (or `setup.py` /
            // `Pipfile`) must still be recognised as Python, not left at `unknown`.
            ("requirements.txt", "python", "backend"),
            ("setup.py", "python", "backend"),
            ("Pipfile", "python", "backend"),
            ("Gemfile", "ruby", "backend"),
            ("Gemfile.lock", "ruby", "backend"),
            ("*.gemspec", "ruby", "backend"),
            ("*.csproj", "csharp", "backend"),
            ("*.sln", "csharp", "backend"),
        ];
        for (file, lang, role) in expected {
            let m = markers
                .iter()
                .find(|m| m.file == file)
                .unwrap_or_else(|| panic!("marker {file} is missing"));
            assert_eq!(m.language.as_str(), lang, "marker {file} has the wrong language");
            assert_eq!(m.role, role, "marker {file} has the wrong role");
        }
        assert_eq!(markers.len(), expected.len(), "the marker count must match the expectation");
    }


    /// The two ports in this crate must **agree**: Ingest uses `MarkerProvider` to pick a sub-project's
    /// language, then hands it to a `TechStackAdapter`. A marker whose language no adapter `serves` would
    /// bootstrap a language whose tech-stack conventions are then missing — silently degrading dependency
    /// detection to a whole-file text probe. This cross-consistency was never asserted.
    ///
    /// `go` is the one deliberate exception: it is marked so the language is recognised (and reported as
    /// `NoParserForLanguage`) even though it has neither a parser nor an adapter.
    #[test]
    fn every_marker_language_is_served_by_an_adapter_except_go() {
        let adapters: Vec<Box<dyn TechStackAdapter>> = vec![
            Box::new(PhpTechStackAdapter::new()),
            Box::new(JavaTechStackAdapter::new()),
            Box::new(PythonTechStackAdapter::new()),
            Box::new(RustTechStackAdapter::new()),
            Box::new(JsTechStackAdapter::new()),
            Box::new(RubyTechStackAdapter::new()),
            Box::new(DotnetTechStackAdapter::new()),
        ];
        const PARSERLESS: &[&str] = &["go"];

        for m in DefaultMarkerProvider::new().markers() {
            let lang = m.language.as_str().to_string();
            let served: Vec<String> = adapters
                .iter()
                .filter(|a| a.serves(&m.language))
                .map(|a| a.language().as_str().to_string())
                .collect();
            if PARSERLESS.contains(&lang.as_str()) {
                assert!(served.is_empty(), "`{lang}` is expected to have no adapter: {served:?}");
                continue;
            }
            assert_eq!(
                served,
                vec![lang.clone()],
                "marker `{}` (language `{lang}`) must be served by exactly that language's adapter: {served:?}",
                m.file
            );
        }
    }

    /// One entry per ecosystem: a duplicated marker file makes the lookup ambiguous (first match wins), so a
    /// second entry would be silently dead — or shadow the right one if the order changed.
    #[test]
    fn marker_files_are_unique_and_non_empty() {
        let markers = DefaultMarkerProvider::new().markers();
        let mut files: Vec<&str> = markers.iter().map(|m| m.file.as_str()).collect();
        let n = files.len();
        files.sort();
        files.dedup();
        assert_eq!(
            files.len(),
            n,
            "every marker file must appear exactly once: {:?}",
            markers.iter().map(|m| &m.file).collect::<Vec<_>>()
        );
        assert!(
            markers.iter().all(|m| !m.file.is_empty() && !m.role.is_empty()),
            "no marker may carry an empty file name or role"
        );
    }

    /// The `Default` impls must equal `new()` so callers can use either form.
    #[test]
    fn defaults_equal_new() {
        assert_eq!(
            DefaultMarkerProvider::default().markers().len(),
            DefaultMarkerProvider::new().markers().len()
        );
        assert_eq!(
            JsTechStackAdapter::default().language().as_str(),
            Language::JAVASCRIPT
        );
    }
}
