//! End-to-end self-check of the **JS/TS (TypeORM)** read / write verb classification.
//!
//! The Java side has `java_db_verbs.rs` and Python has `django_features.rs`, but the TypeORM path —
//! `fkb/js/typeorm.yaml`'s `db_verbs` + the `repo-call` resolver (`strategy: variable_type`) — had **no**
//! pipeline coverage: the node tests only assert entity / column / contract nodes. So a regression that stopped
//! classifying `repo.save()` / `repo.find()` would have left every JS service without a single `WritesDb` /
//! `ReadsDb` edge while the suite stayed green.
//!
//! The calls are written with an explicit `this.` (`this.repo.save(u)`), the spelling every NestJS service uses.
//! `crates/gt-adapter-parser/src/js.rs:871` strips that prefix so the kernel's variable-type resolver sees the
//! bare field name (`repo`) it recorded from the constructor — the same trap that was missing on the Java side
//! (`java_db_verbs::this_qualified_field_call_is_classified_like_the_bare_one`).
//!
//! Synthetic project, no external sample.

mod common;

use std::path::PathBuf;

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

fn synthetic_nestjs_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-node-dbverbs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");

    std::fs::write(
        dir.join("package.json"),
        r#"{
  "name": "synth",
  "dependencies": { "@nestjs/core": "^10.0.0", "@nestjs/common": "^10.0.0", "typeorm": "^0.3.0" }
}
"#,
    )
    .expect("write package.json");

    std::fs::write(
        dir.join("src/user.entity.ts"),
        r#"import { Entity, Column, PrimaryGeneratedColumn } from 'typeorm';

@Entity('user')
export class UserEntity {
  @PrimaryGeneratedColumn()
  id: number;

  @Column()
  username: string;
}
"#,
    )
    .expect("write user.entity.ts");

    std::fs::write(
        dir.join("src/user.service.ts"),
        r#"import { Injectable } from '@nestjs/common';
import { Repository } from 'typeorm';
import { UserEntity } from './user.entity';

@Injectable()
export class UserService {
  constructor(private readonly repo: Repository<UserEntity>) {}

  create(u: UserEntity) {
    return this.repo.save(u);
  }

  readAll() {
    return this.repo.find();
  }
}
"#,
    )
    .expect("write user.service.ts");

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

fn out_edge_kinds(b: &common::Built, method: &str) -> Vec<String> {
    let m = nodes_of_kind(b, "Method")
        .into_iter()
        .find(|n| n.name == method)
        .unwrap_or_else(|| panic!("the `{method}` method node must exist"));
    b.store
        .edges_of(m.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect()
}

/// `this.repo.save(u)` / `this.repo.find()` — the `this.`-qualified field calls of a NestJS service — must be
/// classified into `WritesDb` / `ReadsDb` exactly like the bare spellings would be.
#[test]
fn typeorm_repository_verbs_are_classified_through_this() {
    let dir = synthetic_nestjs_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    // Precondition: the entity really maps to a table, otherwise there is nothing to write to.
    let tables: Vec<String> = nodes_of_kind(&b, "Table").into_iter().map(|n| n.name).collect();
    assert!(
        tables.iter().any(|t| t == "user"),
        "the UserEntity table must be produced, got: {tables:?}"
    );

    let create = out_edge_kinds(&b, "create");
    assert_eq!(
        create.iter().filter(|k| *k == "WritesDb").count(),
        1,
        "`this.repo.save(u)` must be classified as a write (save is a write verb on a Repository<UserEntity>): {create:?}"
    );

    let read = out_edge_kinds(&b, "readAll");
    assert_eq!(
        read.iter().filter(|k| *k == "ReadsDb").count(),
        1,
        "`this.repo.find()` must be classified as a read: {read:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
