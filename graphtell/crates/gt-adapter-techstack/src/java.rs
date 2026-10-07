//! `gt-adapter-techstack` — the Java tech-stack adapter.
//!
//! This adapter only implements what the JVM ecosystem actually has, so FKB's `manifest_dependency`
//! detectors resolve the precise artifactId instead of falling back to a whole-file text probe:
//! * `pom.xml` (Maven) and `build.gradle` / `build.gradle.kts` (Gradle) → dependency names;
//! * `gradle.lockfile` → the resolved dependency closure.
//!
//! What Java does NOT have (and therefore this adapter deliberately leaves at the neutral default):
//! * a namespace manifest — Java packages are inferred from the source tree, like Python modules;
//! * a `return [...]` config file — Spring config lives in annotations + `application.yml`, both already
//!   handled by the parser's annotation literals and FKB, so no `read_manifest` is needed;
//! * a migration-schema loader — JPA entity fields are extracted by the parser's `FieldTypeFact`, so
//!   `Table` / `Column` nodes come from the syntax layer, not from a migration file.

use gt_domain::model::Language;
use gt_domain::port::TechStackAdapter;

/// The Java tech-stack adapter.
pub struct JavaTechStackAdapter;

impl JavaTechStackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JavaTechStackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl TechStackAdapter for JavaTechStackAdapter {
    fn language(&self) -> Language {
        Language::new(Language::JAVA)
    }

    // No other language tag shares the Java parser, so the default `serves` (own language only) is correct.

    fn manifest_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        match file_name {
            "pom.xml" => Some(deps_from_pom(text)),
            "build.gradle" | "build.gradle.kts" => Some(deps_from_gradle(text)),
            _ => None,
        }
    }

    fn lock_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        if file_name == "gradle.lockfile" {
            Some(deps_from_gradle_lockfile(text))
        } else {
            None
        }
    }

    // Spring keeps translations in `messages_{locale}.properties` — locale embedded in the file **name**, not a
    // directory prefix. The `i18n_path_patterns` port only supports a `prefix{locale}suffix` *directory*
    // template, so Java returns nothing rather than guessing a wrong pattern.
    fn i18n_path_patterns(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Every `<artifactId>` inside a `pom.xml`. FKB detectors match by substring (`data-jpa`, `hibernate`),
/// so returning bare artifactIds is enough; we deliberately don't need group:artifact coordinates.
fn deps_from_pom(text: &str) -> Vec<String> {
    let re =
        regex::Regex::new(r#"<artifactId>\s*([^<>\s]+)\s*</artifactId>"#).expect("valid regex");
    re.captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// `implementation 'g:a:v'` / `api "g:a:v"` / bare `'a'` from a Gradle build script.
/// We take the artifact segment (the last `:`-separated piece) so `com.example:lib:1.0` → `lib`.
fn deps_from_gradle(text: &str) -> Vec<String> {
    let re = regex::Regex::new(
        r#"(?i)(?:implementation|api|testImplementation|compileOnly|runtimeOnly|implementationOnly)\s*\(?\s*['"]([^'"]+)['"]"#,
    )
    .expect("valid regex");
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            // `group:artifact:version` → artifact (second-to-last `:` segment); bare `artifact` → itself.
            let parts: Vec<&str> = m.as_str().split(':').collect();
            let name = if parts.len() >= 2 {
                parts[parts.len() - 2]
            } else {
                parts.first().copied().unwrap_or("")
            };
            let name = name.trim();
            if !name.is_empty() && !out.iter().any(|x: &String| x == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// `gradle.lockfile` lines look like `group:name:version=constraint` (or `empty=...`). Take the middle segment.
fn deps_from_gradle_lockfile(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("empty") {
            continue;
        }
        if let Some(name) = line.split(':').nth(1) {
            let name = name.trim();
            if !name.is_empty() && !out.iter().any(|x: &String| x == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_is_java() {
        assert_eq!(JavaTechStackAdapter::new().language().as_str(), "java");
    }

    #[test]
    fn pom_xml_yields_artifact_ids() {
        let pom = r#"
          <dependencies>
            <dependency><groupId>org.springframework.boot</groupId><artifactId>spring-boot-starter-data-jpa</artifactId></dependency>
            <dependency><artifactId>hibernate-core</artifactId></dependency>
          </dependencies>"#;
        let mut deps = JavaTechStackAdapter::new()
            .manifest_dependencies("pom.xml", pom)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"spring-boot-starter-data-jpa".to_string()));
        assert!(deps.contains(&"hibernate-core".to_string()));
        // A non-manifest file must return None so the kernel keeps its text-probe fallback.
        assert!(JavaTechStackAdapter::new()
            .manifest_dependencies("package.json", pom)
            .is_none());
    }

    #[test]
    fn gradle_yields_artifact_names() {
        let g = "dependencies { implementation 'org.springframework.boot:spring-boot-starter-web:3.0' testImplementation \"junit:junit:4.13\" }";
        let mut deps = JavaTechStackAdapter::new()
            .manifest_dependencies("build.gradle", g)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"spring-boot-starter-web".to_string()));
        assert!(deps.contains(&"junit".to_string()));
        // Kotlin DSL uses the same `name("g:a:v")` shape.
        let kts = "dependencies { implementation(\"org.example:lib:1.0\") }";
        let mut k = JavaTechStackAdapter::new()
            .manifest_dependencies("build.gradle.kts", kts)
            .unwrap();
        k.sort();
        assert!(k.contains(&"lib".to_string()));
    }

    #[test]
    fn gradle_lockfile_yields_names() {
        let lock =
            "com.example:lib:1.0=compileClasspath\nempty=\norg.other:tool:2.0=runtimeClasspath\n";
        let mut deps = JavaTechStackAdapter::new()
            .lock_dependencies("gradle.lockfile", lock)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"lib".to_string()));
        assert!(deps.contains(&"tool".to_string()));
        assert!(JavaTechStackAdapter::new()
            .lock_dependencies("pom.xml", lock)
            .is_none());
    }

    #[test]
    fn no_i18n_pattern_for_java() {
        assert!(JavaTechStackAdapter::new().i18n_path_patterns().is_empty());
    }
}
