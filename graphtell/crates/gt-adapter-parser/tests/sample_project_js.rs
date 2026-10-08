//! Parser tests against the real Node.js sample corpus (`nestjs-realworld-example-app`, a NestJS project).
//!
//! These mirror `sample_project_php.rs` / `sample_project_java.rs`: they pin the *facts* tree-sitter
//! extracts from real TypeScript source. The `js_smoke.rs` file already pins the same contracts against
//! tiny synthesized snippets; this file pins them on a real controller where the interesting failures
//! live — a 100+ line file mixing class-level / method-level / parameter decorators, `async` methods with
//! return-type annotations, and constructor parameter properties:
//!
//! * `@Controller('articles')` class-level decorator routing onto the class FQN
//! * method-level `@Get()` / `@Get('feed')` decorators routing onto the `Class.method` FQN, including the
//!   bare `@Get()` (no argument) shape
//! * constructor parameter-property injection (`private readonly articleService: ArticleService`) becoming
//!   an `@Inject` call site carrying the injected type

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{Language, SyntaxFacts};
use gt_domain::port::ParserRegistry;
use gt_sample_support::{missing_hint_named, node_sample_inner_dir, node_sample_root};

fn parse_js(rel: &str) -> Option<SyntaxFacts> {
    let root = node_sample_root()?;
    let path = root.join(node_sample_inner_dir()).join(rel);
    let content = std::fs::read_to_string(&path).ok()?;
    let registry = DefaultParserRegistry::new();
    let parser = registry
        .parser_for(&Language::new(Language::JAVASCRIPT))
        .expect("the javascript parser is registered");
    parser.parse(path.to_str().unwrap(), &content).ok()
}

#[test]
#[ignore = "needs the node sample (nestjs-realworld-example-app), which is not committed (too large to ship with the repo)"]
fn parses_controller_decorators_and_routes() {
    let Some(facts) = parse_js("src/article/article.controller.ts") else {
        panic!("{}", missing_hint_named("node"));
    };
    let class = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "ArticleController")
        .expect("expected the ArticleController class to be parsed");
    assert_eq!(class.fqn, "ArticleController", "a TS class FQN is the class name");

    // `@Controller('articles')` is class-level -> call site owned by the class, carrying the route prefix.
    let ctrl = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "@Controller")
        .expect("the class-level @Controller must become a call site");
    assert_eq!(
        ctrl.owner_fqn, "ArticleController",
        "a class-level decorator must land on the class FQN"
    );
    assert!(
        ctrl.args.iter().any(|a| a.as_str() == Some("articles")),
        "the \"articles\" route prefix must be carried: {:?}",
        ctrl.args
    );

    // `@Get()` on `findAll` is the bare (no-argument) shape -> owned by the method FQN, no args.
    let get_empty = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "@Get" && c.owner_fqn == "ArticleController.findAll")
        .expect("the bare @Get() on findAll must become a call site");
    assert!(
        get_empty.args.is_empty(),
        "the bare @Get() carries no arguments, got: {:?}",
        get_empty.args
    );

    // `@Get('feed')` on `getFeed` carries the path segment and lands on the method FQN.
    let get_feed = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "@Get" && c.owner_fqn == "ArticleController.getFeed")
        .expect("the @Get('feed') on getFeed must become a call site");
    assert!(
        get_feed.args.iter().any(|a| a.as_str() == Some("feed")),
        "the \"feed\" path argument must be carried: {:?}",
        get_feed.args
    );

    // A method-level @Get must land on `Class.method`, never be over-attributed to the class FQN — otherwise FKB
    // would attach the handler to the wrong node.
    assert_ne!(
        get_feed.owner_fqn, "ArticleController",
        "a method-level @Get must land on Class.method, not be over-attributed to the class FQN"
    );

    // The bare @Get() on findAll must not be pre-concatenated with the class `articles` prefix at parse time —
    // route composition (class prefix + method path) is a later FKB step, not the parser's job.
    assert!(
        !get_empty.args.iter().any(|a| a.as_str() == Some("articles")),
        "the bare @Get() must not carry the class \"articles\" prefix"
    );

    // `@Param('id')` and other parameter decorators sit *inside* `parameters`, so they must NOT be collected as
    // call sites (a real over-extraction guard, src/js.rs:547). The real controller mixes such parameter
    // decorators in, so this pins the guard on genuine source rather than a tiny snippet.
    assert!(
        facts.call_sites.iter().all(|c| c.callee_text != "@Param"),
        "parameter decorators must not be collected as call sites: {:?}",
        facts
            .call_sites
            .iter()
            .map(|c| &c.callee_text)
            .collect::<Vec<_>>()
    );
}

/// Constructor parameter properties (`constructor(private readonly articleService: ArticleService)`) feed
/// NestJS DI; the injected type must surface as an `@Inject` call site with `entity = "ArticleService"`,
/// owned by the class — so FKB can build a `DependsOn` edge.
#[test]
#[ignore = "needs the node sample (nestjs-realworld-example-app), which is not committed (too large to ship with the repo)"]
fn parses_constructor_injection_as_inject_call_site() {
    let Some(facts) = parse_js("src/article/article.controller.ts") else {
        panic!("{}", missing_hint_named("node"));
    };
    let inject = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "@Inject")
        .expect("the constructor injection point must be collected");
    assert_eq!(
        inject.owner_fqn, "ArticleController",
        "a constructor injection point is owned by the class"
    );
    assert_eq!(
        inject.entity.as_deref(),
        Some("ArticleService"),
        "the injected field type must be the entity"
    );
}

/// NEGATIVE: the class-level `@Controller('articles')` prefix must be attributed to the **class** FQN and must NOT
/// be duplicated onto every method-level `@Get` site. A regression that re-attaches the prefix to each handler
/// method would double the prefix and mis-route FKB's `HandledBy` edges onto phantom `Class.method` nodes carrying
/// `articles`. The method routes carry `feed` / nothing at parse time — never the class prefix — so a method-owned
/// `@Get` carrying `articles` is a definite over-attribution.
#[test]
#[ignore = "needs the node sample (nestjs-realworld-example-app), which is not committed (too large to ship with the repo)"]
fn class_level_controller_prefix_is_not_owned_by_methods() {
    let Some(facts) = parse_js("src/article/article.controller.ts") else {
        panic!("{}", missing_hint_named("node"));
    };
    let methods_carrying_class_prefix = facts
        .call_sites
        .iter()
        .filter(|c| {
            c.callee_text == "@Get"
                && c.owner_fqn != "ArticleController"
                && c.args.iter().any(|a| a.as_str() == Some("articles"))
        })
        .count();
    assert_eq!(
        methods_carrying_class_prefix, 0,
        "the class-level \"articles\" prefix must not be duplicated onto method-level @Get call sites"
    );
}
