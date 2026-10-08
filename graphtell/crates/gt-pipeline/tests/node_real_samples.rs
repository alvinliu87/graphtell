//! Validate the Node.js web-framework samples' graph build with **real open-source samples**.
//!
//! Where samples come from: the `node_a` and `node_b` checkouts under `samples/`
//! (downloaded by developers, git-ignored, not in the repo), located by
//! `gt_sample_support::node_samples_root` (the `GRAPHTELL_SAMPLE_DIR` env var pointing at a checkout,
//! or a bounded search under `samples/`). When a sample is missing the
//! test **skips** rather than fails, so CI runs without samples.
//!
//! These cases verify end-to-end conclusions: whether a real Node.js controller's `@Get/@Post` becomes an HttpContract
//! with HandledBy to a method node; whether a real Node.js `app.get` becomes an HttpContract.

mod common;

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery};
use gt_sample_support::{missing_hint_named, node_samples_root};

fn nodes_of_kind<'a>(
    b: &'a common::Built,
    kind: &str,
) -> Vec<gt_domain::model::Node> {
    b.store
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            ..Default::default()
        })
        .expect("query_nodes")
        .into_iter()
        .collect()
}

/// Whether a given kind of **out-edge** exists on a semantic node (e.g. HttpContract → Method's HandledBy).
fn has_outgoing_edge(b: &common::Built, kind: &str, edge: &str) -> bool {
    nodes_of_kind(b, kind).iter().any(|n| {
        b.store
            .edges_of(n.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .any(|e| e.kind.as_str() == edge)
    })
}

/// Find an `HttpContract` whose name contains `substr`, and return the name of the node its `HandledBy` edge points
/// to. Used to pin *precise* handler resolution (the decorated method), not merely "some HandledBy edge exists".
fn contract_handler_name(b: &common::Built, substr: &str) -> Option<String> {
    let c = nodes_of_kind(b, "HttpContract")
        .into_iter()
        .find(|n| n.name.contains(substr))?;
    b.store
        .edges_of(c.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "HandledBy")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .map(|t| t.name)
        .next()
}

/// Synthetic-project self-check (self-consistent, no external sample needed): covers both Node.js decorator routes and web-framework member-style routes,
/// verifying the JS parser + FKB produce HttpContract + HandledBy on synthetic code, so CI guards without samples.
#[test]
fn synthetic_nestjs_and_express_graph() {
    let dir = std::env::temp_dir().join("gt_synth_node");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // node_a sub-project
    std::fs::write(
        dir.join("package.json"),
        r#"{
  "name": "synth",
  "dependencies": { "@nestjs/core": "^10.0.0", "@nestjs/common": "^10.0.0", "express": "^4.18.0", "typeorm": "^0.3.0" }
}
"#,
    )
    .unwrap();
    // the injected provider (@Injectable)
    std::fs::write(
        dir.join("src/user.service.ts"),
        r#"import { Injectable } from '@nestjs/common';

@Injectable()
export class UserService {
  findOne() { return 1; }
}
"#,
    )
    .unwrap();
    // ORM entity (@Entity('user') → Table, @Column fields → columns)
    std::fs::write(
        dir.join("src/user.entity.ts"),
        r#"import { Entity, Column, PrimaryGeneratedColumn } from 'typeorm';

@Entity('user')
export class UserEntity {
  @PrimaryGeneratedColumn()
  id: number;

  @Column()
  username: string;

  @Column({ default: '' })
  bio: string;
}
"#,
    )
    .unwrap();
    // second entity: with `@ManyToOne` relation, for the References-edge assertion (foreign-key holder → referenced entity)
    std::fs::write(
        dir.join("src/article.entity.ts"),
        r#"import { Entity, Column, PrimaryGeneratedColumn, ManyToOne } from 'typeorm';
import { UserEntity } from './user.entity';

@Entity('article')
export class ArticleEntity {
  @PrimaryGeneratedColumn()
  id: number;

  @Column()
  slug: string;

  @ManyToOne(type => UserEntity, user => user.articles)
  author: UserEntity;
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("src/user.controller.ts"),
        r#"import { Get, Controller } from '@nestjs/common';
import { UserService } from './user.service';

@Controller()
export class UserController {
  constructor(private readonly userService: UserService) {}

  @Get('user')
  findMe() { return this.userService.findOne(); }
}
"#,
    )
    .unwrap();
    // node_b sub-project (same root as node_a, distinguished by the web-framework dependency in package.json)
    std::fs::write(
        dir.join("app.js"),
        r#"const express = require('express');
const app = express();

app.get('/login', (req, res) => { res.send('ok'); });
app.post('/login', (req, res) => { res.send('ok'); });
"#,
    )
    .unwrap();

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("graphing the synthetic project must succeed");
    };

    // node_a: decorator → HttpContract, and HandledBy points to a method node
    let nest_contracts = nodes_of_kind(&b, "HttpContract");
    let nest_names: Vec<&str> = nest_contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        nest_names.iter().any(|n| n.contains("user")),
        "node_a must produce user-related contracts, got: {nest_names:?}"
    );
    assert!(
        has_outgoing_edge(&b, "HttpContract", "HandledBy"),
        "a node_a HttpContract must have a HandledBy out-edge (to the handler method), got: {nest_names:?}"
    );
    // The `HandledBy` must resolve to the *precise* handler method — `@Get('user') findMe` → `findMe`, not merely
    // "some method". A regression that pointed the edge at the wrong method (e.g. the constructor or a sibling) would
    // otherwise pass the looser `has_outgoing_edge` check above.
    let handler = contract_handler_name(&b, "user")
        .expect("the `user` contract must carry a HandledBy edge");
    assert_eq!(
        handler, "findMe",
        "the `@Get('user')` contract must resolve HandledBy to the `findMe` method, got: {handler}"
    );

    // node_b: member-style `app.get` / `app.post` → HttpContract (path from arg0, method from member name).
    // Both verbs must map correctly: `get` → GET, `post` → POST. The looser original only required *one* of them,
    // so a regression that dropped POST verb handling would have slipped through.
    let all = b
        .store
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: b.project.id,
            ..Default::default()
        })
        .expect("q");
    let login: Vec<&str> = all
        .iter()
        .filter(|n| n.kind.as_str() == "HttpContract")
        .map(|n| n.name.as_str())
        .filter(|n| n.contains("login"))
        .collect();
    assert!(
        login.iter().any(|n| n.starts_with("GET /login"))
            && login.iter().any(|n| n.starts_with("POST /login")),
        "node_b must produce both GET /login and POST /login contracts (get→GET, post→POST), got: {login:?}"
    );

    // node_a DI: `constructor(private readonly userService: UserService)`
    // → `UserController --DependsOn--> UserService`
    assert!(
        class_links_to_named(&b, "UserController", "DependsOn", "UserService"),
        "UserController must DependsOn UserService"
    );

    // TypeORM: `@Entity('user')` → Table node "user", and the model class connects to the table via MapsTo
    let tables: Vec<&str> = all
        .iter()
        .filter(|n| n.kind.as_str() == "Table")
        .map(|n| n.name.as_str())
        .collect();
    assert!(
        tables.iter().any(|t| t == &"user"),
        "the Table node user must be produced, got: {tables:?}"
    );
    assert!(
        class_links_to(&b, "UserEntity", "MapsTo"),
        "UserEntity must connect to the user table via MapsTo"
    );

    // entity field → column: `@Column() username` becomes a `Column` node, pointed at by the entity via HasColumn
    let cols: Vec<String> = nodes_of_kind(&b, "Column")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        cols.iter().any(|c| c == "UserEntity.username"),
        "the column node UserEntity.username must be produced, got: {cols:?}"
    );
    assert!(
        class_links_to(&b, "UserEntity", "HasColumn"),
        "UserEntity must point at its field columns via HasColumn"
    );

    // entity relation (foreign-key holder): `@ManyToOne(…) author: UserEntity`
    // → `ArticleEntity --References--> UserEntity`
    assert!(
        class_links_to_named(&b, "ArticleEntity", "References", "UserEntity"),
        "ArticleEntity must connect to UserEntity via References"
    );

    // table-level foreign key: entity relation projects via `Project` to table → table (both entities have @Entity('x'), so both ends exist)
    assert!(
        node_links_to_named(&b, "Table", "article", "ForeignKey", "user"),
        "the article table must connect to the user table via a foreign key"
    );
}

/// Whether a node (located by `kind`) has an out-edge of the given kind to a node named `target`.
///
/// All three relation-edge kinds go through here: constructor injection `DependsOn`, TypeORM entity relation `References`
/// (foreign-key holder → referenced entity), table-level foreign key `ForeignKey` (table → table).
fn node_links_to_named(
    b: &common::Built,
    kind: &str,
    from: &str,
    edge: &str,
    target: &str,
) -> bool {
    let Some(src) = nodes_of_kind(b, kind).into_iter().find(|n| n.name == from) else {
        return false;
    };
    b.store
        .edges_of(src.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == edge)
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .any(|n| n.name == target)
}

/// Whether a **class** node has an out-edge of the given kind to `target` (class specialization of [`node_links_to_named`]).
fn class_links_to_named(b: &common::Built, from: &str, edge: &str, target: &str) -> bool {
    node_links_to_named(b, "Class", from, edge, target)
}

/// Whether a **class** node has an out-edge of the given kind (e.g. UserEntity → Table's MapsTo).
fn class_links_to(b: &common::Built, class: &str, edge: &str) -> bool {
    let Some(node) = nodes_of_kind(b, "Class")
        .into_iter()
        .find(|n| n.name == class)
    else {
        return false;
    };
    b.store
        .edges_of(node.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .any(|e| e.kind.as_str() == edge)
}

#[test]
fn node_a_real_sample_produces_route_contracts() {
    let Some((nest_root, _)) = node_samples_root() else {
        eprintln!("skipped: {}", missing_hint_named("node-sample-a"));
        return;
    };
    let Some(b) = common::graph_with_root(&nest_root, ProjectConfig::default()) else {
        panic!("graphing the real sample must succeed");
    };
    let contracts = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        !names.is_empty(),
        "the real node_a sample must produce an HttpContract, but the node list is empty"
    );
    // `@Get('user')` / `@Post('users')` etc. in `user.controller.ts` should become
    assert!(
        names.iter().any(|n| n.contains("user")),
        "a user-related contract must be included, got: {names:?}"
    );
    // HandledBy should leave the HttpContract, pointing to a method node (proves owner resolved precisely to `Class.method`).
    // Note the link direction is `to_target`: the edge is **emitted** by the contract node, so look at the HttpContract's **out-edges**.
    assert!(
        has_outgoing_edge(&b, "HttpContract", "HandledBy"),
        "an HttpContract must have a HandledBy out-edge (to the handler method), contracts: {names:?}"
    );
    // DI: `constructor(private readonly userService: UserService)` in `user.controller.ts`
    // → `UserController --DependsOn--> UserService`
    assert!(
        class_links_to_named(&b, "UserController", "DependsOn", "UserService"),
        "UserController must DependsOn UserService"
    );
    // TypeORM: `@Entity('user')` → Table node "user"
    let tables: Vec<String> = nodes_of_kind(&b, "Table")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        tables.iter().any(|t| t == "user"),
        "the Table node user must be produced, got: {tables:?}"
    );
    assert!(
        class_links_to(&b, "UserEntity", "MapsTo"),
        "UserEntity must connect to the user table via MapsTo"
    );

    // entity field → column: each entity has `@PrimaryGeneratedColumn() id`, the column identity carries entity scope,
    // same-named columns must each count separately (if merged into one id node by bare field name, the blast radius would link the two tables together).
    let cols: Vec<String> = nodes_of_kind(&b, "Column")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        cols.iter().any(|c| c == "UserEntity.id") && cols.iter().any(|c| c == "ArticleEntity.id"),
        "the same-named id columns of different entities must stay separate, got: {:?}",
        &cols[..cols.len().min(12)]
    );
    assert!(
        class_links_to(&b, "UserEntity", "HasColumn"),
        "UserEntity must point at its field columns via HasColumn"
    );

    assert!(
        class_links_to_named(&b, "Comment", "References", "ArticleEntity"),
        "Comment must connect to ArticleEntity via References"
    );
    assert!(
        class_links_to_named(&b, "ArticleEntity", "References", "UserEntity"),
        "ArticleEntity must connect to UserEntity via References"
    );
    assert!(
        class_links_to_named(&b, "UserEntity", "References", "ArticleEntity"),
        "UserEntity must connect to ArticleEntity via ManyToMany (favorites)"
    );

    // table-level foreign key: `References` (entity → entity) projects via `Project` into `ForeignKey` (table → table).
    // `Comment` is `@Entity()` with no arg → no Table, its foreign key doesn't enter the graph (better missing than guessed).
    assert!(
        node_links_to_named(&b, "Table", "article", "ForeignKey", "user"),
        "the article table must connect to the user table via a foreign key"
    );
    assert!(
        node_links_to_named(&b, "Table", "user", "ForeignKey", "article"),
        "the user table must connect to the article table via a foreign key (@ManyToMany favorites)"
    );
    eprintln!(
        "real node_a sample: contracts = {}, table examples: {:?}, tables = {}, columns = {}",
        names.len(),
        &names[..names.len().min(8)],
        tables.len(),
        cols.len()
    );
}

/// Module-level `MiddlewareConsumer`: `consumer.apply(AuthMiddleware).forRoutes({path, method})`
/// must turn `AuthMiddleware` into a `Middleware` semantic node, and let the protected routes point to it via `PassesThrough`
/// (reproducing the earlier "object-literal arg-0 misses" regression).
#[test]
fn node_a_real_sample_consumer_middleware() {
    let Some((nest_root, _)) = node_samples_root() else {
        eprintln!("skipped: {}", missing_hint_named("node-sample-a"));
        return;
    };
    let Some(b) = common::graph_with_root(&nest_root, ProjectConfig::default()) else {
        panic!("graphing the real sample must succeed");
    };
    let mws: Vec<String> = nodes_of_kind(&b, "Middleware")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        mws.iter().any(|n| n == "AuthMiddleware"),
        "the Middleware node AuthMiddleware must be produced, got: {mws:?}"
    );
    // at least one contract connects to AuthMiddleware via `PassesThrough` (in-edge).
    let linked = nodes_of_kind(&b, "Middleware")
        .into_iter()
        .filter(|m| m.name == "AuthMiddleware")
        .any(|m| {
            b.store
                .edges_of(m.id, EdgeDirection::Incoming)
                .expect("edges")
                .iter()
                .any(|e| e.kind.as_str() == "PassesThrough")
        });
    assert!(
        linked,
        "at least one HttpContract must connect to AuthMiddleware via PassesThrough"
    );
    let count = nodes_of_kind(&b, "Middleware")
        .into_iter()
        .filter(|m| m.name == "AuthMiddleware")
        .flat_map(|m| b.store.edges_of(m.id, EdgeDirection::Incoming).expect("edges"))
        .filter(|e| e.kind.as_str() == "PassesThrough")
        .count();
    eprintln!("contracts guarded by AuthMiddleware in the real node_a sample = {count}");
}

#[test]
fn node_b_real_sample_produces_route_contracts() {
    let Some((_, expr_root)) = node_samples_root() else {
        eprintln!("skipped: {}", missing_hint_named("node-sample-b"));
        return;
    };
    let Some(b) = common::graph_with_root(&expr_root, ProjectConfig::default()) else {
        panic!("graphing the real sample must succeed");
    };
    let contracts = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        !names.is_empty(),
        "the real node_b sample must produce an HttpContract, but the node list is empty"
    );
    // `app.get('/login', ...)` should become
    assert!(
        names.iter().any(|n| n.contains("login")),
        "the /login contract must be included, got: {names:?}"
    );
    eprintln!("contract count in the real node_b sample = {}, examples: {:?}", names.len(), &names[..names.len().min(8)]);
}



