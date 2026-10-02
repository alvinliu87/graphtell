//! End-to-end self-check of Java (Spring Boot) **DAO -> table** and **read / write verb classification**.
//!
//! Covers this chain: `interface UserRepository extends JpaRepository<User, Long>`
//!   -> the generic argument `User` (the entity the DAO operates on; the parser records it as a `generic.JpaRepository`
//!   synthetic call site)
//!   -> `References` (DAO -> entity, an FKB `Link`)
//!   -> `Project` projects along the entity's `MapsTo` into `MapsTo` (DAO -> table)
//!   -> P7 then turns `repo.save()` / `repo.findById()` into `WritesDb` / `ReadsDb`.

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_spring_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-java-dbverbs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/main/java/com/demo")).expect("mkdir");

    std::fs::write(
        dir.join("pom.xml"),
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-data-jpa</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
    )
    .expect("write pom.xml");

    std::fs::write(
        dir.join("src/main/java/com/demo/User.java"),
        r#"package com.demo;

import com.baomidou.mybatisplus.annotation.TableName;

@TableName("eb_user")
public class User {
    private Long id;
}
"#,
    )
    .expect("write User");

    std::fs::write(
        dir.join("src/main/java/com/demo/UserRepository.java"),
        r#"package com.demo;

import org.springframework.data.jpa.repository.JpaRepository;

public interface UserRepository extends JpaRepository<User, Long> {
    User findById(Long id);
    User save(User u);
}
"#,
    )
    .expect("write UserRepository");

    std::fs::write(
        dir.join("src/main/java/com/demo/UserService.java"),
        r#"package com.demo;

import org.springframework.beans.factory.annotation.Autowired;
import java.util.List;

public class UserService {
    @Autowired
    private UserRepository repo;

    public void run(List<Long> ids) {
        for (Long id : ids) {
            repo.findById(id);
            repo.save(new User());
        }
    }
}
"#,
    )
    .expect("write UserService");

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

#[test]
fn jpa_repository_maps_to_table_and_db_verbs_are_classified() {
    let dir = synthetic_spring_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    let tables = nodes_of_kind(&b, "Table");
    assert!(
        tables.iter().any(|t| t.name.contains("user")),
        "expected a user table to be synthesised, got: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let Some(repo) = nodes_of_kind(&b, "Interface")
        .into_iter()
        .find(|n| n.name == "UserRepository")
    else {
        panic!("expected a UserRepository interface node");
    };
    let maps_to: Vec<String> = b
        .store
        .edges_of(repo.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "MapsTo")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .map(|n| n.name.clone())
        .collect();
    assert!(
        !maps_to.is_empty(),
        "UserRepository should get MapsTo (DAO -> table) via References + Project"
    );
    // (3) Read / write verb classification: `findById` -> ReadsDb, `save` -> WritesDb.
    // The action edge lands on the **method** node (`emit_db_edge` starts from loc.owner).
    let svc = nodes_of_kind(&b, "Method")
        .into_iter()
        .find(|n| n.name == "run")
        .expect("expected a run method");
    let actions: Vec<String> = b
        .store
        .edges_of(svc.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect();
    assert!(
        actions.iter().any(|k| k == "ReadsDb"),
        "findById should become ReadsDb, actual action edges: {actions:?}"
    );
    assert!(
        actions.iter().any(|k| k == "WritesDb"),
        "save should become WritesDb, actual action edges: {actions:?}"
    );
}
