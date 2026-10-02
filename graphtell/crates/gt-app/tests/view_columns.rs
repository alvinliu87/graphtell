//! End-to-end self-check of the view layer's "table -> columns" (synthetic project, runs in CI).
//!
//! Columns are **deliberately not drawn as independent nodes in the folded view** (`Column` is not a semantic node
//! — dozens of tables x a dozen columns each would burst the canvas). But "open a table and see which fields it
//! has" is a hard requirement, so `NodeView.columns` carries columns out as a **node property**: they occupy no
//! space in the folded view and only appear once the table is opened.
//!
//! Both source paths are covered:
//! * PHP: `Table --HasColumn--> Column` (columns hang off the table, from the authoritative schema / migration);
//! * Node / TypeORM: `Table <--MapsTo-- entity class --HasColumn--> Column` (columns hang off the entity class).

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

/// Run one complete graph build on a synthetic project (PHP: an SQL install script; Node: a TypeORM entity).
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
    let container = Container::new(config).expect("container assembly should not fail");
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
        .expect("creating the project should not fail");
    pipeline
        .run(project.id, &NoopObserver)
        .expect("the graph build should not fail");
    Built { container, project_id: project.id }
}

/// The `columns` of the Table node named `table` (find the node by name -> take its view).

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
    // Take the object perspective centred on that table (folded view): `center` is its NodeView
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

/// PHP: columns hang off the table (`Table --HasColumn--> Column`).
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
        "the view of the user table should carry the columns email / phone (as bare column names), got: {cols:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Node / TypeORM: columns hang off the entity class, so the table has to detour one hop back through `MapsTo`.
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
        "the view of the user table should get the entity class columns username / email via MapsTo, got: {cols:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
