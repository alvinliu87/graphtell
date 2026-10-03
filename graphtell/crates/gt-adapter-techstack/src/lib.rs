//! `gt-adapter-techstack` — cross-language tech-stack knowledge (implementation of the outbound port
//! `MarkerProvider`).
//!
//! Ingest needs to decide a sub-project's language **before** any FKB is loaded (bootstrap), so the
//! "which manifest file marks which language / role" table cannot live in FKB. It used to be a hard-coded
//! `MARKERS` table inside the kernel's `phase::ingest`, which meant adding a language always meant editing
//! the kernel. It now lives here, in an adapter, and the kernel only knows the `MarkerProvider` port.

use gt_domain::model::Language;
use gt_domain::port::{Marker, MarkerProvider};

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
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            ("Cargo.toml", "rust", "backend"),
            ("go.mod", "go", "backend"),
            ("pyproject.toml", "python", "backend"),
        ];
        for (file, lang, role) in expected {
            let m = markers
                .iter()
                .find(|m| m.file == file)
                .unwrap_or_else(|| panic!("marker {file} 缺失"));
            assert_eq!(m.language.as_str(), lang, "marker {file} 的语言不对");
            assert_eq!(m.role, role, "marker {file} 的角色不对");
        }
        assert_eq!(markers.len(), expected.len(), "marker 数量应与预期一致");
    }
}
