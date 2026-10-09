//! Hard-proof that a backend **template** URL-form route reference connects to the backend endpoint it
//! calls.
//!
//! A Django `path('/api/orders', ...)` route declares an `HttpContract` (method `ANY`). A Django template
//! `<form action="/api/orders">` — once `.html` is routed to the Python parser — is extracted as a synthetic
//! `template-route` call that the universal FKB rule (`fkb/universal/common.yaml::template-route-http-contract`)
//! turns into the *same* `HttpContract`. Because both live in the one backend sub-project, they merge onto a
//! single node, so the view's `CallsHttp` edge lands on the real endpoint the route declares (`HandledBy`).
//!
//! This pins the whole "view -> backend contract" link end-to-end, with no external sample checkout.

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_django_project() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("graphtell-django-bridge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp/templates")).expect("mkdir");

    // `pyproject.toml` (django dep) is both the sub-project marker (-> language=python) and the detector
    // manifest, so `fkb/python/django.yaml` is recognised.
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"demo\"\ndependencies = [\"django>=4.2\"]\n",
    )
    .expect("write pyproject");
    std::fs::write(dir.join("requirements.txt"), "django>=4.2\n").expect("write requirements");
    std::fs::write(dir.join("myapp/__init__.py"), "").expect("write init");

    // The route: `path('/api/orders', store_order)` -> HttpContract `ANY /api/orders` with HandledBy.
    // Use a bare-name view ref (the proven django_features.rs pattern) so the parser resolves the entity.
    std::fs::write(
        dir.join("myapp/urls.py"),
        "from django.urls import path\nfrom .views import store_order\nurlpatterns = [path('/api/orders', store_order, name='order-store')]\n",
    )
    .expect("write urls");
    std::fs::write(
        dir.join("myapp/views.py"),
        "from django.http import JsonResponse\n\ndef store_order(request):\n    return JsonResponse({})\n",
    )
    .expect("write views");

    // The view template. It must contain `{%` / `{{` so `is_python_template` treats it as a template; the
    // `<form action>` is the URL-form route ref that must bridge to the endpoint.
    std::fs::write(
        dir.join("myapp/templates/order_form.html"),
        "<form action=\"/api/orders\" method=\"post\">{% csrf_token %}<input name=\"q\"></form>\n",
    )
    .expect("write template");
    dir
}

#[test]
fn django_template_form_action_bridges_to_route_contract() {
    let root = synthetic_django_project();
    let built = common::graph_with_root(&root, ProjectConfig::default()).expect("graph build");
    let store = &built.store;

    // The route and the template collapse onto ONE HttpContract node (identity `ANY /api/orders`).
    let contracts: Vec<gt_domain::model::Node> = store
        .query_nodes(&NodeFilter {
            project_id: built.project.id,
            kind: Some(NodeKind(NodeKind::HTTP_CONTRACT.to_string())),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .unwrap()
        .into_iter()
        .filter(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some("ANY /api/orders"))
        .collect();
    assert_eq!(
        contracts.len(),
        1,
        "route + template must merge to ONE contract (route declares it, template calls it)"
    );
    let contract = &contracts[0];

    // The view template reaches the endpoint via an incoming `CallsHttp` edge (template -> contract); the
    // backend route declares it via an outgoing `HandledBy` edge (contract -> handler). Both on the same
    // node proves the route + the template collapsed onto one endpoint and the view -> backend link is closed.
    let inc = store.edges_of(contract.id, EdgeDirection::Incoming).unwrap();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "CallsHttp"),
        "the view template must reach the endpoint (CallsHttp) — the bridge is closed"
    );
    let out = store.edges_of(contract.id, EdgeDirection::Outgoing).unwrap();
    assert!(
        out.iter().any(|e| e.kind.as_str() == "HandledBy"),
        "the backend route must declare the endpoint (HandledBy)"
    );
}
