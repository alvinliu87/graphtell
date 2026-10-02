//! End-to-end self-check of PHP ORM (Laravel / ThinkPHP) **model relations and table-level foreign keys**.
//!
//! Covers the chain "relation method -> `References` (model -> model) -> `Project` projected into `ForeignKey` (table -> table)".
//! Uses a synthetic project (no external sample needed), and deliberately differs from the Node-side strategy:
//! TypeORM's `@ManyToOne` / `@OneToMany` are usually written in pairs, so the Node side builds only the FK owner;
//! Laravel / ThinkPHP **often declare only one side**, so here both sides are built (redundant rather than missing).

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// Synthetic Laravel project: `User hasMany Post` + `Post belongsTo User` (both sides declared).
fn synthetic_laravel_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-orm-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/Models")).expect("mkdir");

    std::fs::write(
        dir.join("composer.json"),
        r#"{
  "name": "demo/shop",
  "require": { "laravel/framework": "^11.0" }
}
"#,
    )
    .expect("write composer.json");

    std::fs::write(
        dir.join("app/Models/User.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class User extends Model
{
    public function posts()
    {
        return $this->hasMany(Post::class);
    }
}
"#,
    )
    .expect("write User");

    std::fs::write(
        dir.join("app/Models/Post.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class Post extends Model
{
    public function user()
    {
        return $this->belongsTo(User::class);
    }
}
"#,
    )
    .expect("write Post");

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

/// Whether the node named `from` has an out-edge of a given kind reaching the node named `target`.
fn links_to_named(b: &common::Built, kind: &str, from: &str, edge: &str, target: &str) -> bool {
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

#[test]
fn laravel_relations_produce_references_and_table_foreign_keys() {
    let root = synthetic_laravel_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Laravel project graph build should succeed");
    };

    // Model -> table (existing capability): `User` / `Post` land on user / post via snake_plural + singularize
    let tables: Vec<String> = nodes_of_kind(&b, "Table")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        tables.iter().any(|t| t == "user") && tables.iter().any(|t| t == "post"),
        "expected user / post tables, got: {tables:?}"
    );

    // Model relation: both sides are built (`hasMany` and its inverse `belongsTo`)
    assert!(
        links_to_named(&b, "Class", "User", "References", "Post"),
        "User should connect to Post via References (hasMany)"
    );
    assert!(
        links_to_named(&b, "Class", "Post", "References", "User"),
        "Post should connect to User via References (belongsTo)"
    );

    // Table-level foreign key: `References` projected via `Project`, each end landing on a table along `MapsTo`
    assert!(
        links_to_named(&b, "Table", "user", "ForeignKey", "post"),
        "user table should connect to post table via ForeignKey"
    );
    assert!(
        links_to_named(&b, "Table", "post", "ForeignKey", "user"),
        "post table should connect to user table via ForeignKey"
    );
}

/// Columns of the authoritative schema (SQL install script) should settle into `Column` graph nodes:
/// `Table(users) --HasColumn--> Column(users.email)`.
///
/// This covers **a different column source**: not the PHP model's field declarations (PHP ORMs usually declare no fields),
/// but the DDL authoritative structure — so impact analysis can drill down to field level, not just stop at the table.
#[test]
fn schema_columns_become_column_nodes_on_tables() {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-schema-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("write composer.json");
    // DDL uses the **plural** table name `users` (Laravel convention), while the table node name is `user` after singularize
    std::fs::write(
        dir.join("install.sql"),
        r#"CREATE TABLE `users` (
  `id` int(11) NOT NULL AUTO_INCREMENT,
  `email` varchar(120) NOT NULL,
  `phone` varchar(30) DEFAULT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;
"#,
    )
    .expect("write install.sql");
    // Model class: let the `extends Model` convention first produce a Table node (named user)
    std::fs::create_dir_all(dir.join("app/Models")).expect("mkdir");
    std::fs::write(
        dir.join("app/Models/User.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class User extends Model
{
}
"#,
    )
    .expect("write User");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    let cols: Vec<String> = nodes_of_kind(&b, "Column").iter().map(|n| n.name.clone()).collect();
    assert!(
        cols.iter().any(|c| c == "user.email"),
        "DDL columns should settle into Column nodes (user.email), got: {cols:?}"
    );
    // Columns hang under the table (`Table --HasColumn--> Column`), not scattered
    let owned = nodes_of_kind(&b, "Table")
        .iter()
        .flat_map(|t| b.store.edges_of(t.id, EdgeDirection::Outgoing).expect("edges"))
        .filter(|e| e.kind.as_str() == "HasColumn")
        .count();
    assert!(owned > 0, "the table should point at its columns via HasColumn");
}

/// Columns of a Laravel migration should enter the `schema` symbol table and settle into `Column` nodes.
///
/// Two behaviours deliberately pinned:
/// * **A modifier must not be taken as a column**: `->comment('note')` / `->after('slug')` also carry a string argument,
///   and would be mistaken for a column name if we "take the first string argument" (that is exactly why parsing goes by a **method whitelist**).
/// * **A parameterless declaration completes the column name by convention**: `$table->id()` -> `id`, `->timestamps()` -> `created_at` / `updated_at`.
#[test]
fn migration_columns_become_column_nodes() {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-migration-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("database/migrations")).expect("mkdir migrations");
    std::fs::create_dir_all(dir.join("app/Models")).expect("mkdir models");
    std::fs::write(
        dir.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("write composer.json");
    // Model class: the `extends Model` convention first produces a Table node (named post)
    std::fs::write(
        dir.join("app/Models/Post.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class Post extends Model
{
}
"#,
    )
    .expect("write Post");
    std::fs::write(
        dir.join("database/migrations/2024_01_01_000000_create_posts_table.php"),
        r#"<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    public function up(): void
    {
        Schema::create('posts', function (Blueprint $table) {
            $table->id();
            $table->string('title')->comment('标题');
            $table->text('body')->nullable();
            $table->string('slug')->after('title')->unique();
            $table->unsignedBigInteger('user_id');
            $table->timestamps();
        });
    }
};
"#,
    )
    .expect("write migration");
    // A change migration: the argument of `dropColumn` is **the column to delete** — must never be treated as a column declaration
    std::fs::write(
        dir.join("database/migrations/2024_02_01_000000_drop_legacy_from_posts_table.php"),
        r#"<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    public function up(): void
    {
        Schema::table('posts', function (Blueprint $table) {
            $table->dropColumn('legacy');
        });
    }
};
"#,
    )
    .expect("write drop migration");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    let cols: Vec<String> = nodes_of_kind(&b, "Column").iter().map(|n| n.name.clone()).collect();
    // The declared columns (including id / created_at / updated_at completed by the parameterless convention)
    for want in ["post.title", "post.body", "post.slug", "post.user_id", "post.id", "post.created_at"] {
        assert!(
            cols.iter().any(|c| c == want),
            "expected column {want}, got: {cols:?}"
        );
    }
    // A string argument that is not a column declaration must never be treated as a column (`dropColumn` is "the column to delete")
    assert!(
        !cols.iter().any(|c| c == "post.legacy"),
        "`dropColumn('legacy')` should not produce a column, got: {cols:?}"
    );
}

/// A relation like `morphTo()` with **no class-constant argument** should not build a dangling edge (skip when it cannot be resolved).
#[test]
fn relation_without_class_argument_is_skipped() {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-orm-morph-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/Models")).expect("mkdir");
    std::fs::write(
        dir.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("write composer.json");
    std::fs::write(
        dir.join("app/Models/Image.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class Image extends Model
{
    public function imageable()
    {
        return $this->morphTo();
    }
}
"#,
    )
    .expect("write Image");

    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };
    let dangling = nodes_of_kind(&b, "Class")
        .iter()
        .filter(|n| n.name == "Image")
        .flat_map(|n| b.store.edges_of(n.id, EdgeDirection::Outgoing).expect("edges"))
        .filter(|e| e.kind.as_str() == "References")
        .count();
    assert_eq!(
        dangling, 0,
        "`morphTo()` cannot resolve the target class, should not build a References edge"
    );
}
