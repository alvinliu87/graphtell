//! 用**真实开源样本**验证 NestJS / Express 的建图效果。
//!
//! 样本从哪来：本目录 `samples/nestjs-realworld-example-app` 与 `samples/hackathon-starter`
//! （由开发者下载，git 忽略，不入库）。样本缺失时测试**跳过**而非失败，以便 CI 无样本也能跑。
//!
//! 这些用例验证的是端到端结论：真实 NestJS 控制器的 `@Get/@Post` 是否落成 HttpContract
//! 且 HandledBy 连到方法节点；真实 Express 的 `app.get` 是否落成 HttpContract。

mod common;

use std::path::PathBuf;

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery};

/// 定位真实 Node 样本根目录：先试环境变量 `GRAPHTELL_NODE_SAMPLE_DIR`，
/// 再回退到仓库内相对路径。返回 (nestjs 根, express 根) 的 Option 元组。
fn node_samples() -> Option<(PathBuf, PathBuf)> {
    if let Ok(dir) = std::env::var("GRAPHTELL_NODE_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some((p.clone(), p));
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..7 {
        let nest = cur.join("samples/nestjs-realworld-example-app");
        let expr = cur.join("samples/hackathon-starter");
        if nest.is_dir() && expr.is_dir() {
            return Some((nest, expr));
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

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

/// 某语义节点的某类**出边**是否存在（如 HttpContract → Method 的 HandledBy）。
fn has_outgoing_edge(b: &common::Built, kind: &str, edge: &str) -> bool {
    nodes_of_kind(b, kind).iter().any(|n| {
        b.store
            .edges_of(n.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .any(|e| e.kind.as_str() == edge)
    })
}

/// 合成工程自检（自洽、无需外部样本）：同时覆盖 NestJS 装饰器路由与 Express 成员式路由，
/// 验证 JS 解析器 + FKB 在合成代码上即可产出 HttpContract + HandledBy，使 CI 无样本也能守护。
#[test]
fn synthetic_nestjs_and_express_graph() {
    let dir = std::env::temp_dir().join("gt_synth_node");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // NestJS 子工程
    std::fs::write(
        dir.join("package.json"),
        r#"{
  "name": "synth",
  "dependencies": { "@nestjs/core": "^10.0.0", "@nestjs/common": "^10.0.0", "express": "^4.18.0", "typeorm": "^0.3.0" }
}
"#,
    )
    .unwrap();
    // 被注入的 provider（@Injectable）
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
    // ORM 实体（@Entity('user') → Table，@Column 字段 → 列）
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
    // 第二个实体：带 `@ManyToOne` 关联，供 References 边断言（外键持有方 → 被引用实体）
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
    // Express 子工程（与 nestjs 同根，靠 package.json 的 `express` 依赖区分框架）
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
        panic!("合成工程建图应成功");
    };

    // NestJS：装饰器 → HttpContract，且 HandledBy 指出到方法节点
    let nest_contracts = nodes_of_kind(&b, "HttpContract");
    let nest_names: Vec<&str> = nest_contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        nest_names.iter().any(|n| n.contains("user")),
        "NestJS 应产出 user 相关契约，实际：{nest_names:?}"
    );
    assert!(
        has_outgoing_edge(&b, "HttpContract", "HandledBy"),
        "NestJS HttpContract 应有 HandledBy 出边（连到处理方法），实际：{nest_names:?}"
    );

    // Express：成员式 `app.get` → HttpContract（路径从 arg0 取，方法从成员名取）
    let express_root = dir.join("app.js");
    let _ = express_root;
    // 用全量节点里找 /login 契约
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
        login.iter().any(|n| n.starts_with("GET /login") || n.starts_with("POST /login")),
        "Express 应产出 /login 契约，实际：{login:?}"
    );

    // NestJS 依赖注入：`constructor(private readonly userService: UserService)`
    // → `UserController --DependsOn--> UserService`
    assert!(
        class_links_to_named(&b, "UserController", "DependsOn", "UserService"),
        "UserController 应 DependsOn 到 UserService"
    );

    // TypeORM：`@Entity('user')` → Table 节点 "user"，且模型类经 MapsTo 连到表
    let tables: Vec<&str> = all
        .iter()
        .filter(|n| n.kind.as_str() == "Table")
        .map(|n| n.name.as_str())
        .collect();
    assert!(
        tables.iter().any(|t| t == &"user"),
        "应产出 Table 节点 user，实际：{tables:?}"
    );
    assert!(
        class_links_to(&b, "UserEntity", "MapsTo"),
        "UserEntity 应经 MapsTo 连到表 user"
    );

    // 实体字段 → 列：`@Column() username` 落成 `Column` 节点，并由实体经 HasColumn 指向
    let cols: Vec<String> = nodes_of_kind(&b, "Column")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        cols.iter().any(|c| c == "UserEntity.username"),
        "应产出列节点 UserEntity.username，实际：{cols:?}"
    );
    assert!(
        class_links_to(&b, "UserEntity", "HasColumn"),
        "UserEntity 应经 HasColumn 指向其字段列"
    );

    // 实体关联（外键持有方）：`@ManyToOne(…) author: UserEntity`
    // → `ArticleEntity --References--> UserEntity`
    assert!(
        class_links_to_named(&b, "ArticleEntity", "References", "UserEntity"),
        "ArticleEntity 应经 References 连到 UserEntity"
    );

    // 表级外键：实体关联经 `Project` 投影到表 → 表（两个实体都有 @Entity('x')，故两端齐备）
    assert!(
        node_links_to_named(&b, "Table", "article", "ForeignKey", "user"),
        "article 表应经 ForeignKey 连到 user 表"
    );
}

/// 某节点（按 `kind` 定位）是否有指定种类的**出边**连到名为 `target` 的节点。
///
/// 三类关系边都走这里：NestJS 构造器注入 `DependsOn`、TypeORM 实体关联 `References`
/// （外键持有方 → 被引用实体）、表级外键 `ForeignKey`（表 → 表）。
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

/// 某**类**节点是否有指定种类的出边连到 `target`（[`node_links_to_named`] 的类特化）。
fn class_links_to_named(b: &common::Built, from: &str, edge: &str, target: &str) -> bool {
    node_links_to_named(b, "Class", from, edge, target)
}

/// 某**类**节点是否有指定种类的**出边**（如 UserEntity → Table 的 MapsTo）。
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
fn nestjs_real_sample_produces_route_contracts() {
    let Some((nest_root, _)) = node_samples() else {
        eprintln!("跳过：未找到 NestJS 真实样本（samples/nestjs-realworld-example-app）");
        return;
    };
    let Some(b) = common::graph_with_root(&nest_root, ProjectConfig::default()) else {
        panic!("真实样本建图应成功");
    };
    let contracts = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        !names.is_empty(),
        "真实 NestJS 样本应产出 HttpContract，实际节点为空"
    );
    // `user.controller.ts` 里 `@Get('user')` / `@Post('users')` 等应落成
    assert!(
        names.iter().any(|n| n.contains("user")),
        "应包含 user 相关契约，实际：{names:?}"
    );
    // HandledBy 应从 HttpContract 指出、连到方法节点（证明 owner 精确到了 `Class.method`）。
    // 注意 link 方向是 `to_target`：边由契约节点**指出**，故查 HttpContract 的**出边**。
    assert!(
        has_outgoing_edge(&b, "HttpContract", "HandledBy"),
        "HttpContract 应有 HandledBy 出边（连到处理方法），实际契约：{names:?}"
    );
    // 依赖注入：`user.controller.ts` 里 `constructor(private readonly userService: UserService)`
    // → `UserController --DependsOn--> UserService`
    assert!(
        class_links_to_named(&b, "UserController", "DependsOn", "UserService"),
        "UserController 应 DependsOn 到 UserService"
    );
    // TypeORM：`@Entity('user')` → Table 节点 "user"
    let tables: Vec<String> = nodes_of_kind(&b, "Table")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        tables.iter().any(|t| t == "user"),
        "应产出 Table 节点 user，实际：{tables:?}"
    );
    assert!(
        class_links_to(&b, "UserEntity", "MapsTo"),
        "UserEntity 应经 MapsTo 连到表 user"
    );

    // 实体字段 → 列：各实体都有 `@PrimaryGeneratedColumn() id`，列身份带实体作用域，
    // 同名列必须各算各的（若按裸字段名合并成一个 id 节点，影响面会把两个表串起来）。
    let cols: Vec<String> = nodes_of_kind(&b, "Column")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        cols.iter().any(|c| c == "UserEntity.id") && cols.iter().any(|c| c == "ArticleEntity.id"),
        "不同实体的同名 id 列应各自独立，实际：{:?}",
        &cols[..cols.len().min(12)]
    );
    assert!(
        class_links_to(&b, "UserEntity", "HasColumn"),
        "UserEntity 应经 HasColumn 指向其字段列"
    );

    // 实体关联：只建**外键持有方**的 `@ManyToOne` / `@ManyToMany`
    // （`Comment.article → ArticleEntity`、`ArticleEntity.author → UserEntity`、
    // `UserEntity.favorites → ArticleEntity`），`@OneToMany` 是反向声明、不重复建边。
    assert!(
        class_links_to_named(&b, "Comment", "References", "ArticleEntity"),
        "Comment 应经 References 连到 ArticleEntity"
    );
    assert!(
        class_links_to_named(&b, "ArticleEntity", "References", "UserEntity"),
        "ArticleEntity 应经 References 连到 UserEntity"
    );
    assert!(
        class_links_to_named(&b, "UserEntity", "References", "ArticleEntity"),
        "UserEntity 应经 ManyToMany(favorites) 连到 ArticleEntity"
    );

    // 表级外键：`References`（实体 → 实体）经 `Project` 投影成 `ForeignKey`（表 → 表）。
    // `Comment` 是 `@Entity()` 无实参 → 没有 Table，它的外键不入图（宁可缺不可猜）。
    assert!(
        node_links_to_named(&b, "Table", "article", "ForeignKey", "user"),
        "article 表应经 ForeignKey 连到 user 表"
    );
    assert!(
        node_links_to_named(&b, "Table", "user", "ForeignKey", "article"),
        "user 表应经 ForeignKey 连到 article 表（@ManyToMany favorites）"
    );
    eprintln!(
        "NestJS 真实样本契约数 = {}，表示例：{:?}，表数 = {}，列数 = {}",
        names.len(),
        &names[..names.len().min(8)],
        tables.len(),
        cols.len()
    );
}

/// 模块级 `MiddlewareConsumer`：`consumer.apply(AuthMiddleware).forRoutes({path, method})`
/// 必须把 `AuthMiddleware` 落成 `Middleware` 语义节点，并让受它保护的路由经 `PassesThrough`
/// 指向它（复现此前"对象字面量实参 0 命中"的回归）。
#[test]
fn nestjs_real_sample_consumer_middleware() {
    let Some((nest_root, _)) = node_samples() else {
        eprintln!("跳过：未找到 NestJS 真实样本（samples/nestjs-realworld-example-app）");
        return;
    };
    let Some(b) = common::graph_with_root(&nest_root, ProjectConfig::default()) else {
        panic!("真实样本建图应成功");
    };
    let mws: Vec<String> = nodes_of_kind(&b, "Middleware")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        mws.iter().any(|n| n == "AuthMiddleware"),
        "应产出 Middleware 节点 AuthMiddleware，实际：{mws:?}"
    );
    // 至少一条契约经 `PassesThrough` 连到 AuthMiddleware（输入边）。
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
        "至少一条 HttpContract 应经 PassesThrough 连到 AuthMiddleware"
    );
    let count = nodes_of_kind(&b, "Middleware")
        .into_iter()
        .filter(|m| m.name == "AuthMiddleware")
        .flat_map(|m| b.store.edges_of(m.id, EdgeDirection::Incoming).expect("edges"))
        .filter(|e| e.kind.as_str() == "PassesThrough")
        .count();
    eprintln!("NestJS 真实样本 AuthMiddleware 守卫的契约数 = {count}");
}

#[test]
fn express_real_sample_produces_route_contracts() {
    let Some((_, expr_root)) = node_samples() else {
        eprintln!("跳过：未找到 Express 真实样本（samples/hackathon-starter）");
        return;
    };
    let Some(b) = common::graph_with_root(&expr_root, ProjectConfig::default()) else {
        panic!("真实样本建图应成功");
    };
    let contracts = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert!(
        !names.is_empty(),
        "真实 Express 样本应产出 HttpContract，实际节点为空"
    );
    // `app.get('/login', ...)` 应落成
    assert!(
        names.iter().any(|n| n.contains("login")),
        "应包含 /login 契约，实际：{names:?}"
    );
    eprintln!("Express 真实样本契约数 = {}，示例：{:?}", names.len(), &names[..names.len().min(8)]);
}
