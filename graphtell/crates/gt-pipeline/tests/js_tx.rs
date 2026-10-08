//! End-to-end self-check that P13 Tx (**multi-table write without a transaction boundary**) fires on a
//! TypeScript Node framework + ORM sample — proving the kernel's language-agnostic Tx rule serves a third stack,
//! not just PHP and Java. The transaction markers come from `fkb/js/typeorm.yaml`'s `tx_calls:`; the `WritesDb`
//! edges come from that file's `db_verbs:` plus the JS analyzer unwrapping `Repository<Entity>` to the entity,
//! whose `MapsTo` Table is built by the `@Entity` rule. So `createBad` (2 tables, no tx) is flagged, and
//! `createInTx` (wrapped in `startTransaction`) is not.

use gt_domain::model::{Annotation, Node, NodeKind, ProjectConfig};
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

fn synthetic_nestjs_tx_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-node-a-tx-{}-{}",
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

  // Writes only ONE table with no transaction boundary -> must NOT be flagged. This pins the "multi" in
  // multi-write-without-tx (P13 requires >= 2 distinct tables), so a regression that dropped the table-count
  // guard could not slip through as a false positive on a single-table method.
  createOne() {
    this.userRepo.save(new UserEntity());
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
        panic!("the graph build of the synthetic Node framework project should succeed");
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
    // Exactly one method must be flagged: createBad (writes two tables, no boundary). Any other owner would be a
    // false positive, so we pin the count rather than just "non-empty".
    assert_eq!(
        hits.len(),
        1,
        "exactly one method should be flagged (createBad), got owner_fqns: {:?}",
        hits.iter()
            .map(|a| a.evidence.get("owner_fqn"))
            .collect::<Vec<_>>()
    );
    let hit = &hits[0];
    assert!(
        hit.evidence
            .get("owner_fqn")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("createBad"))
            .unwrap_or(false),
        "the flagged method must be createBad (no transaction boundary), got: {:?}",
        hit.evidence.get("owner_fqn")
    );
    // The finding must cite exactly two tables — the "multi" in multi-write-without-tx. A single-table write
    // (createOne) must not reach here, and a hypothetical over-count would be caught too.
    assert_eq!(
        hit.evidence.get("tables").and_then(|v| v.as_u64()),
        Some(2),
        "createBad writes two distinct tables, got evidence: {:?}",
        hit.evidence
    );
    // The protected methods must NOT be flagged: createInTx (startTransaction boundary) and createOne
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
        !owner_fqns.iter().any(|s| s.contains("createInTx")),
        "createInTx is wrapped by a transaction boundary, so it must not be annotated"
    );
    assert!(
        !owner_fqns.iter().any(|s| s.contains("createOne")),
        "createOne writes only one table, so it must not be annotated"
    );
}
