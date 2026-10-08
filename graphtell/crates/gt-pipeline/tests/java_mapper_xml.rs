//! MyBatis mapper XML -> pseudo call sites: **tables appear only where MyBatis is actually detected**.
//!
//! This is the regression net for moving the scan off a hard-coded `language == java` check and onto P3's
//! framework detection (`fkb/java/mybatis.yaml`'s detectors). Two things must stay true:
//! * a project that **is** recognised keeps all of its table semantics — these come from nowhere else, since the
//!   mapper XML is not source code and the language parser never touches it;
//! * a project that is **not** recognised must not get them, even with a mapper XML lying in its resources —
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
    int insertOrder(Order o);
    int deleteOrder(Long id);
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
  <insert id="insertOrder">
    insert into {table} (user_id, status) values (#{{userId}}, 1)
  </insert>
  <delete id="deleteOrder">
    delete from {table} where id = #{{id}}
  </delete>
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
/// statement kind: `<select>` -> ReadsDb, `<insert|update|delete>` -> WritesDb. Returns each `(kind, target_table)`
/// so a test can pin both the verb and that it reaches the *right* table (the table name is pulled out of the SQL).
fn db_edges_of(b: &common::Built, method: &str) -> Vec<(String, String)> {
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
        .filter(|e| e.kind.0 == "ReadsDb" || e.kind.0 == "WritesDb")
        .map(|e| {
            let table = b
                .store
                .get_node(e.to_id)
                .expect("node readable")
                .map(|n| n.name)
                .unwrap_or_default();
            (e.kind.0.clone(), table)
        })
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
    // Every mapper statement kind must classify by its verb AND reach the correct table. This pins the full
    // `select -> ReadsDb` and `insert|update|delete -> WritesDb` mapping (the original test only covered `select`
    // + `update`, so a regression dropping `insert` / `delete` from the FKB selector would have slipped through),
    // and proves the table name is really pulled from the SQL rather than synthesised from thin air.
    let reads = db_edges_of(&b, "selectByUser");
    assert!(
        reads.iter().any(|(k, t)| k == "ReadsDb" && t.contains("order")),
        "`<select>` should read the order table, got: {reads:?}"
    );
    for m in ["updateStatus", "insertOrder", "deleteOrder"] {
        let es = db_edges_of(&b, m);
        assert!(
            es.iter().any(|(k, t)| k == "WritesDb" && t.contains("order")),
            "`<{m}>` should write the order table, got: {es:?}"
        );
    }
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
    // None of the four statement kinds may inject a pseudo call site when the framework is unrecognised — the
    // absence must hold for every verb, not just for `select`.
    for m in ["selectByUser", "updateStatus", "insertOrder", "deleteOrder"] {
        assert!(
            db_edges_of(&b, m).is_empty(),
            "no pseudo call site should have been injected for `{m}`"
        );
    }
}

/// Detection must also fire from an **import** (the `import_exists` detector in `mybatis.yaml`), not only from a
/// manifest dependency. The manifest here carries **no** MyBatis dependency, so the only possible signal is the
/// import — which proves detection is genuine framework knowledge and not a hard-coded "has a pom + an xml" heuristic.
/// (The `org.apache.ibatis` import detector is exact-match and effectively unreachable — you cannot import a bare
/// package in Java — so the realistic, reachable import trigger is the MyBatis-Plus prefix detector
/// `com.baomidou.mybatisplus*`, exercised here via `@TableName`'s package. The undetected test above already shows a
/// pom-only, no-import Spring project is *not* recognised, so this test isolates the import path as the trigger.)
#[test]
fn a_mybatis_project_detected_via_import_gets_tables() {
    let dir = mapper_root(
        "via-import",
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        "shop_order",
    );
    // Overwrite the interface to carry a MyBatis-Plus import (the shared helper deliberately omits any import).
    std::fs::write(
        dir.join("src/main/java/demo/OrderMapper.java"),
        r#"package demo;

import com.baomidou.mybatisplus.annotation.TableName;

@TableName("order")
public interface OrderMapper {
    Order selectByUser(Long userId);
    int updateStatus(Long id);
    int insertOrder(Order o);
    int deleteOrder(Long id);
}
"#,
    )
    .expect("rewrite OrderMapper with mybatis-plus import");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    let tables = nodes_of_kind(&b, "Table");
    assert!(
        tables.iter().any(|t| t.name.contains("order")),
        "import-based MyBatis detection must still synthesise the table, got: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    let reads = db_edges_of(&b, "selectByUser");
    assert!(
        reads.iter().any(|(k, t)| k == "ReadsDb" && t.contains("order")),
        "import-detected project must still get a ReadsDb pseudo call site, got: {reads:?}"
    );
}
