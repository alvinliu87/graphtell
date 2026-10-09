//! .NET parser — extracts Razor (`.cshtml` / `.razor`) view templates and ASP.NET Core (`*.cs`) route
//! declarations so backend views *and* backend routes join the graph, mirroring the other backend stacks.
//!
//! Both are regex-extracted from raw text (no tree-sitter grammar needed):
//! * Razor templates yield URL-form route refs that reuse the synthetic `template-route` callee
//!   (`fkb/universal/common.yaml::template-route-http-contract` → `HttpContract` + `CallsHttp`), so a
//!   Razor view reaches the same endpoint node the backend route declares.
//! * `Program.cs` (minimal APIs) and controller attributes yield `AspNetCore::<verb>` call sites that
//!   `fkb/csharp/aspnetcore.yaml::route-http-contract` turns into an `HttpContract` + `HandledBy` edge —
//!   the backend half of the same bridge.

use regex::Regex;

use gt_domain::error::Result;
use gt_domain::model::{
    CallSiteFact, FactValue, ImportFact, Language, Span, SyntaxFacts, TemplateBindingFact,
};
use gt_domain::port::LanguageParser;

pub struct DotnetParser;

impl DotnetParser {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl LanguageParser for DotnetParser {
    fn language(&self) -> Language {
        Language::new(Language::CSHARP)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["cshtml", "razor", "cs"]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        if is_razor_template(path) {
            return Ok(extract_razor_template(source, path));
        }
        if is_cs_file(path) {
            return Ok(extract_aspnet_routes(source, path));
        }
        Ok(SyntaxFacts::default())
    }

    fn namespace_separator(&self) -> &'static [char] {
        &[]
    }

    fn member_separator(&self) -> &'static str {
        "."
    }
}

fn is_razor_template(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".cshtml") || p.ends_with(".razor")
}

/// An ASP.NET Core source file: anything under the project with a `.cs` extension.
fn is_cs_file(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".cs")
}

fn regex_razor_component() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"Component\.InvokeAsync\(\s*["']([^"']+)["']"#).unwrap())
}
fn regex_razor_partial() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<partial\b[^>]*\bname\s*=\s*["']([^"']+)["']"#).unwrap())
}
fn regex_razor_href_action() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?:href|action)\s*=\s*["'](/[^"']*)["']"#).unwrap())
}

/// Extract ASP.NET Core route declarations from C# source (minimal APIs + controller attributes).
///
/// Supported forms:
/// * `app.MapPost("/api/orders", OrdersController.Create)`   (minimal API; handler is the target)
/// * `endpoints.MapGet("/api/orders", Handlers.List)`        (endpoint routing)
/// * `[HttpPost("/api/orders")]` / `[HttpGet]`               (controller action attribute; the enclosing
///   `class XController` becomes the handler target)
///
/// Each match emits an `AspNetCore::<verb>` call site whose `method` carries the HTTP verb (lower-cased)
/// and whose `args[0]` is the path and `args[1]` the `controller#action` / handler target;
/// `fkb/csharp/aspnetcore.yaml` turns that into an `HttpContract` + `HandledBy` edge.
fn extract_aspnet_routes(source: &str, path: &str) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    let re_map = regex_aspnet_map();
    let re_attr = regex_aspnet_attr();
    let re_class = regex_aspnet_class();
    let mut current_class: Option<String> = None;
    for line in source.lines() {
        if let Some(c) = re_class.captures(line) {
            current_class = Some(c[1].to_string());
        }
        let verb_path_handler: Option<(String, String, Option<String>)> =
            if let Some(m) = re_map.captures(line) {
                let verb = m[1].to_ascii_lowercase();
                let route_path = m[2].to_string();
                let handler = m.get(3).map(|h| h.as_str().trim().to_string());
                Some((verb, route_path, handler))
            } else if let Some(a) = re_attr.captures(line) {
                let verb = a[1].to_ascii_lowercase();
                let route_path = a
                    .get(2)
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                // Attribute form: the enclosing controller class is the handler target.
                Some((verb, route_path, current_class.clone()))
            } else {
                None
            };
        if let Some((verb, route_path, handler)) = verb_path_handler {
            if route_path.is_empty() {
                continue; // attribute with no template path carries no URL to bridge to
            }
            let entity = handler.filter(|h| !h.is_empty());
            facts.call_sites.push(CallSiteFact {
                owner_fqn: path.to_string(),
                owner_class: None,
                callee_text: format!("AspNetCore::{}", verb),
                snippet: None,
                // Mirrors Laravel's `Route::{get,post}` callsite shape: the `receiver`/`method` split is
                // what the `AspNetCore::{get,post,...}` selector matches on, and `method` (lower-cased
                // verb) feeds `method_name: true` (upper-cased) in `fkb/csharp/aspnetcore.yaml`.
                receiver: Some("AspNetCore".to_string()),
                method: Some(verb.clone()),
                args: vec![
                    FactValue::String(route_path),
                    FactValue::String(entity.clone().unwrap_or_default()),
                ],
                span: Span::default(),
                db_table: None,
                in_loop: false,
                entity,
            });
        }
    }
    facts
}

fn regex_aspnet_map() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `app.MapPost("/api/orders", OrdersController.Create)` / `endpoints.MapGet(...)`.
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(?:app|endpoints|builder|group|mapgroup)\.Map(Get|Post|Put|Patch|Delete|Head)\s*\(\s*["']([^"']+)["']\s*,\s*([^)]*?)\)"#,
        )
        .unwrap()
    })
}
fn regex_aspnet_attr() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `[HttpPost("/api/orders")]` / `[HttpGet]` (C# attributes, no leading `#` unlike Rust).
    RE.get_or_init(|| {
        Regex::new(r#"(?i)\[\s*(?:Http)?(Get|Post|Put|Patch|Delete|Head)\b(?:\s*\(\s*["']([^"']+)["']\s*\))?\s*\]"#)
            .unwrap()
    })
}
fn regex_aspnet_class() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)\bclass\s+([A-Za-z_]\w*)"#).unwrap())
}

/// Extract Razor template bindings from raw text: view components / partial views / URL-form route refs.
fn extract_razor_template(source: &str, path: &str) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    for m in regex_razor_component().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "component_ref".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_razor_partial().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "include".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_razor_href_action().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "route".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    mirror_template_bindings(&mut facts);
    facts
}

/// Mirror template bindings into classical facts (identical to the PHP / Java / Python / Ruby parsers).
fn mirror_template_bindings(facts: &mut SyntaxFacts) {
    for tb in &facts.template_bindings {
        match tb.kind.as_str() {
            "component_ref" | "include" => {
                facts.imports.push(ImportFact {
                    name: tb.name.clone(),
                    alias: None,
                    span: tb.span.clone(),
                });
            }
            "route" => {
                if tb.name.starts_with('/') {
                    facts.call_sites.push(CallSiteFact {
                        owner_fqn: tb.owner_fqn.clone(),
                        owner_class: None,
                        callee_text: "template-route".to_string(),
                        snippet: None,
                        receiver: None,
                        method: None,
                        args: vec![FactValue::Array(vec![
                            ("url".to_string(), FactValue::String(tb.name.clone())),
                            ("method".to_string(), FactValue::String("ANY".to_string())),
                        ])],
                        span: tb.span.clone(),
                        db_table: None,
                        in_loop: false,
                        entity: None,
                    });
                } else {
                    facts.call_sites.push(CallSiteFact {
                        owner_fqn: tb.owner_fqn.clone(),
                        owner_class: None,
                        callee_text: tb.name.clone(),
                        snippet: None,
                        receiver: None,
                        method: None,
                        args: Vec::new(),
                        span: tb.span.clone(),
                        db_table: None,
                        in_loop: false,
                        entity: None,
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn razor_template_extracts_components_partials_and_url_routes() {
        let src = r#"
            @await Component.InvokeAsync("Cart")
            <partial name="Shared.Foo" />
            <a href="/api/orders">list</a>
        "#;
        let facts = extract_razor_template(src, "Views/Orders/Index.cshtml");
        assert!(
            facts.imports.iter().any(|i| i.name == "Cart"),
            "view component -> component_ref import"
        );
        assert!(
            facts.imports.iter().any(|i| i.name == "Shared.Foo"),
            "partial -> import"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "template-route"
                && matches!(
                    c.args.first(),
                    Some(FactValue::Array(items))
                        if items.iter().any(|(k, v)| k == "url" && v == &FactValue::String("/api/orders".to_string()))
                           && items.iter().any(|(k, v)| k == "method" && v == &FactValue::String("ANY".to_string()))
                )),
            "literal href -> CallsHttp contract (url + method:ANY packed into args[0])"
        );
        // A literal URL is captured; `asp-action` / `asp-controller` (named routes) are intentionally not
        // fabricated into a CallSite because they carry no URL to bridge to.
        assert!(
            !facts.call_sites.iter().any(|c| c.callee_text == "Orders.Create"),
            "asp-action/asp-controller named routes must not be fabricated"
        );
        assert_eq!(facts.template_bindings.len(), 3);
    }

    #[test]
    fn aspnet_routes_extract_http_contract_callsites() {
        let src = r#"
            var app = builder.Build();
            app.MapPost("/api/orders", OrdersController.Create);
            endpoints.MapGet("/api/items", Handlers.List);
            public class OrdersController {
                [HttpPost("/api/checkout")]
                public IActionResult Checkout() { return Ok(); }
            }
        "#;
        let facts = extract_aspnet_routes(src, "Program.cs");
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "AspNetCore::post"
                && c.receiver.as_deref() == Some("AspNetCore")
                && c.method.as_deref() == Some("post")
                && c.args.first() == Some(&FactValue::String("/api/orders".to_string()))
                && c.args.get(1) == Some(&FactValue::String("OrdersController.Create".to_string()))
                && c.entity.as_deref() == Some("OrdersController.Create")),
            "app.MapPost('/api/orders', OrdersController.Create) -> AspNetCore::post contract + HandledBy target"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "AspNetCore::get"
                && c.args.get(1) == Some(&FactValue::String("Handlers.List".to_string()))),
            "endpoints.MapGet -> AspNetCore::get contract"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "AspNetCore::post"
                && c.args.first() == Some(&FactValue::String("/api/checkout".to_string()))
                && c.entity.as_deref() == Some("OrdersController")),
            "controller [HttpPost(\"/api/checkout\")] -> AspNetCore::post with OrdersController handler"
        );
    }
}
