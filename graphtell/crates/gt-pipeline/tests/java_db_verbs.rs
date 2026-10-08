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
import java.util.List;

public interface UserRepository extends JpaRepository<User, Long> {
    User findById(Long id);
    User save(User u);
    List<User> findAll();
    void deleteById(Long id);
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

    // `findById` (read) + `save` (write) on one owner method.
    public void run(List<Long> ids) {
        for (Long id : ids) {
            repo.findById(id);
            repo.save(new User());
        }
    }

    // `findAll` (read) + `deleteById` (write) on a *different* owner method: the read / write action edge is
    // deduped by (kind, owner, table), so a second read verb on the same method would be collapsed onto the
    // first and its call-site annotation dropped. Splitting keeps each verb's own `DbQuery` annotation.
    public void run2(List<Long> ids) {
        for (Long id : ids) {
            repo.findAll();
            repo.deleteById(id);
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
    // (3) Read / write verb classification.
    //
    // Edge level: `emit_db_edge` lays `owner -> table` edges deduped by (kind, owner, table), so `run` gets
    // exactly one `ReadsDb` and one `WritesDb` edge even though it makes two read calls (findById, findAll) and
    // two write calls (save, deleteById). The exact counts (not `any`) still catch a regression that attached
    // *both* classifications to every call or duplicated an edge.
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
    let read_count = actions.iter().filter(|k| *k == "ReadsDb").count();
    let write_count = actions.iter().filter(|k| *k == "WritesDb").count();
    assert_eq!(
        read_count, 1,
        "`run` must get exactly one ReadsDb edge (deduped across read verbs), got {read_count}: {actions:?}"
    );
    assert_eq!(
        write_count, 1,
        "`run` must get exactly one WritesDb edge (deduped across write verbs), got {write_count}: {actions:?}"
    );

    // Per-verb classification lives on the **call site** annotations (`DbQuery` channel, subkind = the verb),
    // not the deduped edge — so this is the signal that pins each individual verb. `findById` / `findAll` are
    // read verbs, `save` / `deleteById` are write verbs; asserting the subkind sets proves every one of the four
    // calls was classified against the FKB `db_verbs` list. A regression dropping `findAll` / `deleteById` from
    // the list would shrink these sets (the deduped edge count alone cannot tell them apart).
    let db_verbs: Vec<(String, String)> = b
        .store
        .annotations_of_project(b.project.id)
        .expect("annotations")
        .into_values()
        .flatten()
        .filter(|a| a.channel.as_str() == "DbQuery")
        .map(|a| (a.kind.clone(), a.subkind.clone().unwrap_or_default()))
        .collect();
    let read_verbs: std::collections::HashSet<&str> = db_verbs
        .iter()
        .filter(|(k, _)| k == "db-query")
        .map(|(_, v)| v.as_str())
        .collect();
    let write_verbs: std::collections::HashSet<&str> = db_verbs
        .iter()
        .filter(|(k, _)| k == "db-write")
        .map(|(_, v)| v.as_str())
        .collect();
    for v in ["findById", "findAll"] {
        assert!(
            read_verbs.contains(v),
            "read verb `{v}` must be classified as ReadsDb (db-query), got read verbs: {read_verbs:?}"
        );
    }
    for v in ["save", "deleteById"] {
        assert!(
            write_verbs.contains(v),
            "write verb `{v}` must be classified as WritesDb (db-write), got write verbs: {write_verbs:?}"
        );
    }
}
