//! End-to-end: P8's **wiring** — a method that only *calls* the action emitter must still get the semantic
//! edge, and it must be marked as coming from propagation.
//!
//! `propagate.rs`'s unit tests pin the traversal itself (up-chain, cycle safety, decay, `indirect`, the
//! `MapsTo` suppression). What they cannot see is the wiring: the seeds are collected at synthesis from FKB
//! rules and staged on `ctx.propagation_seeds`, and `run` starts with
//! `std::mem::take(...)` + `if seeds.is_empty() { return }`. If seed collection broke, P8 would spin up and
//! do **nothing** — silently, with no diagnostic — and every wrapper method would lose its semantic edge,
//! which is the only reason this phase exists.
//!
//! The chain here: `UserService.get` → `UserDao.load` → `repo.findById` (the verb FKB recognises).
//! `UserDao.load` gets `ReadsDb` from its own call site; `UserService.get` has no DB call of its own, so its
//! `ReadsDb` can **only** come from propagation.

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_spring_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-prop-wiring-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/main/java/com/demo")).expect("mkdir");

    let files: &[(&str, &str)] = &[
        (
            "pom.xml",
            r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-data-jpa</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        ),
        (
            "src/main/java/com/demo/User.java",
            r#"package com.demo;

import com.baomidou.mybatisplus.annotation.TableName;

@TableName("eb_user")
public class User {
    private Long id;
}
"#,
        ),
        (
            "src/main/java/com/demo/UserRepository.java",
            r#"package com.demo;

import org.springframework.data.jpa.repository.JpaRepository;

public interface UserRepository extends JpaRepository<User, Long> {
    User findById(Long id);
}
"#,
        ),
        // The emitter: `load` holds the only call FKB recognises as reading the DB.
        (
            "src/main/java/com/demo/UserDao.java",
            r#"package com.demo;

import org.springframework.beans.factory.annotation.Autowired;

public class UserDao {
    @Autowired
    private UserRepository repo;

    public User load(Long id) {
        return repo.findById(id);
    }
}
"#,
        ),
        // The wrapper: no DB call of its own, so its edge can only arrive via P8.
        (
            "src/main/java/com/demo/UserService.java",
            r#"package com.demo;

import org.springframework.beans.factory.annotation.Autowired;

public class UserService {
    @Autowired
    private UserDao dao;

    public User get(Long id) {
        return dao.load(id);
    }
}
"#,
        ),
    ];
    for (rel, body) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(&path, body).expect("write");
    }
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

fn outgoing(b: &common::Built, id: gt_domain::model::NodeId, kind: &str) -> Vec<String> {
    b.store
        .edges_of(id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == kind)
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .map(|n| n.name.clone())
        .collect()
}

#[test]
fn a_caller_without_its_own_db_call_gets_the_edge_by_propagation() {
    let root = synthetic_spring_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Spring project graph build should succeed");
    };

    let method = |name: &str| {
        nodes_of_kind(&b, "Method")
            .into_iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "expected a `{name}` method, got: {:?}",
                    nodes_of_kind(&b, "Method")
                        .iter()
                        .map(|n| &n.name)
                        .collect::<Vec<_>>()
                )
            })
    };
    let dao = method("load");
    let svc = method("get");

    // Premise: the emitter holds the DB call, so it gets the edge from its own call site.
    assert!(
        !outgoing(&b, dao.id, "ReadsDb").is_empty(),
        "持有 DB 调用的方法应自带 ReadsDb（前提不成立则这条测试测不到点上）"
    );

    // The wrapper has no DB call of its own: its ReadsDb can only come from P8.
    let propagated: Vec<_> = b
        .store
        .edges_of(svc.id, EdgeDirection::Outgoing)
        .expect("edges")
        .into_iter()
        .filter(|e| e.kind.as_str() == "ReadsDb")
        .collect();
    assert!(
        !propagated.is_empty(),
        "包装方法应经传播拿到 ReadsDb，实际出边：{:?}",
        b.store
            .edges_of(svc.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .map(|e| e.kind.as_str().to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        propagated[0].properties.get("via").and_then(|v| v.as_str()),
        Some("propagate"),
        "这条边应被标记为传播所得"
    );

    let _ = std::fs::remove_dir_all(&root);
}
