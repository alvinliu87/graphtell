//! End-to-end self-check that P13 Tx (**multi-table write without a transaction boundary**) fires on a
//! TypeScript / NestJS + TypeORM sample — proving the kernel's language-agnostic Tx rule serves a third stack,
//! not just PHP and Java. The transaction markers come from `fkb/js/typeorm.yaml`'s `tx_calls:`; the `WritesDb`
//! edges come from that file's `db_verbs:` plus the JS analyzer unwrapping `Repository<Entity>` to the entity,
//! whose `MapsTo` Table is built by the `@Entity` rule. So `createBad` (2 tables, no tx) is flagged, and
//! `createInTx` (wrapped in `startTransaction`) is not.

use gt_domain::model::{Annotation, Node, NodeKind, ProjectConfig};
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

fn synthetic_nestjs_tx_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-nestjs-tx-{}-{}",
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
  "name": "order-service",
  "dependencies": {
    "typeorm": "^0.3.0",
    "@nestjs/typeorm": "^10.0.0",
    "@nestjs/common": "^10.0.0",
    "@nestjs/core": "^10.0.0"
  }
}
"#,
    )
    .expect("write package.json");

    std::fs::write(
        dir.join("src/user.entity.ts"),
        r#"import { Entity } from 'typeorm';

@Entity('user')
export class UserEntity {
  id: number;
}
"#,
    )
    .expect("write user.entity.ts");

    std::fs::write(
        dir.join("src/order.entity.ts"),
        r#"import { Entity } from 'typeorm';

@Entity('order')
export class OrderEntity {
  id: number;
}
"#,
    )
    .expect("write order.entity.ts");

    std::fs::write(
        dir.join("src/user.service.ts"),
        r#"import { Injectable } from '@nestjs/common';
import { InjectRepository } from '@nestjs/typeorm';
import { Repository, DataSource } from 'typeorm';
import { UserEntity } from './user.entity';
import { OrderEntity } from './order.entity';

@Injectable()
export class UserService {
  constructor(
    @InjectRepository(UserEntity) private userRepo: Repository<UserEntity>,
    @InjectRepository(OrderEntity) private orderRepo: Repository<OrderEntity>,
    private dataSource: DataSource,
  ) {}

  // No transaction boundary: writes two tables (user + order) with nothing opening a transaction.
  createBad() {
    this.userRepo.save(new UserEntity());
    this.orderRepo.save(new OrderEntity());
  }

  // Has a transaction boundary (startTransaction) around the same two writes.
  createInTx() {
    const qr = this.dataSource.createQueryRunner();
    qr.startTransaction();
    this.userRepo.save(new UserEntity());
    this.orderRepo.save(new OrderEntity());
    qr.commitTransaction();
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

fn annotations_of_kind(b: &common::Built, kind: &str) -> Vec<Annotation> {
    let all = b
        .store
        .annotations_of_project(b.project.id)
        .expect("annotations");
    all.values().flatten().filter(|a| a.kind == kind).cloned().collect()
}

#[test]
fn nestjs_multi_write_without_tx_is_flagged() {
    let dir = synthetic_nestjs_tx_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic NestJS project should succeed");
    };

    // Sanity: the two tables the service writes to must exist (the `@Entity` rule synthesises them).
    let tables = nodes_of_kind(&b, "Table");
    assert!(
        tables.iter().any(|t| t.name.contains("user"))
            && tables.iter().any(|t| t.name.contains("order")),
        "expected user + order tables to be synthesised, got: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let hits = annotations_of_kind(&b, "multi-write-without-tx");
    assert!(
        !hits.is_empty(),
        "P13 must fire multi-write-without-tx on the NestJS/TypeORM sample (createBad writes two tables with no transaction boundary)"
    );
    assert!(
        hits.iter().any(|a| a
            .evidence
            .get("owner_fqn")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("createBad"))
            .unwrap_or(false)),
        "the annotated method must be createBad (no transaction boundary), got owner_fqns: {:?}",
        hits.iter().map(|a| a.evidence.get("owner_fqn")).collect::<Vec<_>>()
    );
    assert!(
        !hits.iter().any(|a| a
            .evidence
            .get("owner_fqn")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("createInTx"))
            .unwrap_or(false)),
        "createInTx is wrapped by startTransaction, so it must not be annotated"
    );
    eprintln!("NestJS multi-write-without-tx hits = {}", hits.len());
}
