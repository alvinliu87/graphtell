//! Ruby parser — extracts ERB (`.erb`) view templates and Rails (`config/routes.rb`) route declarations
//! so backend views *and* backend routes join the graph, mirroring the other backend stacks.
//!
//! Both are regex-extracted from raw text (no tree-sitter grammar needed):
//! * `.erb` templates yield URL-form route refs that reuse the synthetic `template-route` callee
//!   (`fkb/universal/common.yaml::template-route-http-contract` → `HttpContract` + `CallsHttp`), so a
//!   Rails view reaches the same endpoint node the backend route declares.
//! * `config/routes.rb` yields `Rails::<verb>` call sites that `fkb/ruby/rails.yaml::route-http-contract`
//!   turns into an `HttpContract` + `HandledBy` edge — the backend half of the same bridge.

use regex::Regex;

use gt_domain::error::Result;
use gt_domain::model::{
    CallSiteFact, FactValue, ImportFact, Language, Span, SyntaxFacts, TemplateBindingFact,
};
use gt_domain::port::LanguageParser;

pub struct RubyParser;

impl RubyParser {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl LanguageParser for RubyParser {
    fn language(&self) -> Language {
        Language::new(Language::RUBY)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["erb", "rb"]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        if is_erb_template(path) {
            return Ok(extract_erb_template(source, path));
        }
        if is_routes_file(path) {
            return Ok(extract_rails_routes(source, path));
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

fn is_erb_template(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".erb")
}

/// A Rails route file: `config/routes.rb` (or any file under `config/routes/`).
fn is_routes_file(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with("config/routes.rb") || (p.contains("config/routes/") && p.ends_with(".rb"))
}

/// Extract Rails route declarations from `config/routes.rb` (DSL, not full Ruby parsing).
///
/// Supported forms (the common cases that carry a controller target):
/// * `post '/api/orders' => 'orders#create'`          (hash-rocket target)
/// * `post '/api/orders', to: 'orders#create'`         (keyword `to:` target)
/// * `post '/api/orders', controller: 'orders', action: 'create'`
///
/// Each match emits a `Rails::<verb>` call site whose `method` carries the HTTP verb (upper-cased) and
/// whose `args[0]` is the path and `args[1]` the `controller#action` target; `fkb/ruby/rails.yaml`
/// turns that into an `HttpContract` + `HandledBy` edge.
fn extract_rails_routes(source: &str, path: &str) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    let re_route = regex_rails_route();
    let re_target = regex_rails_route_target();
    let re_ca = regex_rails_controller_action();
    for line in source.lines() {
        let Some(m) = re_route.captures(line) else { continue };
        let verb = m[1].to_ascii_lowercase();
        let route_path = m[2].to_string();
        let target: Option<String> = if let Some(c) = re_ca.captures(line) {
            Some(format!("{}#{}", &c[1], &c[2]))
        } else if let Some(t) = re_target.captures(line) {
            Some(t[1].to_string())
        } else {
            None
        };
        facts.call_sites.push(CallSiteFact {
            owner_fqn: path.to_string(),
            owner_class: None,
            callee_text: format!("Rails::{}", verb),
            snippet: None,
            // Mirrors Laravel's `Route::{get,post}` callsite shape: the `receiver`/`method` split is what
            // the `Rails::{get,post,...}` selector matches on, and `method` (lower-cased verb) feeds
            // `method_name: true` (upper-cased) in `fkb/ruby/rails.yaml`.
            receiver: Some("Rails".to_string()),
            method: Some(verb.clone()),
            args: vec![
                FactValue::String(route_path),
                FactValue::String(target.clone().unwrap_or_default()),
            ],
            span: Span::default(),
            db_table: None,
            in_loop: false,
            entity: target,
        });
    }
    facts
}

fn regex_erb_partial() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"render\s+(?:partial:\s*)?["']([^"']+)["']"#).unwrap())
}
fn regex_erb_template_render() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"render\s+template:\s*["']([^"']+)["']"#).unwrap())
}
fn regex_erb_link() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // The 2nd argument is either a quoted URL (`link_to "Edit", "/api/orders/1"`) or an unquoted path
    // helper (`link_to "User", user_path`) — capture it either way.
    RE.get_or_init(|| Regex::new(r#"link_to\s+["'][^"']*["']\s*,\s*["']?([^"'\s,]+)["']?"#).unwrap())
}
fn regex_erb_form() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"form_(?:with|tag)\b[^>]*?\burl:\s*["']([^"']+)["']"#).unwrap())
}
fn regex_erb_href_action() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?:href|action)\s*=\s*["'](/[^"']*)["']"#).unwrap())
}
fn regex_rails_route() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `post '/api/orders'` — verb + path literal (the rest of the line is parsed separately).
    RE.get_or_init(|| {
        Regex::new(r#"(?i)^\s*(get|post|put|patch|delete|head)\b\s*['"]([^'"]+)['"]"#).unwrap()
    })
}
fn regex_rails_route_target() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `to: 'orders#create'` or `=> 'orders#create'`.
    RE.get_or_init(|| Regex::new(r#"(?i)(?:to:|=>)\s*['"]([^'"]+)['"]"#).unwrap())
}
fn regex_rails_controller_action() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `controller: 'orders', action: 'create'` -> `orders#create`.
    RE.get_or_init(|| {
        Regex::new(r#"(?i)controller:\s*['"]([^'"]+)['"]\s*,\s*action:\s*['"]([^'"]+)['"]"#).unwrap()
    })
}

/// Extract ERB template bindings from raw text: partial renders / component tags / route references.
fn extract_erb_template(source: &str, path: &str) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    for m in regex_erb_partial().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "include".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_erb_template_render().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "component_ref".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_erb_link().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "route".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_erb_form().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "route".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_erb_href_action().captures_iter(source) {
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

/// Mirror template bindings into classical facts (identical to the PHP / Java / Python parsers):
/// `component_ref` / `include` -> `Import` (a view depends on a component / partial); `route` -> a
/// `CallSite` (the rendered view reaches a backend endpoint). URL-form route refs (`/...`) become the
/// synthetic `template-route` callee so the universal FKB rule synthesises an `HttpContract` +
/// `CallsHttp` edge; named routes stay a queryable `CallSite` without fabricating an orphan contract.
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
    fn erb_template_extracts_components_partials_and_url_routes() {
        let src = r#"
            <%= render partial: "shared/header" %>
            <%= render template: "shared/footer" %>
            <%= link_to "Edit", "/api/orders/1" %>
            <a href="/api/items">list</a>
            <%= form_with url: "/api/checkout" do |f| %><% end %>
            <%= link_to "User", user_path %>
        "#;
        let facts = extract_erb_template(src, "app/views/orders/show.erb");
        assert!(
            facts.imports.iter().any(|i| i.name == "shared/header"),
            "partial render -> import"
        );
        assert!(
            facts.imports.iter().any(|i| i.name == "shared/footer"),
            "template render -> component_ref import"
        );
        // URL-form route refs become the synthetic `template-route` callee.
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "template-route"
                && matches!(
                    c.args.first(),
                    Some(FactValue::Array(items))
                        if items.iter().any(|(k, v)| k == "url" && v == &FactValue::String("/api/orders/1".to_string()))
                           && items.iter().any(|(k, v)| k == "method" && v == &FactValue::String("ANY".to_string()))
                )),
            "link_to with literal URL -> CallsHttp contract"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "template-route"
                && matches!(
                    c.args.first(),
                    Some(FactValue::Array(items))
                        if items.iter().any(|(k, v)| k == "url" && v == &FactValue::String("/api/items".to_string()))
                           && items.iter().any(|(k, v)| k == "method" && v == &FactValue::String("ANY".to_string()))
                )),
            "href with literal URL -> CallsHttp contract"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "template-route"
                && matches!(
                    c.args.first(),
                    Some(FactValue::Array(items))
                        if items.iter().any(|(k, v)| k == "url" && v == &FactValue::String("/api/checkout".to_string()))
                           && items.iter().any(|(k, v)| k == "method" && v == &FactValue::String("ANY".to_string()))
                )),
            "form_with url -> CallsHttp contract"
        );
        // A named route (`user_path`) stays a queryable CallSite, no orphan contract.
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "user_path" && c.args.is_empty()),
            "named route -> plain CallSite"
        );
        assert_eq!(facts.template_bindings.len(), 6);
    }

    #[test]
    fn rails_routes_extract_http_contract_callsites() {
        let src = "Rails.application.routes.draw do\n\
            post '/api/orders' => 'orders#create'\n\
            get '/api/items', to: 'items#index'\n\
            put '/api/items/:id', controller: 'items', action: 'update'\n\
        end\n";
        let facts = extract_rails_routes(src, "config/routes.rb");
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "Rails::post"
                && c.receiver.as_deref() == Some("Rails")
                && c.method.as_deref() == Some("post")
                && c.args.first() == Some(&FactValue::String("/api/orders".to_string()))
                && c.args.get(1) == Some(&FactValue::String("orders#create".to_string()))
                && c.entity.as_deref() == Some("orders#create")),
            "post '/api/orders' => 'orders#create' -> Rails::post contract + HandledBy target"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "Rails::get"
                && c.method.as_deref() == Some("get")
                && c.args.get(1) == Some(&FactValue::String("items#index".to_string()))),
            "get ..., to: -> Rails::get contract"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "Rails::put"
                && c.entity.as_deref() == Some("items#update")),
            "controller:/action: -> Rails::put with items#update"
        );
    }
}
