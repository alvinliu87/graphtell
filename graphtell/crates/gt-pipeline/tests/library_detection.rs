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

/// Frameworks recognised for the **Java** sub-projects of a full pipeline run.
fn detected_frameworks_java(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("建图");
    b.store
        .list_sub_projects(b.project.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.language.as_str() == "java")
        .flat_map(|s| s.frameworks)
        .collect()
}

/// Frameworks recognised for the PHP sub-project of a full pipeline run.
fn detected_frameworks(root: &Path) -> Vec<String> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("建图");
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
        "代码 `use GuzzleHttp\\Client as G;` 应激活 guzzle 知识，实际识别：{got:?}"
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
        "内联 `\\GuzzleHttp\\Client::request()` 应激活 guzzle 知识，实际识别：{got:?}"
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
        "laravel 的 provides 应激活 illuminate-database，实际识别：{got:?}"
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
        "composer.lock 里的依赖应被识别，实际识别：{got:?}"
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
        "@KafkaListener 应激活 spring-kafka，实际识别：{got:?}"
    );
    assert!(got.contains(&"spring-boot".to_string()), "同时应识别为 spring-boot");
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
        "@Cacheable 应激活 spring-cache，实际识别：{got:?}"
    );
    assert!(got.contains(&"spring-boot".to_string()), "同时应识别为 spring-boot");
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
        "@Table / JpaRepository 应激活 spring-jpa，实际识别：{got:?}"
    );
    assert!(got.contains(&"spring-boot".to_string()), "同时应识别为 spring-boot");

    // The `@Table` rule must actually materialise a `Table` node (gated, so it only fires because spring-jpa
    // was detected above).
    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("建图");
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
    assert!(has_order_table, "JPA @Table 应生成 Table(eb_order) 节点");
}

#[test]
fn mybatis_is_detected_from_the_manifest_alone() {
    // The XML-only style: mapper XML plus a starter dependency, and **no** `org.apache.ibatis` import
    // anywhere. This is the path that goes through `manifest_has`'s textual fallback — `pom.xml` is not
    // JSON, so the JSON branch fails and the plain-text probe is what has to catch it.
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
        "pom.xml 里的 mybatis 依赖应被识别（无 Java import），实际识别：{got:?}"
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
    assert!(got.contains(&"spring-boot".to_string()), "应识别为 spring-boot：{got:?}");
    for lib in ["mybatis", "spring-amqp", "spring-kafka", "spring-cache", "spring-jpa"] {
        assert!(
            !got.contains(&lib.to_string()),
            "{lib} 不该被识别（项目没有用它），实际识别：{got:?}"
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
        "没有用到 Guzzle 的项目不应被识别为 guzzle，实际识别：{got:?}"
    );
}
