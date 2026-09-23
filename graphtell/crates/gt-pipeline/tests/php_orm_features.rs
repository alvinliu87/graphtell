//! PHP ORM（Laravel / ThinkPHP）**模型关联与表级外键**的端到端自检。
//!
//! 覆盖「关系方法 → `References`（模型 → 模型）→ `Project` 投影成 `ForeignKey`（表 → 表）」
//! 这条链。用的是合成工程（无需外部样本），且与 Node 侧策略刻意不同：
//! TypeORM 的 `@ManyToOne` / `@OneToMany` 通常成对写，故 Node 侧只建外键持有方；
//! Laravel / ThinkPHP **常只声明一侧**，故这里两侧都建（宁可冗余也不漏）。

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// 合成 Laravel 工程：`User hasMany Post` + `Post belongsTo User`（两侧都声明）。
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

/// 名为 `from` 的节点是否有指定种类的出边连到名为 `target` 的节点。
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
        panic!("合成 Laravel 工程建图应成功");
    };

    // 模型 → 表（既有能力）：`User` / `Post` 经 snake_plural + singularize 落成 user / post
    let tables: Vec<String> = nodes_of_kind(&b, "Table")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        tables.iter().any(|t| t == "user") && tables.iter().any(|t| t == "post"),
        "应产出 user / post 两张表，实际：{tables:?}"
    );

    // 模型关联：两侧都建（`hasMany` 与反向 `belongsTo`）
    assert!(
        links_to_named(&b, "Class", "User", "References", "Post"),
        "User 应经 References 连到 Post（hasMany）"
    );
    assert!(
        links_to_named(&b, "Class", "Post", "References", "User"),
        "Post 应经 References 连到 User（belongsTo）"
    );

    // 表级外键：`References` 经 `Project` 投影，两端各沿 `MapsTo` 落到表
    assert!(
        links_to_named(&b, "Table", "user", "ForeignKey", "post"),
        "user 表应经 ForeignKey 连到 post 表"
    );
    assert!(
        links_to_named(&b, "Table", "post", "ForeignKey", "user"),
        "post 表应经 ForeignKey 连到 user 表"
    );
}

/// 权威 schema（SQL 安装脚本）的列应沉淀成 `Column` 图节点：
/// `Table(users) --HasColumn--> Column(users.email)`。
///
/// 覆盖的是**另一条列来源**：不是 PHP 模型的字段声明（PHP ORM 通常不声明字段），
/// 而是 DDL 这份权威结构 —— 影响面因此能下到字段级，而不只是止于表。
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
    // DDL 用**复数**表名 `users`（Laravel 惯例），而表节点名经 singularize 是 `user`
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
    // 模型类：让 `extends Model` 约定先落出 Table 节点（名为 user）
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
        panic!("建图应成功");
    };

    let cols: Vec<String> = nodes_of_kind(&b, "Column").iter().map(|n| n.name.clone()).collect();
    assert!(
        cols.iter().any(|c| c == "user.email"),
        "应把 DDL 的列沉淀成 Column 节点（user.email），实际：{cols:?}"
    );
    // 列挂在表下（`Table --HasColumn--> Column`），而不是散着
    let owned = nodes_of_kind(&b, "Table")
        .iter()
        .flat_map(|t| b.store.edges_of(t.id, EdgeDirection::Outgoing).expect("edges"))
        .filter(|e| e.kind.as_str() == "HasColumn")
        .count();
    assert!(owned > 0, "表应经 HasColumn 指向其列");
}

/// Laravel migration 的列应进 `schema` 符号表、并沉淀成 `Column` 节点。
///
/// 两个刻意锁定的行为：
/// * **修饰符不能当列**：`->comment('说明')` / `->after('slug')` 也带字符串实参，
///   若"取第一个字符串实参"就会被误当列名（这正是解析走**方法白名单**的原因）。
/// * **无参声明按约定补列名**：`$table->id()` → `id`、`->timestamps()` → `created_at` / `updated_at`。
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
    // 模型类：`extends Model` 约定先落出 Table 节点（名为 post）
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
    // 变更迁移：`dropColumn` 的实参是**要删的列**，绝不能当成列声明
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
        panic!("建图应成功");
    };

    let cols: Vec<String> = nodes_of_kind(&b, "Column").iter().map(|n| n.name.clone()).collect();
    // 声明的列（含无参约定补出的 id / created_at / updated_at）
    for want in ["post.title", "post.body", "post.slug", "post.user_id", "post.id", "post.created_at"] {
        assert!(
            cols.iter().any(|c| c == want),
            "应产出列 {want}，实际：{cols:?}"
        );
    }
    // 非列声明的字符串实参绝不能被当成列（`dropColumn` 是"要删的列"）
    assert!(
        !cols.iter().any(|c| c == "post.legacy"),
        "`dropColumn('legacy')` 不该产出列，实际：{cols:?}"
    );
}

/// `morphTo()` 这类**没有类常量实参**的关系不该建悬空边（取不到就跳过）。
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
        panic!("建图应成功");
    };
    let dangling = nodes_of_kind(&b, "Class")
        .iter()
        .filter(|n| n.name == "Image")
        .flat_map(|n| b.store.edges_of(n.id, EdgeDirection::Outgoing).expect("edges"))
        .filter(|e| e.kind.as_str() == "References")
        .count();
    assert_eq!(
        dangling, 0,
        "`morphTo()` 取不到目标类，不应建 References 边"
    );
}
