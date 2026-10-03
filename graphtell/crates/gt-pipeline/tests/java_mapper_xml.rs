//! MyBatis mapper XML -> pseudo call sites: **tables appear only where MyBatis is actually detected**.
//!
//! This is the regression net for moving the scan off a hard-coded `language == java` check and onto P3's
//! framework detection (`fkb/java/mybatis.yaml`'s detectors). Two things must stay true:
//! * a project that **is** recognised keeps all of its table semantics — these come from nowhere else, since the
//!   mapper XML is not source code and the language parser never touches it;
//! * a project that is **not** recognised no longer gets them, even with a mapper XML lying in its resources —
//!   which is exactly the point: whether a library applies is knowledge, not something the kernel decides.

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// A Spring Boot-ish Java project: one Mapper interface plus one mapper XML reading / writing `table`.
///
/// `pom_xml` is what decides detection: the starter dependency is one of `mybatis.yaml`'s detectors, and the Java
/// sources carry **no** `org.apache.ibatis` import on purpose, so recognition can only come from the manifest.
fn mapper_root(name: &str, pom_xml: &str, table: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-mapper-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/main/java/demo")).expect("mkdir java");
    std::fs::create_dir_all(dir.join("src/main/resources/mapper")).expect("mkdir resources");

    std::fs::write(dir.join("pom.xml"), pom_xml).expect("write pom.xml");

    std::fs::write(
        dir.join("src/main/java/demo/OrderMapper.java"),
        r#"package demo;

public interface OrderMapper {
    Order selectByUser(Long userId);
    int updateStatus(Long id);
}
"#,
    )
    .expect("write OrderMapper");

    std::fs::write(
        dir.join("src/main/resources/mapper/OrderMapper.xml"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" ?>
<mapper namespace="demo.OrderMapper">
  <select id="selectByUser" resultType="demo.Order">
    select * from {table} where user_id = #{{userId}}
  </select>
  <update id="updateStatus">
    update {table} set status = 1 where id = #{{id}}
  </update>
</mapper>
"#
        ),
    )
    .expect("write OrderMapper.xml");

    dir
}

fn nodes_of_kind(b: &common::Built, kind: &str) -> Vec<Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
}

/// The pseudo call site is owned by the **interface method**, whose read / write classification comes from the
/// statement kind: `<select>` -> ReadsDb, `<update>` -> WritesDb.
fn db_edges_of(b: &common::Built, method: &str) -> Vec<String> {
    let Some(m) = nodes_of_kind(b, "Method")
        .into_iter()
        .find(|n| n.name == method)
    else {
        return Vec::new();
    };
    b.store
        .edges_of(m.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .filter(|k| k == "ReadsDb" || k == "WritesDb")
        .collect()
}

#[test]
fn a_detected_mybatis_project_gets_tables_and_db_verbs_from_its_mappers() {
    let dir = mapper_root(
        "detected",
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.mybatis.spring.boot</groupId>
      <artifactId>mybatis-spring-boot-starter</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        "shop_order",
    );
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    let tables = nodes_of_kind(&b, "Table");
    assert!(
        tables.iter().any(|t| t.name.contains("order")),
        "the mapper's table should be synthesised, got: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert_eq!(
        db_edges_of(&b, "selectByUser"),
        vec!["ReadsDb".to_string()],
        "`<select>` should read the table"
    );
    assert_eq!(
        db_edges_of(&b, "updateStatus"),
        vec!["WritesDb".to_string()],
        "`<update>` should write the table"
    );
}

#[test]
fn an_undetected_projects_mappers_are_never_scanned() {
    // Identical layout, minus any MyBatis signal: no starter in `pom.xml`, no `org.apache.ibatis` import. The XML
    // is right there in `resources/mapper`, and it must still contribute nothing — the old kernel asked only
    // "is this Java?", which walked every Spring project looking for `<mapper`.
    let dir = mapper_root(
        "undetected",
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        "shop_gift_card",
    );
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    let tables = nodes_of_kind(&b, "Table");
    assert!(
        !tables.iter().any(|t| t.name.contains("gift")),
        "a project not recognised as MyBatis must get no table from its mapper XML, got: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert!(
        db_edges_of(&b, "selectByUser").is_empty(),
        "no pseudo call site should have been injected"
    );
}
