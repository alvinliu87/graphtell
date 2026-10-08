//! JavaScript / TypeScript parsing smoke test: verify that syntax fact extraction is correct through the public
//! registry.
//!
//! Mirrors `php_smoke.rs`: the unit tests in `src/js.rs` call `JsFrontendParser::new()` directly. The frontend
//! -> backend link (`frontend-js-common-http-contract`) relies on the parser turning an **axios member call**
//! (`axios.post('/api/list')`) into an HTTP call site carrying a normalised `(url, method)` — the member form was
//! a regression gap (receiver=`axios`, method uppercased `POST`). This pins it through `DefaultParserRegistry`,
//! the path the pipeline actually uses.

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{FactValue, Language, SyntaxFacts};
use gt_domain::port::ParserRegistry;

fn parse(path: &str, src: &str) -> SyntaxFacts {
    let reg = DefaultParserRegistry::new();
    let p = reg
        .parser_for(&Language::new(Language::JAVASCRIPT))
        .expect("javascript parser registered");
    p.parse(path, src).expect("parse ok")
}

/// The frontend -> backend link (`frontend-js-common-http-contract`) relies on `axios.post(...)` becoming an HTTP
/// call site with url + method. Guarded in `src/js.rs` directly; this pins it through the registry so the
/// member-form regression (missing HttpContract / CallsHttp) is caught on the path the pipeline uses.
#[test]
fn axios_member_call_normalises_to_http_contract_fact() {
    let facts = parse("sample.js", "axios.post('/api/list')");
    let call = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "axios")
        .expect("the axios call site must be captured");
    let args = call.args.first().expect("the call must carry arguments");
    let FactValue::Array(items) = args else {
        panic!("expected an array of (url, method) pairs, got {args:?}");
    };
    let url = items.iter().find(|(k, _)| k == "url").and_then(|(_, v)| v.as_str());
    let method = items
        .iter()
        .find(|(k, _)| k == "method")
        .and_then(|(_, v)| v.as_str());
    assert_eq!(url, Some("/api/list"), "the URL must be the first argument");
    assert_eq!(method, Some("POST"), "the member name must be uppercased into the HTTP method");
}

/// NestJS routes are declarative framework calls (same mechanism as Java annotations / Python decorators); FKB
/// matches `@Get('/users')` like a PHP/Java route. Guarded in `src/js.rs` directly; this pins it through the registry.
#[test]
fn nestjs_decorators_become_call_sites() {
    let src = "@Controller('cats')\nexport class CatsController {\n  @Get(':id')\n  findOne(@Param('id') id: string) { return 1; }\n}\n";
    let facts = parse("cats.controller.ts", src);
    let ctrl = facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some("Controller"))
        .expect("expected @Controller to be collected");
    assert_eq!(ctrl.owner_fqn, "CatsController");
    assert_eq!(ctrl.args.first().and_then(|a| a.as_str()), Some("cats"));
    assert_eq!(ctrl.callee_text, "@Controller");

    let get = facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some("Get"))
        .expect("expected @Get to be collected");
    assert_eq!(get.owner_fqn, "CatsController.findOne", "a method decorator lands on the Class.method FQN");
    assert_eq!(get.args.first().and_then(|a| a.as_str()), Some(":id"));
    assert_eq!(get.callee_text, "@Get");
    assert!(
        facts.call_sites.iter().all(|c| c.method.as_deref() != Some("Param")),
        "parameter decorators must not be collected as call sites"
    );
}

/// NestJS DI: `constructor(private readonly userService: UserService)` -> an `@Inject` call site with
/// `entity = "UserService"`, owned by the class — so FKB can build a `DependsOn` edge. Guarded in `src/js.rs`
/// directly; this pins it through the registry.
#[test]
fn constructor_param_injection_becomes_inject_call_site() {
    let src = "export class UserController {\n  constructor(private readonly userService: UserService) {}\n}\n";
    let facts = parse("user.controller.ts", src);
    let inject = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "@Inject")
        .expect("the constructor injection point must be collected");
    assert_eq!(inject.owner_fqn, "UserController");
    assert_eq!(inject.owner_class.as_deref(), Some("UserController"));
    assert_eq!(inject.entity.as_deref(), Some("UserService"));
}

/// TypeORM: `@Column()` on a field attaches to the **field FQN** (`UserEntity.username`) so same-named columns in
/// different entities don't merge, and `@ManyToOne(() => UserEntity)` carries the target entity (the basis of
/// relation edges). Guarded in `src/js.rs` directly; this pins both through the registry.
#[test]
fn typeorm_decorators_carry_field_fqn_and_target_entity() {
    let src = "export class UserEntity {\n  @PrimaryGeneratedColumn()\n  id: number;\n\n  @Column()\n  username: string;\n}\n\nexport class ArticleEntity {\n  @ManyToOne(type => UserEntity, user => user.articles)\n  author: UserEntity;\n}\n";
    let facts = parse("entities.ts", src);

    let cols: Vec<&str> = facts
        .call_sites
        .iter()
        .filter(|c| c.callee_text == "@Column" || c.callee_text == "@PrimaryGeneratedColumn")
        .map(|c| c.owner_fqn.as_str())
        .collect();
    assert_eq!(
        cols,
        vec!["UserEntity.id", "UserEntity.username"],
        "a field decorator must attach to the field FQN (Class.field), got: {cols:?}"
    );

    let rel: Vec<(&str, &str)> = facts
        .call_sites
        .iter()
        .filter(|c| c.callee_text == "@ManyToOne")
        .map(|c| (c.owner_fqn.as_str(), c.entity.as_deref().unwrap_or("<none>")))
        .collect();
    assert_eq!(
        rel,
        vec![("ArticleEntity.author", "UserEntity")],
        "a relation decorator must carry the target entity, got: {rel:?}"
    );
}

/// The normalised `(url, method)` of the HTTP call site in `src`, through the registry.
///
/// `method` defaults to GET only when the parser produced none — every HTTP shape always writes one.
fn http_of(src: &str) -> (Option<String>, String) {
    let facts = parse("x.ts", src);
    let c = facts
        .call_sites
        .iter()
        .find(|c| {
            c.args.iter().any(|a| {
                matches!(a, FactValue::Array(items) if items.iter().any(|(k, _)| *k == "url" || *k == "method"))
            })
        })
        .unwrap_or_else(|| panic!("an HTTP call site must normalise to url/method: {:?}", facts.call_sites));
    let FactValue::Array(items) = c.args.first().expect("args[0] must be the url/method array") else {
        panic!("args[0] must be an Array, got {:?}", c.args);
    };
    let url = items.iter().find(|(k, _)| k == "url").and_then(|(_, v)| v.as_str()).map(|s| s.to_string());
    let method = items
        .iter()
        .find(|(k, _)| k == "method")
        .and_then(|(_, v)| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "GET".to_string());
    (url, method)
}

/// `extract_http_args` has a **default GET** in three of its five branches (`Axios(None)`, `Obj`, `Fetch`).
/// Nothing pinned it: every existing case passes an explicit `method`, so a regression returning an empty
/// method (which FKB would turn into a nameless contract) or dropping the URL would have passed.
#[test]
fn http_shapes_without_a_method_default_to_get() {
    // Pinned as-is: the `Axios(None)` branch (a bare `axios(...)`, js.rs:889) reads its URL from the
    // `url:` **field** only — it never takes the first argument — so the callable form `axios('/api/bare')`
    // anchors no URL (the fact records `Unknown("<dynamic-url>")`). `axios(url)` is valid axios usage, so
    // this is a real limitation rather than an intended rejection; the fix would be
    // `first_arg_expr(..).or_else(|| field_expr(args, "url"))` in `extract_http_args`, exactly as the
    // `Axios(Some(verb))` / `Member` branches already do.
    assert_eq!(
        http_of("axios('/api/bare')"),
        (None, "GET".to_string()),
        "pinned: a bare `axios(url)` anchors no URL, but still yields the GET default"
    );
    assert_eq!(
        http_of("axios({ url: '/api/obj' })"),
        (Some("/api/obj".to_string()), "GET".to_string()),
        "the `axios({{ url }})` object form defaults to GET when it declares no method"
    );
    assert_eq!(
        http_of("fetch('/api/f')"),
        (Some("/api/f".to_string()), "GET".to_string()),
        "fetch without an options object defaults to GET"
    );
}

/// Member style with an **object** first argument: `axios.post({ url, method })`. The first argument is not a
/// literal, so `extract_http_args` must fall back to the object's `url:` field — and the member name stays the
/// HTTP method (it wins over any `method:` inside the object).
#[test]
fn http_member_style_falls_back_to_the_url_field() {
    assert_eq!(
        http_of("axios.post({ url: '/api/w', method: 'DELETE' })"),
        (Some("/api/w".to_string()), "POST".to_string()),
        "the URL comes from the object's `url:` field and the method from the member name"
    );
}

/// The frontend -> backend link keys off normalised `(url, method)` HTTP facts across several call shapes; a
/// non-HTTP call must never carry them. The axios *member* form is already pinned above; this pins the object
/// shapes FKB also matches and the guard that rejects ordinary calls. Guarded in `src/js.rs` directly.
#[test]
fn http_call_variants_normalise_to_contract() {
    let (url, method) = http_of("fetch('/api/x', { method: 'PUT' })");
    assert_eq!(url.as_deref(), Some("/api/x"));
    assert_eq!(method, "PUT", "fetch with an options object must lift `method` out");

    let (url, method) = http_of("uni.request({ url: '/api/delete', method: 'POST' })");
    assert_eq!(url.as_deref(), Some("/api/delete"));
    assert_eq!(method, "POST", "uni.request must be treated as an HTTP client");

    // A non-HTTP call must be collected but must NOT carry normalised url/method arguments.
    let facts = parse("y.ts", "doWork('a', b);");
    assert!(facts.call_sites.iter().any(|c| c.callee_text == "doWork"), "an ordinary call is still collected");
    assert!(
        facts.call_sites.iter().all(|c| !c.args.iter().any(|a| {
            matches!(a, FactValue::Array(items) if items.iter().any(|(k, _)| *k == "url" || *k == "method"))
        })),
        "a non-HTTP call must not carry normalised url/method arguments"
    );
}

/// NEGATIVE: only constructor parameters carrying an **access modifier** (`private` / `readonly` / …) are property
/// injections (`@Inject`); a plain typed parameter is an ordinary argument and must NOT become a dependency edge.
/// The positive `private readonly userService` form is pinned above; this pins the guard (`src/js.rs:505`) on the
/// registry path so a regression that injects every constructor parameter is caught.
#[test]
fn constructor_plain_param_without_modifier_is_not_injection() {
    let src = "export class X {\n  constructor(plain: Foo, private svc: Bar) {}\n}\n";
    let facts = parse("x.ts", src);
    let injects: Vec<&str> = facts
        .call_sites
        .iter()
        .filter(|c| c.callee_text == "@Inject")
        .map(|c| c.entity.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        injects,
        vec!["Bar"],
        "only the access-modifier param (svc: Bar) becomes @Inject; plain: Foo must not"
    );
}

/// `this.repo.save(x)` — the qualified-self spelling every NestJS service uses (mirror of the Java case in
/// `java_smoke.rs`). `src/js.rs:871` strips the `this.` so the kernel's variable-type resolver sees the bare
/// field name (`repo`) it recorded from the constructor; without it, `repo.save()` loses its read / write
/// classification. The callee keeps the source spelling.
#[test]
fn this_qualified_receiver_is_stripped_for_resolution() {
    let facts = parse(
        "user.service.ts",
        "class UserService {\n  save(u) { return this.repo.save(u); }\n}\n",
    );
    let call = facts
        .call_sites
        .iter()
        .find(|c| c.method.as_deref() == Some("save") && c.receiver.is_some())
        .expect("the `this.repo.save(...)` call must be captured");
    assert_eq!(
        call.receiver.as_deref(),
        Some("repo"),
        "`this.` must be stripped so the field lookup finds `repo`: {:?}",
        call.receiver
    );
    assert_eq!(
        call.callee_text, "this.repo.save",
        "the callee keeps the source spelling: {}",
        call.callee_text
    );
}

/// NEGATIVE: a `.get()` / `.post()` member call on a **non-HTTP receiver** (`cache`, `storage`, `list`) is still a
/// call site, but its arguments must NOT be normalised into a `{url, method}` array — only the closed set of HTTP
/// clients does that (`is_http_client_recv` + the `Other` branch, `src/js.rs:867`). Without this, every
/// `cache.get(...)` / `array.map(...)` would fabricate a phantom `HttpContract`.
#[test]
fn non_http_receiver_member_call_is_not_normalised_to_contract() {
    let facts = parse("y.ts", "cache.get('/api/x'); storage.post('/y'); list.map(z => z);");
    assert!(
        facts.call_sites.iter().any(|c| c.callee_text == "cache.get"),
        "an ordinary member call is still collected"
    );
    assert!(
        facts.call_sites.iter().any(|c| c.callee_text == "storage.post"),
        "an ordinary member call is still collected"
    );
    assert!(
        facts
            .call_sites
            .iter()
            .all(|c| !c.args.iter().any(|a| matches!(a, FactValue::Array(_)))),
        "a .get()/.post() on a non-HTTP receiver must not be normalised into an HttpContract"
    );
}

/// NEGATIVE: `axios.get(<variable>)` still carries the HTTP method (`GET`), but a **variable** URL must NOT be
/// fabricated into a literal — `extract_http_args` abandons a URL whose first segment is not statically anchored
/// (`src/js.rs:939`). Without this, `axios.get(BASE + '/api')` would invent a wrong endpoint shape.
#[test]
fn axios_member_call_with_variable_url_does_not_fabricate_literal() {
    let facts = parse("x.ts", "axios.get(someVar);");
    let call = facts
        .call_sites
        .iter()
        .find(|c| c.callee_text == "axios")
        .expect("the axios call site must be captured");
    let FactValue::Array(items) = call.args.first().expect("args[0] is the url/method array") else {
        panic!("expected a url/method array, got {:?}", call.args);
    };
    let method = items.iter().find(|(k, _)| k == "method").and_then(|(_, v)| v.as_str());
    let url = items.iter().find(|(k, _)| k == "url").and_then(|(_, v)| v.as_str());
    assert_eq!(method, Some("GET"), "the member name still becomes the HTTP method");
    assert_eq!(url, None, "a variable URL must not be fabricated into a literal");
}
