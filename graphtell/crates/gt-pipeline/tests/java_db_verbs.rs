//! Java（Spring Boot）**DAO → 表** 与 **读 / 写动词分类**的端到端自检。
//!
//! 覆盖这条链：`interface UserRepository extends JpaRepository<User, Long>`
//!   → 泛型实参 `User`（DAO 操作的实体，parser 记成 `generic.JpaRepository` 合成调用点）
//!   → `References`（DAO → 实体，FKB `Link`）
//!   → `Project` 沿实体的 `MapsTo` 投影成 `MapsTo`（DAO → 表）
//!   → P7 据此把 `repo.save()` / `repo.findById()` 落成 `WritesDb` / `ReadsDb`。

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
        panic!("建图应成功");
    };

    let tables = nodes_of_kind(&b, "Table");
    assert!(
        tables.iter().any(|t| t.name.contains("user")),
        "应合成出 user 表，实际：{:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let Some(repo) = nodes_of_kind(&b, "Interface")
        .into_iter()
        .find(|n| n.name == "UserRepository")
    else {
        panic!("应有 UserRepository 接口节点");
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
        "UserRepository 应经 References + Project 拿到 MapsTo（DAO → 表）"
    );
    // ③ 读 / 写动词分类：`findById` → ReadsDb、`save` → WritesDb。
    // 动作边落在**方法**节点上（`emit_db_edge` 以 loc.owner 为起点）。
    let svc = nodes_of_kind(&b, "Method")
        .into_iter()
        .find(|n| n.name == "run")
        .expect("应有 run 方法");
    let actions: Vec<String> = b
        .store
        .edges_of(svc.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect();
    assert!(
        actions.iter().any(|k| k == "ReadsDb"),
        "findById 应落成 ReadsDb，实际动作边：{actions:?}"
    );
    assert!(
        actions.iter().any(|k| k == "WritesDb"),
        "save 应落成 WritesDb，实际动作边：{actions:?}"
    );
}
