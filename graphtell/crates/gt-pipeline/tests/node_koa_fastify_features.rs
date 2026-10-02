//! End-to-end self-check of Node backend **Koa / Fastify route contracts**.
//!
//! Both are "zero parser change": to `JsFrontendParser` a route registration is just an ordinary member call
//! (`receiver.method(path, handler)`), the same shape as Express, so only an FKB rule is needed.
//! This verifies that an `HttpContract`'s **method + path** are correct.

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

fn synthetic_root(tag: &str, deps: &str, file: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-{tag}-{}-{}",
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
        format!("{{\n  \"name\": \"app\",\n  \"dependencies\": {{ {deps} }}\n}}\n"),
    )
    .expect("write package.json");
    std::fs::write(dir.join("src").join(file), src).expect("write source");
    dir
}

fn contract_names(b: &common::Built) -> Vec<String> {
    let mut names: Vec<String> = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("HttpContract".to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .iter()
        .map(|n: &Node| n.name.clone())
        .collect();
    names.sort();
    names
}

#[test]
fn koa_routes_become_http_contracts() {
    let dir = synthetic_root(
        "koa",
        r#""koa": "^2.14.0", "@koa/router": "^10.1.1""#,
        "routes.js",
        r#"const Router = require('@koa/router');

const router = new Router({ prefix: '/users' });
const admin = new Router();

router.get('/', listUsers);
router.post('/', createUser);
router.get('/:id', getUser);
admin.delete('/:id', removeUser);
admin.all('/health', health);

module.exports = { router, admin };
"#,
    );
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };
    let names = contract_names(&b);
    assert_eq!(
        names,
        vec![
            "ANY /health",
            "DELETE /:id",
            "GET /",
            "GET /:id",
            "POST /",
        ],
        "Koa contracts do not match: {names:?}"
    );
}

#[test]
fn fastify_routes_become_http_contracts() {
    let dir = synthetic_root(
        "fastify",
        r#""fastify": "^4.0.0""#,
        "app.js",
        r#"const fastify = require('fastify')();

fastify.get('/users', listUsers);
fastify.post('/users', createUser);
server.put('/users/:id', replaceUser);

module.exports = fastify;
"#,
    );
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };
    let names = contract_names(&b);
    assert_eq!(
        names,
        vec!["GET /users", "POST /users", "PUT /users/:id"],
        "Fastify contracts do not match: {names:?}"
    );
}
