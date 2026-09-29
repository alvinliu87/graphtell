//! 视图层「表 → 列」的端到端自检（合成工程，CI 可跑）。
//!
//! 列**刻意不作为独立节点画进折叠视图**（`Column` 不是语义节点 —— 几十张表 × 十几列
//! 会把画布撑爆）。但"点开一张表看看有哪些字段"是刚需，故由 `NodeView.columns`
//! 把列作为**节点属性**带出来：折叠视图里不占位，点开表才看到。
//!
//! 两条来源路径都要覆盖：
//! * PHP：`Table --HasColumn--> Column`（列挂在表下，来自权威 schema / migration）；
//! * Node / TypeORM：`Table <--MapsTo-- 实体类 --HasColumn--> Column`（列挂在实体类上）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, ViewService};
use gt_domain::model::{NewProject, ProjectId};
use gt_domain::port::{GraphQuery, NodeFilter, NoopObserver, Persistence, RuleProvider, SystemClock};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}

struct Built {
    container: Container,
    project_id: ProjectId,
}

/// 用合成工程跑一次完整建图（PHP：SQL 安装脚本；Node：TypeORM 实体）。
fn build(root: PathBuf) -> Built {
    let data_dir = std::env::temp_dir().join(format!(
        "graphtell-viewcols-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&data_dir).expect("mkdir data");

    let config = AppConfig {
        data_dir,
        fkb_dir: Some(workspace_root().join("fkb")),
        views_dir: Some(workspace_root().join("views")),
        rules_dir: Some(workspace_root().join("rules")),
        bind: "127.0.0.1".into(),
        port: 0,
        ui_dir: None,
    };
    let container = Container::new(config).expect("容器装配不应失败");
    let projects = ProjectService::new(
        container.store.clone() as Arc<dyn Persistence>,
        Arc::new(SystemClock),
    );
    let pipeline = PipelineService::new(
        container.store.clone() as Arc<dyn Persistence>,
        Arc::clone(&container.deps),
        Arc::clone(&container.rules) as Arc<dyn RuleProvider>,
    );
    let project = projects
        .create(NewProject {
            name: "view-columns".into(),
            root_path: root,
            description: None,
            config: None,
        })
        .expect("创建工程不应失败");
    pipeline
        .run(project.id, &NoopObserver)
        .expect("建图不应失败");
    Built { container, project_id: project.id }
}

/// 名为 `table` 的 Table 节点的 `columns`（按名取节点 → 取它的视图）。
fn columns_of_table(b: &Built, table: &str) -> Option<Vec<String>> {
    let store = b.container.store.clone();
    let nodes = store
        .query_nodes(&NodeFilter {
            project_id: b.project_id,
            kind: Some(gt_domain::model::NodeKind("Table".into())),
            name_contains: None,
            limit: Some(100),
            offset: Some(0),
        })
        .expect("query");
    let node = nodes.into_iter().find(|n| n.name == table)?;
    let views = ViewService::new(store.clone(), b.container.views());
    // 以该表为中心取对象视角（折叠视图）：`center` 就是它的 NodeView
    let view = views
        .object_view(b.project_id, "table", node.id, Some(1))
        .ok()?;
    view.center.columns
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphtell-vc-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

/// PHP：列挂在表下（`Table --HasColumn--> Column`）。
#[test]
fn php_table_view_carries_columns() {
    let d = temp_dir("php");
    std::fs::create_dir_all(d.join("app/Models")).expect("mkdir");
    std::fs::write(
        d.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("composer");
    std::fs::write(
        d.join("app/Models/User.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class User extends Model
{
}
"#,
    )
    .expect("model");
    std::fs::write(
        d.join("install.sql"),
        "CREATE TABLE `users` (\n  `id` int(11) NOT NULL,\n  `email` varchar(120) NOT NULL,\n  `phone` varchar(30) DEFAULT NULL\n);\n",
    )
    .expect("sql");

    let b = build(d.clone());
    let cols = columns_of_table(&b, "user").unwrap_or_default();
    assert!(
        cols.iter().any(|c| c == "email") && cols.iter().any(|c| c == "phone"),
        "user 表的视图应带上列 email / phone（且是裸列名），实际：{cols:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Node / TypeORM：列挂在实体类上，表要经 `MapsTo` 反向绕一跳。
#[test]
fn node_table_view_carries_columns_via_entity() {
    let d = temp_dir("node");
    std::fs::create_dir_all(d.join("src")).expect("mkdir");
    std::fs::write(
        d.join("package.json"),
        r#"{ "dependencies": { "typeorm": "^0.3.0" } }"#,
    )
    .expect("package.json");
    std::fs::write(
        d.join("src/user.entity.ts"),
        r#"import { Entity, Column, PrimaryGeneratedColumn } from 'typeorm';

@Entity('user')
export class UserEntity {
  @PrimaryGeneratedColumn()
  id: number;

  @Column()
  username: string;

  @Column()
  email: string;
}
"#,
    )
    .expect("entity");

    let b = build(d.clone());
    let cols = columns_of_table(&b, "user").unwrap_or_default();
    assert!(
        cols.iter().any(|c| c == "username") && cols.iter().any(|c| c == "email"),
        "user 表的视图应经 MapsTo 取到实体类的列 username / email，实际：{cols:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
