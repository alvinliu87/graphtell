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

/// How many `HttpContract` nodes are owned by the backend sub-project (`side = backend`). In these samples there is
/// no frontend caller, so every contract must be backend-owned — the same ownership invariant `node_a_features`
/// and `link_sample` pin for backend-only endpoints.
fn backend_contract_count(b: &common::Built) -> usize {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("HttpContract".to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .into_iter()
        .filter(|n| n.properties.get("side").and_then(|v| v.as_str()) == Some("backend"))
        .count()
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
    // Ownership: every Koa contract must be a backend contract (no frontend caller in this sample).
    assert_eq!(
        backend_contract_count(&b),
        names.len(),
        "every Koa contract must be owned by the backend sub-project (side = backend)"
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
fastify.delete('/users/:id', deleteUser);
fastify.patch('/users/:id', patchUser);

module.exports = fastify;
"#,
    );
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };
    let names = contract_names(&b);
    assert_eq!(
        names,
        vec![
            "DELETE /users/:id",
            "GET /users",
            "PATCH /users/:id",
            "POST /users",
            "PUT /users/:id",
        ],
        "Fastify contracts do not match: {names:?}"
    );
    // Ownership: every Fastify contract must be a backend contract (no frontend caller in this sample).
    assert_eq!(
        backend_contract_count(&b),
        names.len(),
        "every Fastify contract must be owned by the backend sub-project (side = backend)"
    );
}
