//! End-to-end proof that each backend-template parser's URL-form route reference (`<form action>` /
//! `<a href>` / `link_to` / `url()`) reaches a synthesised `HttpContract` via an incoming `CallsHttp`
//! edge, driven by the language-agnostic universal rule
//! (`fkb/universal/common.yaml::template-route-http-contract`).
//!
//! The Django hard-proof (`django_template_route_bridge.rs`) additionally proves the template contract
//! *merges* with a backend route-declared contract (same scope, `ANY` ↔ concrete). These tests prove
//! the other half — that the universal rule fires for every template parser that emits the synthetic
//! `template-route` call site — which is exactly the regression class that was broken when the rule's
//! `path` was changed to read `field: "url"`: only the arguments of the *parser-emitted* call site matter
//! here, and every parser must pack `url` + `method: ANY` into `args[0]`.

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// Build the graph for `root` and assert that exactly one `HttpContract` with identity `ANY <path>` is
/// synthesised and reached by the template via an incoming `CallsHttp` edge.
fn assert_template_bridges_to_contract(root: &std::path::Path, path: &str) {
    let built = common::graph_with_root(root, ProjectConfig::default()).expect("graph build");
    let store = &built.store;
    let identity = format!("ANY {}", path);

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
        .filter(|n| n.identity.as_ref().map(|i| i.value.as_str()) == Some(identity.as_str()))
        .collect();
    assert_eq!(
        contracts.len(),
        1,
        "exactly one HttpContract {identity} must be synthesised from the template route ref"
    );
    let contract = &contracts[0];

    let inc = store.edges_of(contract.id, EdgeDirection::Incoming).unwrap();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "CallsHttp"),
        "the view template must reach the endpoint via an incoming CallsHttp edge (universal rule fired)"
    );
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("graphtell-bridge-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    dir
}

#[test]
fn jsp_form_action_bridges_to_contract() {
    let root = scratch_dir("jsp");
    // `pom.xml` marks a Java sub-project; the `.jsp` file is routed to the Java template extractor.
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom");
    std::fs::write(
        root.join("order_form.jsp"),
        "<form action=\"/api/orders\" method=\"post\"><input name=\"q\"></form>\n",
    )
    .expect("write jsp");
    assert_template_bridges_to_contract(&root, "/api/orders");
}

#[test]
fn erb_link_to_bridges_to_contract() {
    let root = scratch_dir("erb");
    std::fs::write(root.join("Gemfile"), "source \"https://rubygems.org\"\n").expect("write gemfile");
    std::fs::write(
        root.join("order_form.html.erb"),
        "<a href=\"/api/orders\">Orders</a>\n",
    )
    .expect("write erb");
    assert_template_bridges_to_contract(&root, "/api/orders");
}

#[test]
fn razor_href_bridges_to_contract() {
    let root = scratch_dir("razor");
    std::fs::write(root.join("App.csproj"), "<Project></Project>").expect("write csproj");
    std::fs::write(
        root.join("OrderForm.cshtml"),
        "<a href=\"/api/orders\">Orders</a>\n",
    )
    .expect("write cshtml");
    assert_template_bridges_to_contract(&root, "/api/orders");
}

#[test]
fn blade_form_action_bridges_to_contract() {
    let root = scratch_dir("blade");
    std::fs::write(root.join("composer.json"), "{\"require\":{}}").expect("write composer");
    std::fs::create_dir_all(root.join("resources/views")).expect("mkdir views");
    std::fs::write(
        root.join("resources/views/order_form.blade.php"),
        "<form action=\"/api/orders\" method=\"post\">@csrf<input name=\"q\"></form>\n",
    )
    .expect("write blade");
    assert_template_bridges_to_contract(&root, "/api/orders");
}

/// Full-bridge proof for Java/Spring: the JSP `<form action="/api/orders">` synthesises the template
/// `ANY /api/orders` contract (universal rule, reached via an incoming `CallsHttp`), while the controller's
/// `@PostMapping("/api/orders")` synthesises the route `POST /api/orders` contract with an outgoing
/// `HandledBy` to the controller (spring-boot `mapping-http-contract`). The two must merge (ANY ↔ concrete)
/// into a single node that is *both* reached by the view and handled by the controller — the bridge closed.
/// This is the same hard proof the Django test gives, but for a non-Django parser, guarding the
/// merge step that the parser-only assertions cannot.
#[test]
fn jsp_form_action_merges_with_spring_route_contract() {
    let root = scratch_dir("jsp-spring");
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom");
    // `src/main/java` triggers the spring-boot framework detector.
    std::fs::create_dir_all(root.join("src/main/java")).expect("mkdir java");
    std::fs::write(
        root.join("src/main/java/OrderController.java"),
        "import org.springframework.web.bind.annotation.*;\n\
         @RestController\n\
         public class OrderController {\n\
             @PostMapping(\"/api/orders\")\n\
             public void store() {}\n\
         }\n",
    )
    .expect("write controller");
    std::fs::write(
        root.join("order_form.jsp"),
        "<form action=\"/api/orders\" method=\"post\"><input name=\"q\"></form>\n",
    )
    .expect("write jsp");

    let built = common::graph_with_root(&root, ProjectConfig::default()).expect("graph build");
    let store = &built.store;
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
        .filter(|n| {
            n.identity
                .as_ref()
                .map(|i| i.value.ends_with("/api/orders"))
                .unwrap_or(false)
        })
        .collect();
    // Exactly one merged contract: the template's `ANY` and the route's `POST` collapse into a single node.
    assert_eq!(
        contracts.len(),
        1,
        "template ANY and route POST must merge into one contract"
    );
    let contract = &contracts[0];

    let inc = store.edges_of(contract.id, EdgeDirection::Incoming).unwrap();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "CallsHttp"),
        "the view template must reach the contract via an incoming CallsHttp edge"
    );
    let out = store.edges_of(contract.id, EdgeDirection::Outgoing).unwrap();
    assert!(
        out.iter().any(|e| e.kind.as_str() == "HandledBy"),
        "the controller must handle the contract via an outgoing HandledBy edge"
    );
}

/// Full-bridge proof for PHP/Laravel: the Blade `<form action="/api/orders">` synthesises the template
/// `ANY /api/orders` contract (universal rule, incoming `CallsHttp`), while `routes/web.php` declares
/// `Route::post('/api/orders', [...])` which the laravel `route-http-contract` rule turns into
/// `POST /api/orders` with an outgoing `HandledBy` to the controller. The two must merge (ANY ↔ concrete)
/// into a single node that is *both* reached by the view and handled by the controller.
#[test]
fn blade_form_action_merges_with_laravel_route_contract() {
    let root = scratch_dir("blade-laravel");
    // composer.json marks a PHP sub-project; the `artisan` file triggers the laravel framework detector.
    std::fs::write(root.join("composer.json"), "{\"require\":{}}").expect("write composer");
    std::fs::write(root.join("artisan"), "").expect("write artisan");
    std::fs::create_dir_all(root.join("resources/views")).expect("mkdir views");
    std::fs::write(
        root.join("resources/views/order_form.blade.php"),
        "<form action=\"/api/orders\" method=\"post\">@csrf<input name=\"q\"></form>\n",
    )
    .expect("write blade");
    std::fs::create_dir_all(root.join("routes")).expect("mkdir routes");
    std::fs::write(
        root.join("routes/web.php"),
        "<?php\nRoute::post('/api/orders', [\\App\\Http\\Controllers\\OrderController::class, 'store']);\n",
    )
    .expect("write routes");
    std::fs::create_dir_all(root.join("app/Http/Controllers")).expect("mkdir controllers");
    std::fs::write(
        root.join("app/Http/Controllers/OrderController.php"),
        "<?php\nnamespace App\\Http\\Controllers;\nclass OrderController {\n    public function store() {}\n}\n",
    )
    .expect("write controller");

    let built = common::graph_with_root(&root, ProjectConfig::default()).expect("graph build");
    let store = &built.store;
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
        .filter(|n| {
            n.identity
                .as_ref()
                .map(|i| i.value.ends_with("/api/orders"))
                .unwrap_or(false)
        })
        .collect();
    // Exactly one merged contract: the template's `ANY` and the route's `POST` collapse into a single node.
    assert_eq!(
        contracts.len(),
        1,
        "template ANY and route POST must merge into one contract"
    );
    let contract = &contracts[0];

    let inc = store.edges_of(contract.id, EdgeDirection::Incoming).unwrap();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "CallsHttp"),
        "the view template must reach the contract via an incoming CallsHttp edge"
    );
    let out = store.edges_of(contract.id, EdgeDirection::Outgoing).unwrap();
    assert!(
        out.iter().any(|e| e.kind.as_str() == "HandledBy"),
        "the controller must handle the contract via an outgoing HandledBy edge"
    );
}

/// Full-bridge proof for Ruby/Rails: the ERB `<form action="/api/orders">` synthesises the template
/// `ANY /api/orders` contract (universal rule, incoming `CallsHttp`), while `config/routes.rb` declares
/// `post '/api/orders' => 'orders#create'` which the rails `route-http-contract` rule turns into
/// `POST /api/orders` with an outgoing `HandledBy` to the controller. The two must merge (ANY ↔ concrete)
/// into a single node that is *both* reached by the view and handled by the controller.
#[test]
fn erb_form_action_merges_with_rails_route_contract() {
    let root = scratch_dir("erb-rails");
    std::fs::write(root.join("Gemfile"), "source 'https://rubygems.org'\ngem 'rails'\n").expect("write gemfile");
    std::fs::create_dir_all(root.join("config")).expect("mkdir config");
    std::fs::write(
        root.join("config/routes.rb"),
        "Rails.application.routes.draw do\n  post '/api/orders' => 'orders#create'\nend\n",
    )
    .expect("write routes");
    std::fs::create_dir_all(root.join("app/views/orders")).expect("mkdir views");
    std::fs::write(
        root.join("app/views/orders/new.erb"),
        "<form action=\"/api/orders\" method=\"post\"><input name=\"q\"></form>\n",
    )
    .expect("write erb");
    std::fs::create_dir_all(root.join("app/controllers")).expect("mkdir controllers");
    std::fs::write(
        root.join("app/controllers/orders_controller.rb"),
        "class OrdersController < ApplicationController\n  def create; end\nend\n",
    )
    .expect("write controller");

    let built = common::graph_with_root(&root, ProjectConfig::default()).expect("graph build");
    let store = &built.store;
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
        .filter(|n| {
            n.identity
                .as_ref()
                .map(|i| i.value.ends_with("/api/orders"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        contracts.len(),
        1,
        "template ANY and route POST must merge into one contract"
    );
    let contract = &contracts[0];
    let inc = store.edges_of(contract.id, EdgeDirection::Incoming).unwrap();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "CallsHttp"),
        "the view template must reach the contract via an incoming CallsHttp edge"
    );
    let out = store.edges_of(contract.id, EdgeDirection::Outgoing).unwrap();
    assert!(
        out.iter().any(|e| e.kind.as_str() == "HandledBy"),
        "the controller must handle the contract via an outgoing HandledBy edge"
    );
}

/// Full-bridge proof for C# / ASP.NET Core: the Razor `<form action="/api/orders">` synthesises the
/// template `ANY /api/orders` contract (universal rule, incoming `CallsHttp`), while `Program.cs` declares
/// `app.MapPost("/api/orders", OrdersController.Create)` which the aspnetcore `route-http-contract` rule
/// turns into `POST /api/orders` with an outgoing `HandledBy` to the controller. The two must merge
/// (ANY ↔ concrete) into a single node that is *both* reached by the view and handled by the controller.
#[test]
fn razor_form_action_merges_with_aspnet_route_contract() {
    let root = scratch_dir("razor-aspnet");
    // `*.csproj` marks a C# sub-project; `Program.cs` triggers the aspnetcore framework detector.
    std::fs::write(root.join("Web.csproj"), "<Project></Project>").expect("write csproj");
    std::fs::create_dir_all(root.join("Views/Orders")).expect("mkdir views");
    std::fs::write(
        root.join("Views/Orders/Index.cshtml"),
        "<form action=\"/api/orders\" method=\"post\"><input name=\"q\"></form>\n",
    )
    .expect("write razor");
    std::fs::write(
        root.join("Program.cs"),
        "var app = builder.Build();\napp.MapPost(\"/api/orders\", OrdersController.Create);\n",
    )
    .expect("write program");

    let built = common::graph_with_root(&root, ProjectConfig::default()).expect("graph build");
    let store = &built.store;
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
        .filter(|n| {
            n.identity
                .as_ref()
                .map(|i| i.value.ends_with("/api/orders"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        contracts.len(),
        1,
        "template ANY and route POST must merge into one contract"
    );
    let contract = &contracts[0];
    let inc = store.edges_of(contract.id, EdgeDirection::Incoming).unwrap();
    assert!(
        inc.iter().any(|e| e.kind.as_str() == "CallsHttp"),
        "the view template must reach the contract via an incoming CallsHttp edge"
    );
    let out = store.edges_of(contract.id, EdgeDirection::Outgoing).unwrap();
    assert!(
        out.iter().any(|e| e.kind.as_str() == "HandledBy"),
        "the controller must handle the contract via an outgoing HandledBy edge"
    );
}
