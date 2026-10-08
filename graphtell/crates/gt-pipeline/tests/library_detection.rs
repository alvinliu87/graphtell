//! Self-check of the three ways a **library** (not a framework) gets recognised.
//!
//! The recurring problem: a project's own manifest names only what it asked for directly. A Laravel app's
//! `composer.json` mentions `laravel/framework` and nothing else, so every `illuminate/*` component its code
//! uses is invisible to `manifest_dependency`. Three answers, in increasing order of indirectness:
//!
//! * `import_exists` / `call_exists` — read what the **code** does (P2 evidence);
//! * `lock_dependency` — read the **resolved closure** (`composer.lock`), which does list transitive packages;
//! * `provides` — the framework declares the bundle, so recognising it switches the components on directly.
//!
//! Every case here runs the **full pipeline** (P0 → P2 → P3), so they also cover what a unit test cannot:
//! that the evidence P2 extracts from the source, and the `provides` graph, actually reach P3.

mod common;

use std::path::Path;

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{GraphQuery, NodeFilter, ProjectReader};

const BARE_COMPOSER: &str = r#"{ "name": "demo/app", "require": { "php": ">=8.1" } }"#;

/// A one-file PHP project: an explicit manifest, an optional lock file, and one source file.
fn project(tag: &str, composer: &str, lock: Option<&str>, php: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-lib-detect-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/Service")).expect("mkdir");
    std::fs::write(dir.join("composer.json"), composer).expect("write composer.json");
    if let Some(l) = lock {
        std::fs::write(dir.join("composer.lock"), l).expect("write composer.lock");
    }
    std::fs::write(dir.join("src/Service/Notifier.php"), php).expect("write php");
    dir
}

/// A one-file PHP project whose manifest mentions nothing but `php`.
fn synthetic_root(tag: &str, php: &str) -> std::path::PathBuf {
    project(tag, BARE_COMPOSER, None, php)
}

/// A Java project: a `pom.xml` plus any number of source files, as `(relative path, contents)`.
fn java_project(tag: &str, pom: &str, sources: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-lib-detect-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("pom.xml"), pom).expect("write pom.xml");
    for (path, body) in sources {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
        std::fs::write(full, body).expect("write source");
    }
    dir
}

/// A **Gradle** Java project: one or more build scripts (`build.gradle`, `build.gradle.kts`) and sources,
/// but **no `pom.xml`** — the build system every other Java fixture here lacks.
fn gradle_project(tag: &str, builds: &[(&str, &str)], sources: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-lib-detect-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    for (name, body) in builds {
        std::fs::write(dir.join(name), body).expect("write build script");
    }
    for (path, body) in sources {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
        std::fs::write(full, body).expect("write source");
    }
    dir
}

/// The languages a full pipeline run decided for the sub-projects of `root`.
fn detected_languages(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    b.store
        .list_sub_projects(b.project.id)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.language.as_str().to_string())
        .collect()
}

/// Frameworks recognised for the **Java** sub-projects of a full pipeline run.
fn detected_frameworks_java(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    b.store
        .list_sub_projects(b.project.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.language.as_str() == "java")
        .flat_map(|s| s.frameworks)
        .collect()
}

/// A JavaScript project: a `package.json` plus any number of source files.
fn js_project(tag: &str, package_json: &str, sources: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-lib-detect-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("package.json"), package_json).expect("write package.json");
    for (path, body) in sources {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
        std::fs::write(full, body).expect("write source");
    }
    dir
}

/// A synthetic project laid out as `(relative path, contents)` pairs — used by the Python / Rust cases,
/// which only need a manifest plus one source file each.
fn project_files(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-lib-detect-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    for (path, body) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
        std::fs::write(full, body).expect("write");
    }
    dir
}

/// Frameworks recognised for the **Rust** sub-project of a full pipeline run.
fn detected_frameworks_rust(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    b.store
        .list_sub_projects(b.project.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.language.as_str() == "rust")
        .flat_map(|s| s.frameworks)
        .collect()
}

/// Frameworks recognised for the **Python** sub-project of a full pipeline run.
fn detected_frameworks_python(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    b.store
        .list_sub_projects(b.project.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.language.as_str() == "python")
        .flat_map(|s| s.frameworks)
        .collect()
}

/// Frameworks recognised for the **JavaScript** sub-project of a full pipeline run.
fn detected_frameworks_js(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    b.store
        .list_sub_projects(b.project.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.language.as_str() == "javascript")
        .flat_map(|s| s.frameworks)
        .collect()
}

/// Frameworks recognised for the PHP sub-project of a full pipeline run.
fn detected_frameworks(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    b.store
        .list_sub_projects(b.project.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.language.as_str() == "php")
        .flat_map(|s| s.frameworks)
        .collect()
}

#[test]
fn import_exists_detects_the_library_through_an_alias() {
    // `as G` is the case a manifest-only detector fails hardest at: the local name has nothing in common
    // with the package, yet the recorded FQN is identical.
    let root = synthetic_root(
        "alias",
        r#"<?php
namespace App\Service;

use GuzzleHttp\Client as G;

class Notifier
{
    public function send(string $url): void
    {
        $c = new G();
        $c->request('GET', $url);
    }
}
"#,
    );
    let got = detected_frameworks(&root);
    assert!(
        got.contains(&"guzzle".to_string()),
        "the code `use GuzzleHttp\\Client as G;` must activate the guzzle knowledge, recognised: {got:?}"
    );
}

#[test]
fn call_exists_detects_the_inline_fully_qualified_call() {
    // No `use` at all — the import table is empty, so only the call-site detector can see this.
    let root = synthetic_root(
        "inline",
        r#"<?php
namespace App\Service;

class Notifier
{
    public function send(string $url): void
    {
        \GuzzleHttp\Client::request('GET', $url);
    }
}
"#,
    );
    let got = detected_frameworks(&root);
    assert!(
        got.contains(&"guzzle".to_string()),
        "the inline `\\GuzzleHttp\\Client::request()` must activate the guzzle knowledge, recognised: {got:?}"
    );
}

#[test]
fn provides_switches_on_a_component_the_manifest_never_names() {
    // The case `manifest_dependency` structurally cannot reach: an app requires `laravel/framework`, and
    // `illuminate/database` appears in no file the app wrote. Only the framework's bundle declaration can
    // switch it on.
    let root = project(
        "provides",
        r#"{ "name": "demo/app", "require": { "laravel/framework": "^11.0" } }"#,
        None,
        r#"<?php
namespace App\Service;

use Illuminate\Support\Facades\DB;

class Notifier
{
    public function run(): void
    {
        DB::transaction(function () {
        });
    }
}
"#,
    );
    let got = detected_frameworks(&root);
    assert!(
        got.contains(&"illuminate-database".to_string()),
        "laravel's provides must activate illuminate-database, recognised: {got:?}"
    );
}

#[test]
fn lock_dependency_sees_a_package_only_the_lock_file_names() {
    // No laravel/framework here at all — so `provides` is not what does the work; the resolved closure is.
    let root = project(
        "lock",
        BARE_COMPOSER,
        Some(r#"{"packages": [{"name": "illuminate/database", "version": "11.0"}]}"#),
        r#"<?php
namespace App\Service;

class Notifier
{
    public function run(): void
    {
    }
}
"#,
    );
    let got = detected_frameworks(&root);
    assert!(
        got.contains(&"illuminate-database".to_string()),
        "a dependency in composer.lock must be recognised, recognised: {got:?}"
    );
}

/// A `pom.xml` with only Spring Boot — no broker, no ORM.
const POM_SPRING_ONLY: &str = r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
  </dependencies>
</project>
"#;

#[test]
fn kafka_is_detected_from_its_annotation() {
    // Why this exists: **no sample in `samples/java-projects` uses Kafka**, so on real data
    // `spring-kafka` is `detected=false` everywhere — which is also exactly what a broken detector looks
    // like. The corpus cannot tell those apart, so the positive case has to be synthetic.
    let root = java_project(
        "kafka",
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
    <dependency>
      <groupId>org.springframework.kafka</groupId>
      <artifactId>spring-kafka</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        &[(
            "src/main/java/demo/OrderListener.java",
            r#"package demo;

import org.springframework.kafka.annotation.KafkaListener;

public class OrderListener {
    @KafkaListener(topics = "orders")
    public void onMessage(String body) {
    }
}
"#,
        )],
    );
    let got = detected_frameworks_java(&root);
    assert!(
        got.contains(&"spring-kafka".to_string()),
        "@KafkaListener must activate spring-kafka, recognised: {got:?}"
    );
    assert!(got.contains(&"spring-boot".to_string()), "it must be recognised as spring-boot as well");
}

#[test]
fn spring_cache_is_detected_from_its_annotation() {
    // Like Kafka, no sample under `samples/java-projects` uses Spring Cache, so the positive case must be
    // synthetic. The precise detector is the `org.springframework.cache.annotation.Cacheable` import; the
    // `@Cacheable` annotation itself is the call site that the rule keys off.
    let root = java_project(
        "spring-cache",
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        &[(
            "src/main/java/demo/ProductService.java",
            r#"package demo;

import org.springframework.cache.annotation.Cacheable;

public class ProductService {
    @Cacheable("products")
    public String get(long id) {
        return "p";
    }
}
"#,
        )],
    );
    let got = detected_frameworks_java(&root);
    assert!(
        got.contains(&"spring-cache".to_string()),
        "@Cacheable must activate spring-cache, recognised: {got:?}"
    );
    assert!(got.contains(&"spring-boot".to_string()), "it must be recognised as spring-boot as well");
}

#[test]
fn spring_jpa_is_detected_from_its_annotation() {
    // Like Kafka / Spring Cache, no sample under `samples/java-projects` uses JPA, so the positive case must
    // be synthetic. The precise detector is the `org.springframework.data.jpa.repository.JpaRepository` import;
    // the `@Table` annotation is the call site the rule keys off.
    let root = java_project(
        "spring-jpa",
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        &[
            (
                "src/main/java/demo/Order.java",
                r#"package demo;

import jakarta.persistence.Table;

@Table(name = "eb_order")
public class Order {
    private Long id;
}
"#,
            ),
            (
                "src/main/java/demo/OrderRepository.java",
                r#"package demo;

import org.springframework.data.jpa.repository.JpaRepository;

public interface OrderRepository extends JpaRepository<Order, Long> {
}
"#,
            ),
        ],
    );
    let got = detected_frameworks_java(&root);
    assert!(
        got.contains(&"spring-jpa".to_string()),
        "@Table / JpaRepository must activate spring-jpa, recognised: {got:?}"
    );
    assert!(got.contains(&"spring-boot".to_string()), "it must be recognised as spring-boot as well");

    // The `@Table` rule must actually materialise a `Table` node (gated, so it only fires because spring-jpa
    // was detected above).
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("graphing");
    let has_order_table = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Table".to_string())),
            name_contains: Some("order".to_string()),
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .into_iter()
        .any(|t| t.name.to_ascii_lowercase().contains("order"));
    assert!(has_order_table, "a JPA @Table must produce the Table(eb_order) node");
}

#[test]
fn spring_amqp_is_detected_from_its_annotation() {
    // Like Kafka / Spring Cache / Spring JPA, no sample under `samples/java-projects` uses Spring AMQP, so the
    // positive case must be synthetic. The precise detector is the
    // `org.springframework.amqp.rabbit.annotation.RabbitListener` import; the `@RabbitListener` annotation is the
    // call site the rule keys off. This is the fourth Java annotation library and closes the gap left by
    // `a_spring_project_without_those_libraries_gets_neither`, which lists `spring-amqp` in its *negative* set —
    // without this positive pin, a broken AMQP FKB would still pass every test (the negative only proves it is
    // absent when the code is silent, never that it is present when it should be).
    let root = java_project(
        "spring-amqp",
        POM_SPRING_ONLY,
        &[(
            "src/main/java/demo/OrderConsumer.java",
            r#"package demo;

import org.springframework.amqp.rabbit.annotation.RabbitListener;

public class OrderConsumer {
    @RabbitListener(queues = "orders")
    public void onMessage(String body) {
    }
}
"#,
        )],
    );
    let got = detected_frameworks_java(&root);
    assert!(
        got.contains(&"spring-amqp".to_string()),
        "@RabbitListener must activate spring-amqp, recognised: {got:?}"
    );
    assert!(got.contains(&"spring-boot".to_string()), "it must be recognised as spring-boot as well");
}

#[test]
fn mybatis_is_detected_from_the_manifest_alone() {
    // The XML-only style: mapper XML plus a starter dependency, and **no** `org.apache.ibatis` import
    // anywhere. `pom.xml` is not JSON, so the JSON branch fails; the Java tech-stack adapter's
    // `<artifactId>` extraction is what catches it (and the plain-text probe behind it).
    let root = java_project(
        "mybatis-xml",
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.mybatis.spring.boot</groupId>
      <artifactId>mybatis-spring-boot-starter</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        &[(
            "src/main/java/demo/OrderMapper.java",
            r#"package demo;

public interface OrderMapper {
    int insert(int id);
}
"#,
        )],
    );
    let got = detected_frameworks_java(&root);
    assert!(
        got.contains(&"mybatis".to_string()),
        "the mybatis dependency in pom.xml must be recognised (with no Java import), recognised: {got:?}"
    );
}

/// The **Gradle** half of the Java stack, end to end. Every other Java fixture here is Maven (`pom.xml`),
/// so `build.gradle` was previously parsed by nothing outside `gt-adapter-techstack`'s unit tests —
/// meaning a Gradle project could silently lose its sub-project language and every manifest detector.
#[test]
fn gradle_project_is_recognised_as_java_and_spring_boot() {
    let root = gradle_project(
        "gradle-groovy",
        &[(
            "build.gradle",
            "plugins { id 'java' }\n\
             dependencies {\n    implementation 'org.springframework.boot:spring-boot-starter-web:3.2.0'\n}\n",
        )],
        &[(
            "src/main/java/demo/OrderController.java",
            r#"package demo;

public class OrderController {
    public String list() {
        return "ok";
    }
}
"#,
        )],
    );

    let langs = detected_languages(&root);
    assert!(
        langs.contains(&"java".to_string()),
        "build.gradle is a marker, so the sub-project must be java: {langs:?}"
    );
    let got = detected_frameworks_java(&root);
    assert!(
        got.contains(&"spring-boot".to_string()),
        "a Gradle project must be recognised as spring-boot too: {got:?}"
    );
}

/// The Kotlin-DSL spelling, `build.gradle.kts`: the parser and the tech-stack adapter both list it, so a
/// project that ships only that file must end up with the same sub-project language.
#[test]
fn gradle_kotlin_dsl_project_is_recognised_as_java() {
    let root = gradle_project(
        "gradle-kts",
        &[(
            "build.gradle.kts",
            "plugins { java }\n\
             dependencies {\n    implementation(\"org.springframework.boot:spring-boot-starter-web:3.2.0\")\n}\n",
        )],
        &[(
            "src/main/java/demo/OrderController.java",
            r#"package demo;

public class OrderController {
    public String list() {
        return "ok";
    }
}
"#,
        )],
    );

    let langs = detected_languages(&root);
    assert!(
        langs.contains(&"java".to_string()),
        "build.gradle.kts must mark the sub-project as java: {langs:?}"
    );
}

#[test]
fn a_spring_project_without_those_libraries_gets_neither() {
    let root = java_project(
        "spring-only",
        POM_SPRING_ONLY,
        &[(
            "src/main/java/demo/OrderController.java",
            r#"package demo;

public class OrderController {
    public String list() {
        return "ok";
    }
}
"#,
        )],
    );
    let got = detected_frameworks_java(&root);
    assert!(got.contains(&"spring-boot".to_string()), "it must be recognised as spring-boot: {got:?}");
    for lib in ["mybatis", "spring-amqp", "spring-kafka", "spring-cache", "spring-jpa"] {
        assert!(
            !got.contains(&lib.to_string()),
            "{lib} must not be recognised (the project does not use it), recognised: {got:?}"
        );
    }
}

/// The JS counterpart of `mybatis_is_detected_from_the_manifest_alone`: a `package.json` dependency
/// activates the framework knowledge. Which sections hold dependencies — and that a **scoped** name keeps
/// its scope (`@nestjs/core`) — is the JS adapter's job, not the kernel's.
///
/// No test asserted `SubProject.frameworks` for JavaScript before: the node tests only assert graph
/// structure (HttpContract / HandledBy), which the knowledge could produce through some other detector, so
/// a broken `manifest_dependency` path for JS would have gone unnoticed.
#[test]
fn js_manifest_dependency_activates_the_framework() {
    let root = js_project(
        "js-manifest",
        r#"{
  "name": "demo",
  "dependencies": { "@nestjs/core": "^10.0.0", "express": "^4.18.0" },
  "devDependencies": { "typeorm": "^0.3.0" }
}"#,
        &[("src/app.js", "const app = require('express')();\n")],
    );
    let got = detected_frameworks_js(&root);
    assert!(
        got.contains(&"nestjs".to_string()),
        "a scoped `@nestjs/core` dependency must activate nestjs, recognised: {got:?}"
    );
    assert!(
        got.contains(&"express".to_string()),
        "express must be recognised, recognised: {got:?}"
    );
    assert!(
        got.contains(&"typeorm".to_string()),
        "devDependencies count as declared dependencies, recognised: {got:?}"
    );
}

/// Rust: `manifest_dependency` reads `Cargo.toml`'s dependency tables through the **Rust adapter**
/// (`serde = "1.0"` / `tokio = { version = "1" }` -> bare crate name). Nothing asserted
/// `SubProject.frameworks` for Rust before — `rust_sample.rs` only pins HttpContract / Database nodes,
/// which the FKB could produce through some other detector, so a broken manifest path for Rust would
/// have gone unnoticed.
///
/// Note: unlike the Python case below, this cannot isolate the adapter from the text-probe fallback
/// (`declared_in` always falls back to a whole-file `text.contains`), because Rust crate names are
/// lowercase by convention — there is no realistic capitalised spelling to exploit. It pins the
/// end-to-end detection instead.
#[test]
fn rust_manifest_dependency_activates_the_framework() {
    let root = project_files(
        "rs-manifest",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nactix-web = \"4\"\n\
                 sqlx = { version = \"0.7\", features = [\"postgres\"] }\n",
            ),
            ("src/main.rs", "fn main() {}\n"),
        ],
    );
    let got = detected_frameworks_rust(&root);
    assert!(
        got.contains(&"actix-web".to_string()),
        "actix-web must be recognised from Cargo.toml, recognised: {got:?}"
    );
    assert!(
        got.contains(&"sqlx".to_string()),
        "`sqlx = {{ version = ... }}` (inline table) must be recognised too, recognised: {got:?}"
    );
}

/// The gate must stay shut for Rust: a project that declares neither of them must not get their knowledge.
#[test]
fn rust_project_without_the_library_does_not_get_it() {
    let root = project_files(
        "rs-absent",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
            ),
            ("src/main.rs", "fn main() {}\n"),
        ],
    );
    let got = detected_frameworks_rust(&root);
    for lib in ["actix-web", "axum", "rocket", "sqlx", "diesel", "rusqlite"] {
        assert!(
            !got.contains(&lib.to_string()),
            "{lib} must not be recognised (the project declares only serde), recognised: {got:?}"
        );
    }
}

/// Python: `manifest_dependency` reads `requirements.txt` / `pyproject.toml` through the **Python adapter**
/// (bare package names, version constraints stripped — `django==5.0` -> `django`). No Python test asserted
/// `SubProject.frameworks` before: they only assert graph structure, which the knowledge could produce through
/// another detector, so a broken manifest path for Python would have gone unnoticed.
#[test]
fn python_manifest_dependency_activates_the_framework() {
    let req = project_files(
        "py-req",
        &[
            ("requirements.txt", "django==5.0\ncelery==5.3\n# comment\n"),
            ("app/views.py", "from django.db import models\n"),
        ],
    );
    let got = detected_frameworks_python(&req);
    assert!(
        got.contains(&"django".to_string()),
        "`django==5.0` must activate django (version constraint stripped), recognised: {got:?}"
    );
    assert!(
        got.contains(&"celery".to_string()),
        "celery must be recognised, recognised: {got:?}"
    );

    // The same through PEP 621 metadata: `[project] dependencies = [...]`.
    let toml = project_files(
        "py-toml",
        &[
            (
                "pyproject.toml",
                "[project]\nname = \"demo\"\ndependencies = [\"fastapi>=0.100\", \"uvicorn\"]\n",
            ),
            ("app/main.py", "from fastapi import FastAPI\n"),
        ],
    );
    let got = detected_frameworks_python(&toml);
    assert!(
        got.contains(&"fastapi".to_string()),
        "pyproject.toml dependencies must activate fastapi, recognised: {got:?}"
    );

    // The one case the text-probe fallback **cannot** cover, so it is the only thing in this file that
    // actually proves the Python adapter is wired in: `declared_in`'s fallback is a case-sensitive
    // `text.contains(dependency)`, while the adapter compares the extracted names case-insensitively.
    // `Django==5.0` therefore only matches through `PythonTechStackAdapter`.
    let cased = project_files(
        "py-case",
        &[("requirements.txt", "Django==5.0\n"), ("app/views.py", "X = 1\n")],
    );
    let got = detected_frameworks_python(&cased);
    assert!(
        got.contains(&"django".to_string()),
        "a capitalised `Django==5.0` must still activate django — only the adapter's case-insensitive \
         name matching can do that, the whole-file text probe is case-sensitive: {got:?}"
    );
}

/// The gate must stay shut for Python too: a project that declares only flask must not get django / celery /
/// fastapi knowledge.
#[test]
fn python_project_without_the_library_does_not_get_it() {
    let root = project_files(
        "py-absent",
        &[
            ("requirements.txt", "flask==3.0\n"),
            ("app/app.py", "from flask import Flask\n"),
        ],
    );
    let got = detected_frameworks_python(&root);
    assert!(
        got.contains(&"flask".to_string()),
        "flask must still be recognised, recognised: {got:?}"
    );
    for lib in ["django", "celery", "fastapi", "sqlalchemy", "redis"] {
        assert!(
            !got.contains(&lib.to_string()),
            "{lib} must not be recognised (the project does not declare it), recognised: {got:?}"
        );
    }
}

/// The other half: a project that never declares the library must not get its knowledge — the gate has to
/// stay shut for JS exactly as it does for PHP / Java.
#[test]
fn js_project_without_the_library_does_not_get_it() {
    let root = js_project(
        "js-absent",
        r#"{ "name": "demo", "dependencies": { "express": "^4.18.0" } }"#,
        &[("src/app.js", "const app = require('express')();\n")],
    );
    let got = detected_frameworks_js(&root);
    assert!(
        got.contains(&"express".to_string()),
        "express must still be recognised, recognised: {got:?}"
    );
    for lib in ["nestjs", "typeorm", "koa", "fastify"] {
        assert!(
            !got.contains(&lib.to_string()),
            "{lib} must not be recognised (the project does not declare it), recognised: {got:?}"
        );
    }
}

#[test]
fn a_project_that_never_uses_the_library_does_not_get_it() {
    // The other half of the contract: detection is a gate, so it must stay shut when the code is silent.
    // Without this, the two detectors above would pass equally well with the gate removed.
    let root = synthetic_root(
        "absent",
        r#"<?php
namespace App\Service;

use App\Support\Mailer;

class Notifier
{
    public function send(string $url): void
    {
        $m = new Mailer();
        $m->request('GET', $url);
    }
}
"#,
    );
    let got = detected_frameworks(&root);
    assert!(
        !got.contains(&"guzzle".to_string()),
        "a project that does not use Guzzle must not be recognised as guzzle, recognised: {got:?}"
    );
}
