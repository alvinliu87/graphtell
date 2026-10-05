//! P2 CfAst: create syntax-level nodes and edges from the language grammar.
//!
//! Products: `Class` / `Interface` / `Trait` / `Enum` / `Method` / `Function` /
//! `Property` / `Const` / `Namespace` / `CallSite` nodes,
//! the `imports` table (short name -> FQN), the `by_name` index (FQN -> NodeId),
//! and inheritance / implementation / trait edges.
//!
//! This phase is **completely language-agnostic**: it only consumes `SyntaxFacts`.

use std::collections::HashMap;

use gt_domain::model::{
    Declaration, EdgeKind, FactValue, NamespacePolicy, NewEdge, NewNode, NodeId, NodeKind, Phase,
    ProjectId, ResolveAs, Severity, SourceFile, Span, SyntaxFacts,
};
use gt_domain::port::{FileSystem, ParserRegistry, TechStackRegistry};
use tracing::{debug, warn};

use crate::context::PipelineContext;

/// Run CfAst.
pub fn run(
    ctx: &mut PipelineContext,
    parsers: &dyn ParserRegistry,
    fs: &dyn FileSystem,
    techstack: &dyn TechStackRegistry,
) {
    let phase = Phase(Phase::CF_AST.to_string());
    let project_id = ctx.project.id;
    let root = ctx.project.root_path.clone();
    let files: Vec<SourceFile> = ctx.files.clone();

    let mut unsupported: HashMap<String, usize> = HashMap::new();

    for file in files {
        let abs = root.join(&file.path);
        let source = match fs.read_to_string(&abs) {
            Ok(s) => s,
            Err(e) => {
                debug!("skipping an unreadable file {}: {e}", file.path);
                continue;
            }
        };
        let Some(parser) = parsers.parser_for(&file.language) else {
            *unsupported.entry(file.language.as_str().to_string()).or_insert(0) += 1;
            // Degrade instead of dropping: still build the **File node**.
            build_file(ctx, project_id, &file, &SyntaxFacts::default(), &phase, techstack);
            continue;
        };
        let facts: SyntaxFacts = match parser.parse(&file.path, &source) {
            Ok(f) => f,
            Err(e) => {
                warn!("parse failed {}: {e}", file.path);
                continue;
            }
        };
        build_file(ctx, project_id, &file, &facts, &phase, techstack);
    }

    // Report **per language** aggregated (not per file, to avoid flooding).
    let mut langs: Vec<(String, usize)> = unsupported.into_iter().collect();
    langs.sort();
    for (lang, n) in langs {
        ctx.ws.diagnose(
            &phase,
            "NoParserForLanguage",
            Severity::Warning,
            format!(
                "language `{lang}` has no parser yet: {n} source files only built file nodes (the structural layer),\
                 no class / function / call semantic extraction — this sub-project's graph has only file structure"
            ),
            None,
        );
        // The same fact in machine-readable form, for the UI to render a banner directly (diagnostics are for humans, this is for the interface to look up).
        ctx.ws.put_symbol(
            ctx.project.id,
            "unsupported_languages",
            &lang,
            serde_json::json!({ "files": n }),
        );
        warn!("language {lang} has no parser yet: {n} files only built structural-layer nodes (no semantic extraction)");
    }
}

fn build_file(
    ctx: &mut PipelineContext,
    project_id: ProjectId,
    file: &SourceFile,
    facts: &SyntaxFacts,
    phase: &Phase,
    techstack: &dyn TechStackRegistry,
) {
    // This phase is language-agnostic: every notation rule (namespace separator, member separator,
    // builtin types) comes from the sub-project's `NamespacePolicy`, never hard-coded here.
    let policy = ctx.lang_policy_for_sub(file.sub_project_id).clone();
    let span = Span::default();
    let file_node = ctx.ws.add_node(NewNode {
        id: None,
        project_id,
        sub_project_id: file.sub_project_id,
        kind: NodeKind(NodeKind::FILE.to_string()),
        name: file.path.clone(),
        fqn: Some(file.path.clone()),
        identity: None,
        file_id: Some(file.id),
        span,
        language: file.language.clone(),
        phase: phase.clone(),
        confidence: 1.0,
        properties: serde_json::json!({ "size": file.size_bytes }),
    });
    ctx.ws.record_file_node(&file.path, file_node);

    // Imports table: short name (lowercased) -> FQN (class short name maps to fully qualified name)
    let mut imports: HashMap<String, String> = HashMap::new();
    for imp in &facts.imports {
        let fqn = imp
            .name
            .trim_start_matches(|c| policy.ns_separators.contains(&c))
            .to_string();
        let short = match &imp.alias {
            Some(a) => a.clone(),
            // Short name takes the last namespace segment, split on this language's separators.
            None => fqn
                .rsplit(|c: char| policy.ns_separators.contains(&c))
                .next()
                .unwrap_or(&fqn)
                .to_string(),
        };
        imports
            .entry(short.to_ascii_lowercase())
            .or_insert_with(|| fqn.clone());
        if ctx.ws.get_symbol("imports", &short.to_ascii_lowercase()).is_none() {
            ctx.ws.put_symbol(
                project_id,
                "imports",
                &short.to_ascii_lowercase(),
                serde_json::json!({ "fqn": fqn, "file": file.path }),
            );
        }
    }
    // Keep a **per-file** imports table for P7 to resolve call receivers: the global `imports` symbol table is first-come-first-served,
    // and would necessarily break when the same short name points to different classes in different files; only storing per file matches PHP resolution rules.
    ctx.ws
        .record_file_imports(file.id.get(), &file.path, imports.clone());

    // Namespace node (deduped by FQN)
    let mut ns_node: Option<NodeId> = None;
    for d in &facts.declarations {
        if d.kind.as_str() != NodeKind::NAMESPACE {
            continue;
        }
        let id = match ctx.ws.find_by_name(&d.fqn) {
            Some(id) => id,
            None => ctx.ws.add_node(NewNode {
                id: None,
                project_id,
                sub_project_id: file.sub_project_id,
                kind: NodeKind(NodeKind::NAMESPACE.to_string()),
                name: d.name.clone(),
                fqn: Some(d.fqn.clone()),
                identity: None,
                file_id: Some(file.id),
                span: d.span,
                language: file.language.clone(),
                phase: phase.clone(),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            }),
        };
        ns_node = Some(id);
    }

    // Type nodes (build classes first, then members, so by_name is usable)
    let mut local: HashMap<String, NodeId> = HashMap::new();
    for d in &facts.declarations {
        if !matches!(
            d.kind.as_str(),
            NodeKind::CLASS | NodeKind::INTERFACE | NodeKind::TRAIT | NodeKind::ENUM
        ) {
            continue;
        }
        let id = ctx.ws.add_node(NewNode {
            id: None,
            project_id,
            sub_project_id: file.sub_project_id,
            kind: d.kind.clone(),
            name: d.name.clone(),
            fqn: Some(d.fqn.clone()),
            identity: None,
            file_id: Some(file.id),
            span: d.span,
            language: file.language.clone(),
            phase: phase.clone(),
            confidence: 1.0,
            properties: d.extra.clone(),
        });
        local.insert(d.fqn.clone(), id);
        ctx.ws.add_edge(NewEdge {
            project_id,
            kind: EdgeKind(EdgeKind::DECLARES.to_string()),
            from_id: file_node,
            to_id: id,
            phase: phase.clone(),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
        if let Some(ns) = ns_node {
            ctx.ws.add_edge(NewEdge {
                project_id,
                kind: EdgeKind(EdgeKind::CONTAINS.to_string()),
                from_id: ns,
                to_id: id,
                phase: phase.clone(),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            });
        }
    }

    // Member nodes: Method / Function / Property / Const / EnumCase
    for d in &facts.declarations {
        if !matches!(
            d.kind.as_str(),
            NodeKind::METHOD
                | NodeKind::FUNCTION
                | NodeKind::PROPERTY
                | NodeKind::CONST
                | NodeKind::ENUM_CASE
        ) {
            continue;
        }
        let id = ctx.ws.add_node(NewNode {
            id: None,
            project_id,
            sub_project_id: file.sub_project_id,
            kind: d.kind.clone(),
            name: d.name.clone(),
            fqn: Some(d.fqn.clone()),
            identity: None,
            file_id: Some(file.id),
            span: d.span,
            language: file.language.clone(),
            phase: phase.clone(),
            confidence: 1.0,
            properties: d.extra.clone(),
        });
        local.insert(d.fqn.clone(), id);
        if d.kind.as_str() == NodeKind::PROPERTY {
            if let Some(owner) = d.parent_fqn.as_ref().and_then(|p| local.get(p).copied()) {
                ctx.ws.record_property(owner, &d.name, property_value(d));
            }
        }
        if d.kind.as_str() == NodeKind::METHOD {
            // Parameter type: `__construct(LoginServices $services)` -> variable services : LoginServices.
            // Used by P7 to resolve `$services->appAuth()` (by variable type).
            if let Some(params) = d.extra.get("parameters").and_then(|v| v.as_array()) {
                for p in params {
                    let (Some(var), Some(ty)) = (
                        p.get("name").and_then(|v| v.as_str()),
                        p.get("type").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    if is_builtin_type(ty, &policy) {
                        continue;
                    }
                    let fqn = resolve_type(
                        facts.namespace.as_deref(),
                        &imports,
                        ty.trim_start_matches('?'),
                        &policy,
                    );
                    // Strip the `$` prefix from variable names uniformly, so they can be looked up by `$var`.
                    ctx.ws
                        .add_param_type(&d.fqn, var.trim_start_matches('$'), &fqn);
                }
            }
            // Constructor injection: `$this->services = $services` -> property services type = parameter services type.
            if let Some(assigns) = d.extra.get("this_assigns").and_then(|v| v.as_array()) {
                let Some(class_fqn) = d.parent_fqn.as_deref() else {
                    continue;
                };
                for a in assigns {
                    let (Some(prop), Some(var)) = (
                        a.get("prop").and_then(|v| v.as_str()),
                        a.get("var").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    if let Some(ty) = ctx.ws.param_type(&d.fqn, var) {
                        let ty = ty.to_string();
                        ctx.ws.set_prop_type(class_fqn, prop, &ty);
                    }
                }
            }
            if let Some(assigns) = d.extra.get("this_assign_types").and_then(|v| v.as_array()) {
                let Some(class_fqn) = d.parent_fqn.as_deref() else {
                    continue;
                };
                for a in assigns {
                    let (Some(prop), Some(cls)) = (
                        a.get("prop").and_then(|v| v.as_str()),
                        a.get("class").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    if ctx.ws.prop_type(class_fqn, prop).is_some() {
                        continue;
                    }
                    let fqn = resolve_type(facts.namespace.as_deref(), &imports, cls, &policy);
                    ctx.ws.set_prop_type(class_fqn, prop, &fqn);
                }
            }
            // Local variable inside a method: `$model = new OrderModel();` -> `$model->where(...)` is resolvable.
            if let Some(list) = d.extra.get("local_assign_types").and_then(|v| v.as_array()) {
                for a in list {
                    let (Some(var), Some(cls)) = (
                        a.get("var").and_then(|v| v.as_str()),
                        a.get("class").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    let fqn = resolve_type(facts.namespace.as_deref(), &imports, cls, &policy);
                    ctx.ws.set_local_type(&d.fqn, var, &fqn);
                }
            }
            // `return X::class;` -> declarative link: owning class -> X (e.g. `Dao::setModel()` -> Model).
            // Cross-file references are resolved uniformly by P7 (the target class may not have a node yet).
            if let Some(list) = d.extra.get("returns_class").and_then(|v| v.as_array()) {
                if let Some(owner_id) = d.parent_fqn.as_ref().and_then(|p| local.get(p).copied()) {
                    for c in list {
                        let Some(name) = c.as_str() else { continue };
                        let fqn = resolve_type(facts.namespace.as_deref(), &imports, name, &policy);
                        ctx.ws.pending_links.push(crate::workspace::PendingLink {
                            from: owner_id,
                            kind: EdgeKind(EdgeKind::RESOLVES_TO.to_string()),
                            raw: fqn,
                            method: None,
                            resolve: ResolveAs::ClassConst,
                            confidence: 0.85,
                            sub: file.sub_project_id,
                            file: file.path.clone(),
                            line: d.span.start_line,
                        });
                    }
                }
            }
        }
        let owner = d
            .parent_fqn
            .as_ref()
            .and_then(|p| local.get(p).copied())
            .or(ns_node);
        if let Some(owner) = owner {
            ctx.ws.add_edge(NewEdge {
                project_id,
                kind: EdgeKind(EdgeKind::DECLARES.to_string()),
                from_id: owner,
                to_id: id,
                phase: phase.clone(),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            });
        }
    }

    // Inheritance / implementation / trait
    for inh in &facts.inheritances {
        let base_fqn = resolve_type(facts.namespace.as_deref(), &imports, &inh.base_name, &policy);
        let target = ctx
            .ws
            .find_by_name(&base_fqn)
            .or_else(|| ctx.ws.resolve_short_name(&base_fqn).and_then(|f| ctx.ws.find_by_name(&f)));
        let Some(child) = local.get(&inh.child_fqn).copied() else {
            continue;
        };
        ctx.ws.record_supertype(&inh.child_fqn, &base_fqn);
        ctx.ws.inherits.push(crate::workspace::InheritRecord {
            child,
            child_fqn: inh.child_fqn.clone(),
            base: base_fqn.clone(),
            kind: inh.kind.clone(),
            sub: file.sub_project_id,
            file: file.path.clone(),
            span: inh.span,
        });
        let target = match target {
            Some(t) => t,
            None => {
                ctx.ws.diagnose(
                    phase,
                    "AliasTargetMissing",
                    Severity::Info,
                    format!(
                        "base class {} is not in the graph (possibly excluded by Ingest), a placeholder node was recorded",
                        base_fqn
                    ),
                    Some(format!("{}:{}", file.path, inh.span.start_line)),
                );
                ctx.ws.add_node(NewNode {
                    id: None,
                    project_id,
                    sub_project_id: file.sub_project_id,
                    kind: NodeKind(NodeKind::CLASS.to_string()),
                    name: base_fqn
                        .rsplit(|c: char| policy.ns_separators.contains(&c))
                        .next()
                        .unwrap_or(&base_fqn)
                        .to_string(),
                    fqn: Some(base_fqn.clone()),
                    identity: None,
                    file_id: None,
                    span: inh.span,
                    language: file.language.clone(),
                    phase: phase.clone(),
                    confidence: 0.0,
                    properties: serde_json::Value::Null,
                })
            }
        };
        ctx.ws.add_edge(NewEdge {
            project_id,
            kind: inh.kind.clone(),
            from_id: child,
            to_id: target,
            phase: phase.clone(),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
    }

    // `@method` magic methods: methods a class declares via phpdoc and forwards through `__call`.
    // They have no method node, but the call site writes the real method name — P7 uses this to hand it to the FKB-declared delegation attribute.
    for d in &facts.declarations {
        if d.kind.as_str() != NodeKind::CLASS {
            continue;
        }
        let names: Vec<String> = d
            .extra
            .get("magic_methods")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        ctx.ws.set_magic_methods(&d.fqn, &names);
    }

    // Field-declaration type: restored to FQN via import, written into `prop_types` (so P7 can resolve `field.method()`
    // instance calls by field type, connecting the `service -> mapper -> table` call chain).
    for ft in &facts.field_types {
        let type_fqn = resolve_type(facts.namespace.as_deref(), &imports, &ft.type_name, &policy);
        ctx.ws.set_prop_type(&ft.class_fqn, &ft.field, &type_fqn);
    }

    // Call sites: refine down to CallSite nodes
    for call in &facts.call_sites {
        let owner = local
            .get(&call.owner_fqn)
            .copied()
            .or_else(|| ctx.ws.find_by_name(&call.owner_fqn))
            .or_else(|| {
                owner_parent(&call.owner_fqn, &policy.member_separator).and_then(|p| {
                    local.get(&p).copied().or_else(|| ctx.ws.find_by_name(&p))
                })
            })
            .unwrap_or(file_node);
        let call_node = ctx.ws.add_node(NewNode {
            id: None,
            project_id,
            sub_project_id: file.sub_project_id,
            kind: NodeKind(NodeKind::CALL_SITE.to_string()),
            name: call.callee_text.clone(),
            fqn: Some(format!(
                "{}#{}:{}",
                call.owner_fqn, call.callee_text, call.span.start_line
            )),
            identity: None,
            file_id: Some(file.id),
            span: call.span,
            language: file.language.clone(),
            phase: phase.clone(),
            confidence: 1.0,
            properties: {
                let mut props = match &call.snippet {
                    Some(s) => serde_json::json!({ "snippet": s }),
                    None => serde_json::json!({}),
                };
                if call.in_loop {
                    props["in_loop"] = serde_json::json!(true);
                }
                if props.as_object().is_some_and(|o| o.is_empty()) {
                    serde_json::Value::Null
                } else {
                    props
                }
            },
        });
        ctx.ws.add_edge(NewEdge {
            project_id,
            kind: EdgeKind(EdgeKind::HAS_CALL_SITE.to_string()),
            from_id: owner,
            to_id: call_node,
            phase: phase.clone(),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
        ctx.ws.calls.push(crate::workspace::CallRecord {
            node: call_node,
            owner,
            owner_fqn: call.owner_fqn.clone(),
            owner_class: call.owner_class.clone(),
            callee: call.callee_text.clone(),
            receiver: call.receiver.clone(),
            method: call.method.clone(),
            args: call.args.clone(),
            db_table: call.db_table.clone(),
            in_loop: call.in_loop,
            entity: call.entity.clone(),
            span: call.span,
            file: file.path.clone(),
            sub: file.sub_project_id,
            language: file.language.clone(),
        });
        // Same-line chained-call arguments (`->except(['read'])`): `calls` is temporarily moved out of the workspace in P4/P5,
        // so register it in the index here on the side, for the Synthesize expansion table to read.
        ctx.ws.index_chained(
            &file.path,
            call.span.start_line,
            call.method.as_deref(),
            &call.args,
        );
    }

    // Config entries (`return [...]`-style files)
    let locale = crate::phase::facts::locale_of_path(&file.path, &file.language, techstack);
    let file_stem = std::path::Path::new(&file.path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string());
    for entry in &facts.config_entries {
        ctx.ws.configs.push(crate::workspace::ConfigRecord {
            file: file.path.clone(),
            key_path: entry.key_path.clone(),
            value: entry.value.clone(),
            span: entry.span,
            sub: file.sub_project_id,
            locale: locale.clone(),
            file_stem: file_stem.clone(),
        });
    }

    // Response-header assignments (e.g. `$header['Access-Control-Allow-Origin'] = ...`): promote the parse
    // fact into a graph node so an FKB rule can judge it (reflected CORS) without the kernel hard-coding any
    // language. Which assignments count is decided by the parser (per-language); the kernel only materialises
    // the fact. The rule matches on `key` (the header name) and `rhs` (the assigned expression).
    for h in &facts.header_assignments {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id,
            sub_project_id: file.sub_project_id,
            kind: NodeKind::from(NodeKind::HEADER_ASSIGNMENT),
            name: h.rhs_snippet.clone(),
            fqn: None,
            identity: None,
            file_id: Some(file.id),
            span: h.span,
            language: file.language.clone(),
            phase: phase.clone(),
            confidence: 1.0,
            properties: serde_json::json!({ "key": h.key, "rhs": h.rhs_snippet }),
        });
    }

    // Signature-comparison fact: add the file path, so `phase::sign` aligns with the in-function signature computation.
    for c in &facts.sign_compares {
        ctx.ws.sign_compares.push(gt_domain::model::syntax::SignCompareFact {
            left: c.left.clone(),
            right: c.right.clone(),
            operator: c.operator.clone(),
            owner_fqn: c.owner_fqn.clone(),
            file: file.path.clone(),
            span: c.span,
        });
    }

    // Local-variable-assignment fact: add the file path, so `phase::taint` traces variable origins backward within the same function.
    for a in &facts.variable_assignments {
        ctx.ws
            .variable_assignments
            .push(gt_domain::model::syntax::VariableAssignFact {
                var: a.var.clone(),
                rhs: a.rhs.clone(),
                owner_fqn: a.owner_fqn.clone(),
                file: file.path.clone(),
                span: a.span,
            });
    }
}

/// `Class.member` / `Ns\Class::member` -> `Class` (returns `None` when there is no member separator).
///
/// The member separator varies by language: PHP `::`, Java / JS `.`. Only used when **the exact FQN lookup finds no node**,
/// so it will not wrongly slice a class FQN (`com.example.MyClass`) into a package name — that case would never reach here.
fn owner_parent(fqn: &str, member_separator: &str) -> Option<String> {
    let idx = fqn.rfind(member_separator)?;
    let parent = &fqn[..idx];
    if parent.is_empty() {
        None
    } else {
        Some(parent.to_string())
    }
}

fn property_value(d: &Declaration) -> FactValue {
    d.extra
        .get("default")
        .and_then(|v| serde_json::from_value::<FactValue>(v.clone()).ok())
        .unwrap_or(FactValue::Null)
}

/// Resolve a type name written in source into a fully qualified name.
///
/// The namespace notation (leading root separator, separator used inside an already-qualified name,
/// and the joiner used to qualify a bare name against the current namespace) is **not** hard-coded:
/// it comes from the language's `NamespacePolicy`.
pub fn resolve_type(
    ns: Option<&str>,
    imports: &HashMap<String, String>,
    name: &str,
    policy: &NamespacePolicy,
) -> String {
    let raw = name.trim();
    if raw.starts_with(|c| policy.ns_separators.contains(&c)) {
        return raw
            .trim_start_matches(|c| policy.ns_separators.contains(&c))
            .to_string();
    }
    if raw.contains(|c| policy.ns_separators.contains(&c)) {
        return raw.to_string();
    }
    if let Some(fqn) = imports.get(&raw.to_ascii_lowercase()) {
        return fqn.clone();
    }
    match (ns, policy.ns_separator) {
        // Without this language's namespace separator the name cannot be qualified — leave it as written
        // rather than joining with another stack's separator.
        (Some(ns), Some(sep)) if !ns.is_empty() => format!("{}{}{}", ns, sep, raw),
        _ => raw.to_string(),
    }
}

/// Whether it is a language-builtin type (`int` / `string` / `array` …), not a class name; skipped, not
/// participating in type inference. The primitive set is language-specific and comes from `policy`.
fn is_builtin_type(t: &str, policy: &NamespacePolicy) -> bool {
    let t = t.trim_start_matches('?').trim().to_ascii_lowercase();
    policy
        .builtin_types
        .iter()
        .any(|b| b.eq_ignore_ascii_case(&t))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gt_domain::error::Result as DomainResult;
    use gt_domain::model::{
        CallSiteFact, ConfigEntryFact, Declaration, EdgeKind, FactValue, ImportFact, InheritanceFact,
        Language, NamespacePolicy, NodeKind, Project, ProjectId, ProjectStatus, SourceFile, FileId,
        Span, SyntaxFacts,
    };
    use gt_domain::port::{FileSystem, LanguageParser, ParserRegistry, TechStackRegistry};

    use crate::context::PipelineContext;

    // ---- pure helpers ------------------------------------------------------

    /// A PHP-style notation policy; also the default used by the `run`-level harness below.
    fn php_policy() -> NamespacePolicy {
        NamespacePolicy {
            ns_separator: Some('\\'),
            ns_separators: vec!['\\'],
            member_separator: "::".to_string(),
            variable_prefixes: vec!["$".to_string()],
            builtin_types: vec![
                "int".into(),
                "string".into(),
                "array".into(),
                "bool".into(),
                "float".into(),
                "void".into(),
            ],
            bare_field_receivers: false,
        }
    }

    #[test]
    fn resolve_type_leading_separator_is_stripped() {
        let policy = php_policy();
        let empty: HashMap<String, String> = HashMap::new();
        // `\Foo\Bar` -> `Foo\Bar` (leading root separator must be normalised away)
        let r = super::resolve_type(None, &empty, "\\Foo\\Bar", &policy);
        assert_eq!(r, "Foo\\Bar");
    }

    #[test]
    fn resolve_type_already_qualified_returns_as_is() {
        let policy = php_policy();
        let empty: HashMap<String, String> = HashMap::new();
        // an already-qualified name is returned verbatim — and this check happens *before* the
        // import lookup, so it must not be swallowed by an import miss.
        let r = super::resolve_type(None, &empty, "App\\Foo", &policy);
        assert_eq!(r, "App\\Foo");
    }

    #[test]
    fn resolve_type_import_alias_is_case_insensitive() {
        let policy = php_policy();
        let mut imports: HashMap<String, String> = HashMap::new();
        imports.insert("user".to_string(), "App\\User".to_string());
        // short name lookup is keyed lower-cased; the source may write any casing.
        assert_eq!(super::resolve_type(None, &imports, "User", &policy), "App\\User");
        assert_eq!(super::resolve_type(None, &imports, "uSeR", &policy), "App\\User");
    }

    #[test]
    fn resolve_type_unqualified_is_namespace_joined() {
        let policy = php_policy();
        let empty: HashMap<String, String> = HashMap::new();
        // bare name in a namespace is qualified with the language's separator.
        let r = super::resolve_type(Some("App"), &empty, "Foo", &policy);
        assert_eq!(r, "App\\Foo");
    }

    #[test]
    fn resolve_type_no_namespace_and_no_import_returns_raw() {
        let policy = php_policy();
        let empty: HashMap<String, String> = HashMap::new();
        // no import, no namespace -> leaf as written (must not invent a separator).
        assert_eq!(super::resolve_type(None, &empty, "Foo", &policy), "Foo");
        assert_eq!(super::resolve_type(Some(""), &empty, "Foo", &policy), "Foo");
    }

    #[test]
    fn owner_parent_returns_class_segment() {
        assert_eq!(
            super::owner_parent("App\\Order::pay", "::"),
            Some("App\\Order".to_string())
        );
    }

    #[test]
    fn owner_parent_none_when_no_member_separator() {
        // a class FQN without the member separator yields no parent.
        assert_eq!(super::owner_parent("App\\Order", "::"), None);
    }

    /// `is_builtin_type` must strip the nullable `?` prefix and match case-insensitively against the
    /// policy's primitive set — otherwise `?string` / `INT` would be wrongly treated as a class name and
    /// pulled into type inference.
    #[test]
    fn is_builtin_type_strips_nullable_and_is_case_insensitive() {
        let policy = php_policy();
        assert!(super::is_builtin_type("string", &policy));
        assert!(super::is_builtin_type("?string", &policy), "可空前缀 ? 应被剥离");
        assert!(super::is_builtin_type("INT", &policy), "大小写不敏感");
        assert!(super::is_builtin_type(" Array ", &policy), "前后空白应被 trim");
        assert!(!super::is_builtin_type("DateTime", &policy), "非内建类型不应命中");
        assert!(!super::is_builtin_type("?CustomType", &policy));
    }

    /// `property_value` pulls the `default` field out of a declaration's `extra` into a `FactValue`, and
    /// falls back to `Null` when it is absent or not a valid `FactValue`.
    #[test]
    fn property_value_reads_default_or_null() {
        let with_default = Declaration {
            kind: NodeKind(NodeKind::PROPERTY.to_string()),
            name: "x".into(),
            fqn: "C::x".into(),
            parent_fqn: None,
            span: Span::default(),
            // `default` is a `FactValue`, so it must be FactValue-shaped json (internally tagged).
            extra: serde_json::json!({ "default": { "t": "Int", "v": 42 } }),
        };
        let expected =
            serde_json::from_value::<FactValue>(serde_json::json!({ "t": "Int", "v": 42 })).unwrap();
        assert_eq!(super::property_value(&with_default), expected, "应读取 default 字段");

        let no_default = Declaration {
            kind: NodeKind(NodeKind::PROPERTY.to_string()),
            name: "y".into(),
            fqn: "C::y".into(),
            parent_fqn: None,
            span: Span::default(),
            extra: serde_json::Value::Null,
        };
        assert_eq!(
            super::property_value(&no_default),
            FactValue::Null,
            "无 default 应回退 Null"
        );
    }

    // ---- build_file via `run` (stub parser + in-memory fs) -----------------

    struct StubParser {
        facts: SyntaxFacts,
    }
    impl LanguageParser for StubParser {
        fn language(&self) -> Language {
            Language::new("php")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["php"]
        }
        fn parse(&self, _path: &str, _source: &str) -> DomainResult<SyntaxFacts> {
            Ok(self.facts.clone())
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['\\']
        }
        fn member_separator(&self) -> &'static str {
            "::"
        }
    }

    struct StubRegistry {
        parser: StubParser,
    }
    impl ParserRegistry for StubRegistry {
        fn parser_for(&self, _: &Language) -> Option<&dyn LanguageParser> {
            Some(&self.parser)
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("php")]
        }
    }

    struct MemFs {
        source: String,
    }
    impl FileSystem for MemFs {
        fn exists(&self, _: &std::path::Path) -> bool {
            true
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            false
        }
        fn read_to_string(&self, _: &std::path::Path) -> DomainResult<String> {
            Ok(self.source.clone())
        }
        fn len(&self, _: &std::path::Path) -> DomainResult<u64> {
            Ok(self.source.len() as u64)
        }
    }

    struct NoTechStack;
    impl TechStackRegistry for NoTechStack {
        fn adapter_for(&self, _: &Language) -> Option<&dyn gt_domain::port::TechStackAdapter> {
            None
        }
    }

    /// Drive `run` over a single in-memory file whose parser always returns `facts`.
    fn run_on(facts: SyntaxFacts) -> PipelineContext {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.lang_policy_default = php_policy();
        ctx.files.push(SourceFile {
            id: FileId(1),
            project_id: ProjectId(1),
            sub_project_id: None,
            path: "app/Order.php".into(),
            language: Language::new("php"),
            size_bytes: 10,
            content_hash: "x".into(),
        });
        let registry = StubRegistry {
            parser: StubParser { facts },
        };
        let fs = MemFs {
            source: "<?php".into(),
        };
        let tech = NoTechStack;
        super::run(&mut ctx, &registry, &fs, &tech);
        ctx
    }

    #[test]
    fn build_file_creates_class_method_and_declares_edge() {
        let mut facts = SyntaxFacts::default();
        facts.namespace = Some("app".into());
        facts.declarations = vec![
            Declaration {
                kind: NodeKind(NodeKind::CLASS.to_string()),
                name: "Order".into(),
                fqn: "app\\Order".into(),
                parent_fqn: None,
                span: Span::default(),
                extra: serde_json::Value::Null,
            },
            Declaration {
                kind: NodeKind(NodeKind::METHOD.to_string()),
                name: "pay".into(),
                fqn: "app\\Order::pay".into(),
                parent_fqn: Some("app\\Order".into()),
                span: Span::default(),
                extra: serde_json::Value::Null,
            },
        ];
        facts.call_sites = vec![CallSiteFact {
            owner_fqn: "app\\Order::pay".into(),
            owner_class: None,
            callee_text: "Db::name".into(),
            receiver: None,
            method: None,
            args: vec![],
            span: Span::default(),
            snippet: None,
            db_table: None,
            in_loop: false,
            entity: None,
        }];
        let ctx = run_on(facts);
        let class = ctx.ws.find_by_name("app\\Order").expect("class node");
        let method = ctx
            .ws
            .find_by_name("app\\Order::pay")
            .expect("method node");
        let has_declares = ctx.ws.edges().iter().any(|e| {
            e.from_id == class && e.to_id == method && e.kind.as_str() == EdgeKind::DECLARES
        });
        assert!(has_declares, "Class must DECLARES its method");
        assert_eq!(ctx.ws.calls.len(), 1, "one call site must be recorded");
    }

    #[test]
    fn build_file_namespace_node_is_deduped() {
        let mut facts = SyntaxFacts::default();
        facts.namespace = Some("app".into());
        // Two namespace declarations with the same FQN — only one node must be materialised.
        facts.declarations = vec![
            Declaration {
                kind: NodeKind(NodeKind::NAMESPACE.to_string()),
                name: "app".into(),
                fqn: "app".into(),
                parent_fqn: None,
                span: Span::default(),
                extra: serde_json::Value::Null,
            };
            2
        ];
        let ctx = run_on(facts);
        let ns_count = ctx
            .ws
            .node_ids()
            .iter()
            .filter(|id| {
                ctx.ws
                    .node(**id)
                    .map(|n| n.kind.as_str() == NodeKind::NAMESPACE)
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(ns_count, 1, "duplicate namespace FQN must dedup to one node");
        assert!(ctx.ws.find_by_name("app").is_some());
    }

    #[test]
    fn build_file_missing_base_class_gets_placeholder() {
        let mut facts = SyntaxFacts::default();
        facts.namespace = Some("app".into());
        facts.declarations = vec![Declaration {
            kind: NodeKind(NodeKind::CLASS.to_string()),
            name: "Order".into(),
            fqn: "app\\Order".into(),
            parent_fqn: None,
            span: Span::default(),
            extra: serde_json::Value::Null,
        }];
        facts.inheritances = vec![InheritanceFact {
            child_fqn: "app\\Order".into(),
            base_name: "BaseModel".into(),
            kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        // `BaseModel` is not in the graph -> resolved to `app\BaseModel` and a placeholder node is recorded.
        let base = ctx
            .ws
            .find_by_name("app\\BaseModel")
            .expect("missing base class must get a placeholder node");
        let order = ctx.ws.find_by_name("app\\Order").expect("child class");
        let has_extends = ctx.ws.edges().iter().any(|e| {
            e.from_id == order && e.to_id == base && e.kind.as_str() == EdgeKind::EXTENDS
        });
        assert!(has_extends, "child must EXTENDS the placeholder base");
    }

    #[test]
    fn build_file_import_table_lowercases_short_name() {
        let mut facts = SyntaxFacts::default();
        facts.imports = vec![ImportFact {
            alias: None,
            name: "app\\dao\\OrderDao".into(),
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        // short name takes the last `\`-segment and is stored lower-cased.
        assert!(
            ctx.ws.get_symbol("imports", "orderdao").is_some(),
            "import short name must be lower-cased in the symbol table"
        );
    }

    #[test]
    fn build_file_config_entries_pushed() {
        let mut facts = SyntaxFacts::default();
        facts.config_entries = vec![ConfigEntryFact {
            key_path: "listen.order".into(),
            value: FactValue::Null,
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        assert_eq!(ctx.ws.configs.len(), 1, "config entry must be pushed to ws.configs");
        assert_eq!(ctx.ws.configs[0].key_path, "listen.order");
    }
}
