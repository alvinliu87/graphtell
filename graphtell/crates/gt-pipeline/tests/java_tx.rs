//! End-to-end self-check that P13 Tx (**multi-table write without a transaction boundary**) fires on a
//! Java (Spring Boot) sample — proving the kernel's language-agnostic Tx rule serves a second stack, not
//! just PHP. The transaction markers come from `fkb/java/spring-boot.yaml`'s `tx_calls:` (FKB vocabulary,
//! replacing a hard-coded `language == php` gate); the `WritesDb` edges come from that file's `db_verbs:`
//! plus the Java analyzer's existing `Repository<Entity> -> Table` resolution.
//!
//! `runBad` writes two tables (user + order) with no transaction boundary -> must be flagged.
//! `runInTx` wraps the same two writes in `em.getTransaction().begin() / .commit()` -> must NOT be flagged.

use gt_domain::model::{Annotation, Node, NodeKind, ProjectConfig};
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

fn synthetic_spring_tx_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-java-tx-{}-{}",
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
        dir.join("src/main/java/com/demo/Order.java"),
        r#"package com.demo;

import com.baomidou.mybatisplus.annotation.TableName;

@TableName("eb_order")
public class Order {
    private Long id;
}
"#,
    )
    .expect("write Order");

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
        dir.join("src/main/java/com/demo/OrderRepository.java"),
        r#"package com.demo;

import org.springframework.data.jpa.repository.JpaRepository;

public interface OrderRepository extends JpaRepository<Order, Long> {
    Order save(Order o);
}
"#,
    )
    .expect("write OrderRepository");

    std::fs::write(
        dir.join("src/main/java/com/demo/UserService.java"),
        r#"package com.demo;

import org.springframework.beans.factory.annotation.Autowired;
import javax.persistence.EntityManager;

public class UserService {
    @Autowired
    private UserRepository repo;

    @Autowired
    private OrderRepository orderRepo;

    @Autowired
    private EntityManager em;

    // No transaction boundary: writes two tables (user + order) with nothing opening a transaction.
    public void runBad() {
        repo.save(new User());
        orderRepo.save(new Order());
    }

    // Has a transaction boundary (begin / commit) around the same two writes.
    public void runInTx() {
        em.getTransaction().begin();
        repo.save(new User());
        orderRepo.save(new Order());
        em.getTransaction().commit();
    }

    // Writes only ONE table with no transaction boundary -> must NOT be flagged. This pins the "multi" in
    // multi-write-without-tx (P13 requires >= 2 distinct tables), so a regression that dropped the table-count
    // guard could not slip through as a false positive on a single-table method.
    public void runOne() {
        repo.save(new User());
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

fn annotations_of_kind(b: &common::Built, kind: &str) -> Vec<Annotation> {
    let all = b
        .store
        .annotations_of_project(b.project.id)
        .expect("annotations");
    all.values().flatten().filter(|a| a.kind == kind).cloned().collect()
}

#[test]
fn java_multi_write_without_tx_is_flagged() {
    let dir = synthetic_spring_tx_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    // Sanity: the two tables the services write to must exist.
    let tables = nodes_of_kind(&b, "Table");
    assert!(
        tables.iter().any(|t| t.name.contains("user"))
            && tables.iter().any(|t| t.name.contains("order")),
        "expected user + order tables to be synthesised, got: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let hits = annotations_of_kind(&b, "multi-write-without-tx");
    // Exactly one method must be flagged: runBad (writes two tables, no boundary). Any other owner would be a
    // false positive, so we pin the count rather than just "non-empty".
    assert_eq!(
        hits.len(),
        1,
        "exactly one method should be flagged (runBad), got owner_fqns: {:?}",
        hits.iter()
            .map(|a| a.evidence.get("owner_fqn"))
            .collect::<Vec<_>>()
    );
    let hit = &hits[0];
    assert!(
        hit.evidence
            .get("owner_fqn")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("runBad"))
            .unwrap_or(false),
        "the flagged method must be runBad (no transaction boundary), got: {:?}",
        hit.evidence.get("owner_fqn")
    );
    // The finding must cite exactly two tables — the "multi" in multi-write-without-tx. A single-table write
    // (runOne) must not reach here, and a hypothetical over-count would be caught too.
    assert_eq!(
        hit.evidence.get("tables").and_then(|v| v.as_u64()),
        Some(2),
        "runBad writes two distinct tables, got evidence: {:?}",
        hit.evidence
    );
    // The protected methods must NOT be flagged: runInTx (programmatic begin / commit boundary) and runOne
    // (only one table, so the >= 2 table guard excludes it even without a boundary).
    let owner_fqns: Vec<String> = hits
        .iter()
        .filter_map(|a| {
            a.evidence
                .get("owner_fqn")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    assert!(
        !owner_fqns.iter().any(|s| s.contains("runInTx")),
        "runInTx is wrapped by a transaction boundary, so it must not be annotated"
    );
    assert!(
        !owner_fqns.iter().any(|s| s.contains("runOne")),
        "runOne writes only one table, so it must not be annotated"
    );
}
