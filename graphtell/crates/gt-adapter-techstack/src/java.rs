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

    /// `serves` is not overridden here — the source comment at the impl states that is deliberate ("no other
    /// language tag shares the Java parser"). That claim was never asserted: a JVM-ecosystem tag must NOT be
    /// served by this adapter, or a Kotlin / Scala / Groovy file would silently pick up Maven / Gradle rules.
    #[test]
    fn serves_only_the_java_language_tag() {
        let a = JavaTechStackAdapter::new();
        assert!(a.serves(&Language::new(Language::JAVA)), "java must be served");
        for other in ["kotlin", "scala", "groovy", "typescript", ""] {
            assert!(!a.serves(&Language::new(other)), "`{other}` must not be served by the Java adapter");
        }
    }

    /// The documented shapes: a bare artifact (`implementation 'junit'`) keeps its own name, and the same
    /// artifact declared in two configurations collapses to one entry (the dedup contract).
    #[test]
    fn gradle_dedupes_and_handles_a_bare_artifact() {
        assert_eq!(deps_from_gradle("implementation 'junit'"), vec!["junit".to_string()], "a bare artifact keeps its own name");
        assert_eq!(
            deps_from_gradle("implementation 'a:b:1'\ntestImplementation \"a:b:2\""),
            vec!["b".to_string()],
            "one artifact declared in two configurations must be reported once"
        );
    }

    /// Coordinate shapes, pinned as-is. `deps_from_gradle` takes `parts[parts.len() - 2]`, which is the
    /// artifact **only for the 3-segment `group:artifact:version` form**:
    /// * `g:a:v`            -> `a`  (correct)
    /// * `org.example:lib`  -> `org.example`  — the **group**, though the doc comment promises the artifact
    /// * `g:a:jar:1.0`      -> `jar`          — the classifier, same root cause
    ///
    /// The artifact of a Gradle coordinate is the segment at index 1 in every form above, so
    /// `parts.get(1)` (falling back to `parts[0]` for a bare name) would match the documented intent.
    /// Pinned rather than fixed; if it is corrected, these two assertions are the ones to update.
    #[test]
    fn gradle_coordinate_shapes_are_pinned_as_is() {
        assert_eq!(
            deps_from_gradle("implementation 'com.example:lib:1.0'"),
            vec!["lib".to_string()],
            "the 3-segment form is what the code is right about"
        );
        assert_eq!(
            deps_from_gradle("implementation 'org.example:lib'"),
            vec!["org.example".to_string()],
            "pinned: a 2-segment coordinate currently yields the group, not the artifact"
        );
        assert_eq!(
            deps_from_gradle("implementation 'g:a:jar:1.0'"),
            vec!["jar".to_string()],
            "pinned: a 4-segment coordinate currently yields the classifier"
        );
    }

    /// `deps_from_pom` is deliberately broad — FKB detectors match by substring (`data-jpa`, `hibernate`), so
    /// every `<artifactId>` counts, including the project's own and plugin ones. It is also the one extractor
    /// that does **not** dedupe (unlike the Gradle paths).
    ///
    /// Also pinned: `manifest_dependencies("pom.xml", "")` is `Some(empty)`, not `None` — "recognised and
    /// empty" must not fall back to the kernel's plain-text probe.
    #[test]
    fn pom_extraction_is_broad_keeps_duplicates_and_never_falls_back() {
        let pom = r#"<project><artifactId>my-app</artifactId>
          <dependency><artifactId>hibernate-core</artifactId></dependency>
          <build><plugin><artifactId>maven-surefire</artifactId></plugin></build></project>"#;
        let deps = deps_from_pom(pom);
        assert!(deps.contains(&"my-app".to_string()), "the project's own artifactId is included: {deps:?}");
        assert!(deps.contains(&"maven-surefire".to_string()), "plugin artifactIds are included: {deps:?}");
        assert!(deps.contains(&"hibernate-core".to_string()));

        assert_eq!(
            deps_from_pom("<artifactId>x</artifactId><artifactId>x</artifactId>"),
            vec!["x".to_string(), "x".to_string()],
            "unlike the Gradle paths, the POM path does not dedupe"
        );

        let a = JavaTechStackAdapter::new();
        assert_eq!(
            a.manifest_dependencies("pom.xml", ""),
            Some(Vec::new()),
            "an empty but recognised manifest returns Some(empty), not None (None would trigger the text probe)"
        );
    }

    /// Lock-file lines are `group:name:version=constraint`: a line with no `:` carries no artifact and must be
    /// skipped, and the same artifact pinned twice collapses to one entry.
    #[test]
    fn gradle_lockfile_skips_malformed_lines_and_dedupes() {
        let lock = "nocolonline\ncom.example:lib:1.0=r\ncom.example:lib:2.0=r\n\n";
        assert_eq!(
            deps_from_gradle_lockfile(lock),
            vec!["lib".to_string()],
            "a line without a `:` is skipped and a repeated artifact is reported once"
        );
    }

    #[test]
    fn no_i18n_pattern_for_java() {
        assert!(JavaTechStackAdapter::new().i18n_path_patterns().is_empty());
    }
}
