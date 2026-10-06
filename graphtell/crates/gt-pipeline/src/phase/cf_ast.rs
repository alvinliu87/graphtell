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

    use gt_domain::error::{DomainError, Result as DomainResult};
    use gt_domain::model::{
        CallSiteFact, ConfigEntryFact, Declaration, EdgeKind, FactValue, FieldTypeFact, ImportFact,
        InheritanceFact, Language, NamespacePolicy, NodeKind, Project, ProjectId, ProjectStatus,
        SourceFile, FileId, Span, SyntaxFacts,
    };
    use gt_domain::model::syntax::HeaderAssignFact;
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
        assert!(super::is_builtin_type("?string", &policy), "the nullable `?` prefix must be stripped");
        assert!(super::is_builtin_type("INT", &policy), "case-insensitive");
        assert!(super::is_builtin_type(" Array ", &policy), "surrounding whitespace must be trimmed");
        assert!(!super::is_builtin_type("DateTime", &policy), "a non-builtin type must not match");
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
        assert_eq!(super::property_value(&with_default), expected, "must read the default field");

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
            "with no default it must fall back to Null"
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

    /// Build a context holding the given files (`id`, `path`), all PHP, sharing the PHP notation policy.
    fn context_with(files: &[(i64, &str)]) -> PipelineContext {
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
        for (id, path) in files {
            ctx.files.push(SourceFile {
                id: FileId(*id),
                project_id: ProjectId(1),
                sub_project_id: None,
                path: (*path).into(),
                language: Language::new("php"),
                size_bytes: 10,
                content_hash: "x".into(),
            });
        }
        ctx
    }

    /// Drive `run` over a single in-memory file whose parser always returns `facts`.
    fn run_on(facts: SyntaxFacts) -> PipelineContext {
        let mut ctx = context_with(&[(1, "app/Order.php")]);
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

    // ---- `run` degradation: no parser for the file's language -----------------

    struct NoParserRegistry;
    impl ParserRegistry for NoParserRegistry {
        fn parser_for(&self, _: &Language) -> Option<&dyn LanguageParser> {
            None
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![]
        }
    }

    /// `run` with no parser registered: it must still build the structural File node (degrade rather than
    /// drop the file) and raise both the human diagnostic and the machine-readable `unsupported_languages`
    /// signal, so the UI can banner the gap.
    fn run_on_no_parser() -> PipelineContext {
        let mut ctx = context_with(&[(1, "app/Order.php")]);
        let registry = NoParserRegistry;
        let fs = MemFs { source: "<?php".into() };
        let tech = NoTechStack;
        super::run(&mut ctx, &registry, &fs, &tech);
        ctx
    }

    // ---- failure injection: unreadable files, failing parsers, per-file facts ----

    struct ErrFs;
    impl FileSystem for ErrFs {
        fn exists(&self, _: &std::path::Path) -> bool {
            true
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            false
        }
        fn read_to_string(&self, _: &std::path::Path) -> DomainResult<String> {
            Err(DomainError::infra("unreadable"))
        }
        fn len(&self, _: &std::path::Path) -> DomainResult<u64> {
            Err(DomainError::infra("unreadable"))
        }
    }

    struct FailingParser;
    impl LanguageParser for FailingParser {
        fn language(&self) -> Language {
            Language::new("php")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["php"]
        }
        fn parse(&self, _path: &str, _source: &str) -> DomainResult<SyntaxFacts> {
            Err(DomainError::infra("boom"))
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['\\']
        }
        fn member_separator(&self) -> &'static str {
            "::"
        }
    }

    struct FailingRegistry {
        parser: FailingParser,
    }
    impl ParserRegistry for FailingRegistry {
        fn parser_for(&self, _: &Language) -> Option<&dyn LanguageParser> {
            Some(&self.parser)
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("php")]
        }
    }

    /// A parser whose answer depends on the file path, so several files with different facts can go
    /// through one `run` call.
    struct MultiParser {
        by_path: HashMap<String, SyntaxFacts>,
    }
    impl LanguageParser for MultiParser {
        fn language(&self) -> Language {
            Language::new("php")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["php"]
        }
        fn parse(&self, path: &str, _source: &str) -> DomainResult<SyntaxFacts> {
            Ok(self.by_path.get(path).cloned().unwrap_or_default())
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['\\']
        }
        fn member_separator(&self) -> &'static str {
            "::"
        }
    }

    struct MultiRegistry {
        parser: MultiParser,
    }
    impl ParserRegistry for MultiRegistry {
        fn parser_for(&self, _: &Language) -> Option<&dyn LanguageParser> {
            Some(&self.parser)
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("php")]
        }
    }

    /// Drive `run` over several files at once (`file id`, `path`, `facts`).
    fn run_paths(files: Vec<(i64, &str, SyntaxFacts)>) -> PipelineContext {
        let mut ctx = context_with(
            &files
                .iter()
                .map(|(id, path, _)| (*id, *path))
                .collect::<Vec<_>>(),
        );
        let registry = MultiRegistry {
            parser: MultiParser {
                by_path: files
                    .into_iter()
                    .map(|(_, path, facts)| (path.to_string(), facts))
                    .collect(),
            },
        };
        let fs = MemFs {
            source: "<?php".into(),
        };
        let tech = NoTechStack;
        super::run(&mut ctx, &registry, &fs, &tech);
        ctx
    }

    #[test]
    fn run_still_builds_file_node_and_signals_unsupported_without_a_parser() {
        let ctx = run_on_no_parser();
        // The structural File node is still materialised even though no semantic extraction happened.
        assert!(
            ctx.ws.find_by_name("app/Order.php").is_some(),
            "File node must still be built when no parser exists"
        );
        // `unsupported_languages` diagnostics + the machine-readable symbol are both emitted.
        assert!(
            ctx.ws.diagnostics.iter().any(|d| d.code == "NoParserForLanguage"),
            "should diagnose the missing parser"
        );
        assert!(
            ctx.ws.get_symbol("unsupported_languages", "php").is_some(),
            "should record unsupported_languages symbol"
        );
    }

    // ---- build_file: property / method-param / namespace-contains / import-alias / field-type / header / call-site

    #[test]
    fn build_file_property_records_default_value() {
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
                kind: NodeKind(NodeKind::PROPERTY.to_string()),
                name: "id".into(),
                fqn: "app\\Order::id".into(),
                parent_fqn: Some("app\\Order".into()),
                span: Span::default(),
                extra: serde_json::json!({ "default": { "t": "String", "v": "x" } }),
            },
        ];
        let ctx = run_on(facts);
        let order = ctx.ws.find_by_name("app\\Order").expect("class");
        let expected = serde_json::from_value::<FactValue>(serde_json::json!({ "t": "String", "v": "x" })).unwrap();
        assert_eq!(
            ctx.ws.property_of(order, "id"),
            Some(expected),
            "property default must be recorded on the owning class"
        );
    }

    #[test]
    fn build_file_method_param_type_inferred_and_builtin_skipped() {
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
                name: "save".into(),
                fqn: "app\\Order::save".into(),
                parent_fqn: Some("app\\Order".into()),
                span: Span::default(),
                extra: serde_json::json!({
                    "parameters": [
                        { "name": "svc", "type": "LoginService" },
                        { "name": "n", "type": "string" }
                    ]
                }),
            },
        ];
        let ctx = run_on(facts);
        // A non-builtin parameter type is resolved against the namespace (P7 later uses this to walk the
        // `$services->appAuth()` chain).
        assert_eq!(
            ctx.ws.param_type("app\\Order::save", "svc"),
            Some("app\\LoginService"),
            "non-builtin param type resolved against the namespace"
        );
        // Builtin `string` is skipped entirely so it never pollutes type inference.
        assert_eq!(
            ctx.ws.param_type("app\\Order::save", "n"),
            None,
            "builtin param type must not be recorded"
        );
    }

    #[test]
    fn build_file_namespace_contains_class() {
        let mut facts = SyntaxFacts::default();
        facts.namespace = Some("app".into());
        facts.declarations = vec![
            Declaration {
                kind: NodeKind(NodeKind::NAMESPACE.to_string()),
                name: "app".into(),
                fqn: "app".into(),
                parent_fqn: None,
                span: Span::default(),
                extra: serde_json::Value::Null,
            },
            Declaration {
                kind: NodeKind(NodeKind::CLASS.to_string()),
                name: "Order".into(),
                fqn: "app\\Order".into(),
                parent_fqn: None,
                span: Span::default(),
                extra: serde_json::Value::Null,
            },
        ];
        let ctx = run_on(facts);
        let ns = ctx.ws.find_by_name("app").expect("namespace");
        let order = ctx.ws.find_by_name("app\\Order").expect("class");
        assert!(
            ctx.ws.edges().iter().any(|e| e.from_id == ns
                && e.to_id == order
                && e.kind.as_str() == EdgeKind::CONTAINS),
            "namespace must CONTAINS the class"
        );
    }

    #[test]
    fn build_file_import_alias_used_as_short_name() {
        let mut facts = SyntaxFacts::default();
        facts.imports = vec![ImportFact {
            alias: Some("X".into()),
            name: "app\\dao\\OrderDao".into(),
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        // When an alias is given, the short name is the alias (still lower-cased), not the last segment.
        assert!(
            ctx.ws.get_symbol("imports", "x").is_some(),
            "import alias must be used as the (lower-cased) short name"
        );
        assert!(
            ctx.ws.get_symbol("imports", "orderdao").is_none(),
            "the last segment must not be used when an alias is present"
        );
    }

    #[test]
    fn build_file_field_type_sets_prop_type() {
        let mut facts = SyntaxFacts::default();
        facts.field_types = vec![FieldTypeFact {
            class_fqn: "app\\Order".into(),
            field: "mapper".into(),
            type_name: "app\\Dao".into(),
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        // Field-declaration types feed `prop_types`, letting P7 resolve `field.method()` instance calls.
        assert_eq!(
            ctx.ws.prop_type("app\\Order", "mapper"),
            Some("app\\Dao"),
            "field type must be resolved into a prop_type"
        );
    }

    #[test]
    fn build_file_header_assignment_node_created() {
        let mut facts = SyntaxFacts::default();
        facts.header_assignments = vec![HeaderAssignFact {
            key: "access-control-allow-origin".into(),
            rhs_snippet: "app()->request->header('origin')".into(),
            file: String::new(),
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        // The parse fact is promoted into a matchable node; the kernel only materialises it, the rule
        // (language-specific) decides whether it is a reflected-CORS problem.
        let nodes = ctx.ws.nodes_of_kind(NodeKind::HEADER_ASSIGNMENT);
        assert_eq!(nodes.len(), 1, "one HeaderAssignment node");
        let props = ctx.ws.node(nodes[0]).unwrap().properties.clone();
        assert_eq!(
            props.get("key").and_then(|v| v.as_str()),
            Some("access-control-allow-origin")
        );
        assert_eq!(
            props.get("rhs").and_then(|v| v.as_str()),
            Some("app()->request->header('origin')")
        );
    }

    #[test]
    fn build_file_call_site_records_snippet_and_in_loop() {
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
        facts.call_sites = vec![CallSiteFact {
            owner_fqn: "app\\Order".into(),
            owner_class: None,
            callee_text: "Db::name".into(),
            receiver: None,
            method: None,
            args: vec![],
            span: Span::default(),
            snippet: Some("Db::name('x')".into()),
            db_table: None,
            in_loop: true,
            entity: None,
        }];
        let ctx = run_on(facts);
        let nodes = ctx.ws.nodes_of_kind(NodeKind::CALL_SITE);
        assert_eq!(nodes.len(), 1, "one call site node");
        let props = ctx.ws.node(nodes[0]).unwrap().properties.clone();
        assert_eq!(
            props.get("snippet").and_then(|v| v.as_str()),
            Some("Db::name('x')")
        );
        assert_eq!(props.get("in_loop").and_then(|v| v.as_bool()), Some(true));
    }

    // ===================================================================
    // Negative / degradation coverage
    //
    // The tests above pin the happy paths. Everything below pins what must **not** happen: the
    // `continue` branches, the malformed-input fallbacks and the precedence rules that are invisible
    // in production (a silently skipped file looks exactly like an empty project) right up to the
    // moment someone changes them.
    // ===================================================================

    // ---- notation policy ----

    /// Regression guard: `NamespacePolicy::default()` used to *be* PHP's policy, so an unknown
    /// language silently inherited `\` / `::` / PHP's primitives. Empty means "no notation
    /// knowledge", and every consumer must degrade honestly instead of guessing.
    #[test]
    fn resolve_type_without_a_language_policy_never_invents_a_separator() {
        let policy = NamespacePolicy::default();
        let empty: HashMap<String, String> = HashMap::new();
        assert_eq!(
            super::resolve_type(Some("App"), &empty, "Foo", &policy),
            "Foo",
            "no namespace separator -> the bare name stays bare, never `App\\Foo`"
        );
        assert_eq!(
            super::resolve_type(None, &empty, "\\Foo\\Bar", &policy),
            "\\Foo\\Bar",
            "without separators nothing can be recognised as rooted"
        );
        assert_eq!(
            super::resolve_type(Some("App"), &empty, "App\\Foo", &policy),
            "App\\Foo",
            "nor as already qualified"
        );
    }

    #[test]
    fn resolve_type_import_hit_precedes_namespace_join() {
        let policy = php_policy();
        let mut imports: HashMap<String, String> = HashMap::new();
        imports.insert("foo".to_string(), "App\\Dao\\Foo".to_string());
        // An imported class wins over the current namespace, otherwise `use`-ed names would be
        // shadowed by whatever namespace the file happens to declare.
        assert_eq!(
            super::resolve_type(Some("App\\Service"), &imports, "Foo", &policy),
            "App\\Dao\\Foo"
        );
    }

    #[test]
    fn resolve_type_trims_the_written_name() {
        let policy = php_policy();
        let empty: HashMap<String, String> = HashMap::new();
        let mut imports: HashMap<String, String> = HashMap::new();
        imports.insert("foo".to_string(), "App\\Dao\\Foo".to_string());
        // Whitespace around the source text must neither leak into the FQN nor break the lookup.
        assert_eq!(super::resolve_type(Some("App"), &empty, "  Foo  ", &policy), "App\\Foo");
        assert_eq!(super::resolve_type(None, &imports, " Foo ", &policy), "App\\Dao\\Foo");
    }

    #[test]
    fn owner_parent_none_when_the_separator_is_at_the_start() {
        // `::pay` has an empty parent — distinct from "no separator at all", and it must not be
        // returned as a class FQN (an empty string would match nothing useful in `by_name`).
        assert_eq!(super::owner_parent("::pay", "::"), None);
    }

    #[test]
    fn owner_parent_splits_on_the_last_separator() {
        assert_eq!(
            super::owner_parent("App\\Order::pay::helper", "::"),
            Some("App\\Order::pay".to_string()),
            "`rfind` semantics: walk back one level only"
        );
    }

    #[test]
    fn owner_parent_with_an_empty_member_separator_does_not_panic() {
        // Reachable today: `NamespacePolicy::default()` has an empty member separator, so every
        // file whose language is not wired up lands here (via the unresolvable-owner fallback).
        // It degenerates to "the owner is its own parent", which can never resolve to *another*
        // node, so the caller falls through to the File node.
        assert_eq!(super::owner_parent("App\\Order", ""), Some("App\\Order".to_string()));
    }

    #[test]
    fn is_builtin_type_is_false_for_a_policy_without_primitives() {
        let policy = NamespacePolicy::default();
        assert!(!super::is_builtin_type("int", &policy));
        assert!(!super::is_builtin_type("?string", &policy), "no primitives declared -> nothing is skipped");
    }

    #[test]
    fn property_value_falls_back_to_null_on_a_malformed_default() {
        let decl = |extra| Declaration {
            kind: NodeKind(NodeKind::PROPERTY.to_string()),
            name: "x".into(),
            fqn: "app\\Order::x".into(),
            parent_fqn: Some("app\\Order".into()),
            span: Span::default(),
            extra,
        };
        assert_eq!(super::property_value(&decl(serde_json::json!({ "default": 42 }))), FactValue::Null);
        assert_eq!(
            super::property_value(&decl(serde_json::json!({ "default": { "t": "Nope", "v": 1 } }))),
            FactValue::Null,
            "an unknown FactValue tag must not panic"
        );
        assert_eq!(super::property_value(&decl(serde_json::json!({ "other": 1 }))), FactValue::Null);
        assert_eq!(super::property_value(&decl(serde_json::json!("not-an-object"))), FactValue::Null);
        assert_eq!(super::property_value(&decl(serde_json::Value::Null)), FactValue::Null);
    }

    // ---- file-level degradation ----

    #[test]
    fn run_skips_an_unreadable_file_and_builds_nothing() {
        let mut ctx = context_with(&[(1, "app/Order.php")]);
        let registry = StubRegistry {
            parser: StubParser {
                facts: SyntaxFacts::default(),
            },
        };
        let tech = NoTechStack;
        super::run(&mut ctx, &registry, &ErrFs, &tech);
        assert!(ctx.ws.node_ids().is_empty(), "an unreadable file must produce nothing at all");
        assert!(
            ctx.ws.file_node("app/Order.php").is_none(),
            "not even a File node: Ingest already accounted for it, CfAst must not invent a node for content it never read"
        );
        assert!(
            !ctx.ws.diagnostics.iter().any(|d| d.code == "NoParserForLanguage"),
            "unreadable is not the same as unsupported — no misleading banner"
        );
        assert!(ctx.ws.get_symbol("unsupported_languages", "php").is_none());
    }

    #[test]
    fn run_skips_a_file_whose_parse_fails() {
        let mut ctx = context_with(&[(1, "app/Order.php")]);
        let tech = NoTechStack;
        let fs = MemFs {
            source: "<?php".into(),
        };
        super::run(&mut ctx, &FailingRegistry { parser: FailingParser }, &fs, &tech);
        // Asymmetry worth knowing (pinned here): the *no parser* path still degrades to a File node,
        // but a file whose parse failed produces nothing. Half-built nodes from broken source would
        // pollute every downstream phase, so this is a deliberate choice, not an oversight.
        assert!(ctx.ws.node_ids().is_empty(), "no node may survive a failed parse");
        assert!(ctx.ws.file_node("app/Order.php").is_none());
        assert!(ctx.ws.diagnostics.is_empty(), "parse failures are logged (warn!), not diagnosed");
    }

    #[test]
    fn run_raises_no_unsupported_language_signal_when_a_parser_exists() {
        let ctx = run_on(SyntaxFacts::default());
        assert!(
            !ctx.ws.diagnostics.iter().any(|d| d.code == "NoParserForLanguage"),
            "a supported language must never be reported as unsupported"
        );
        assert!(ctx.ws.get_symbol("unsupported_languages", "php").is_none());
    }

    // ---- build_file: what must be dropped, skipped or left unresolved ----

    #[test]
    fn build_file_ignores_declaration_kinds_it_does_not_model() {
        let mut facts = SyntaxFacts::default();
        facts.declarations = vec![Declaration {
            kind: NodeKind("annotation".into()),
            name: "Route".into(),
            fqn: "app\\Order::Route".into(),
            parent_fqn: Some("app\\Order".into()),
            span: Span::default(),
            extra: serde_json::Value::Null,
        }];
        let ctx = run_on(facts);
        assert_eq!(
            ctx.ws.node_count(),
            1,
            "only the File node survives: an unmodelled kind must be dropped, not materialised under a stray NodeKind"
        );
        assert!(ctx.ws.find_by_name("app\\Order::Route").is_none());
    }

    #[test]
    fn build_file_inheritance_with_an_unknown_child_is_dropped() {
        let mut facts = SyntaxFacts::default();
        facts.namespace = Some("app".into());
        facts.inheritances = vec![InheritanceFact {
            child_fqn: "app\\Ghost".into(),
            base_name: "BaseModel".into(),
            kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        assert!(ctx.ws.inherits.is_empty(), "no supertype record without a child node declared in this file");
        assert!(
            ctx.ws.find_by_name("app\\BaseModel").is_none(),
            "no placeholder base either: the clause is skipped wholesale, not half-recorded"
        );
        assert!(ctx.ws.edges().is_empty(), "no dangling Extends edge into nowhere");
    }

    #[test]
    fn build_file_member_with_an_unknown_parent_gets_no_owner_edge() {
        let mut facts = SyntaxFacts::default();
        facts.declarations = vec![Declaration {
            kind: NodeKind(NodeKind::METHOD.to_string()),
            name: "pay".into(),
            fqn: "app\\Ghost::pay".into(),
            parent_fqn: Some("app\\Ghost".into()),
            span: Span::default(),
            extra: serde_json::Value::Null,
        }];
        let ctx = run_on(facts);
        let member = ctx
            .ws
            .find_by_name("app\\Ghost::pay")
            .expect("the member node itself is still built");
        assert!(
            !ctx.ws.edges().iter().any(|e| e.to_id == member),
            "an unresolvable parent must not be invented as an owner"
        );
    }

    #[test]
    fn build_file_call_site_owner_falls_back_to_the_declaring_class() {
        let mut facts = SyntaxFacts::default();
        facts.declarations = vec![Declaration {
            kind: NodeKind(NodeKind::CLASS.to_string()),
            name: "Order".into(),
            fqn: "app\\Order".into(),
            parent_fqn: None,
            span: Span::default(),
            extra: serde_json::Value::Null,
        }];
        // The method node is absent (parser only reported the class), so `Class::method` must walk
        // back one level through the member separator and land on the class.
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
        let class = ctx.ws.find_by_name("app\\Order").expect("class");
        let call = ctx.ws.nodes_of_kind(NodeKind::CALL_SITE)[0];
        assert_eq!(ctx.ws.calls[0].owner, class, "owner falls back to the declaring class");
        assert!(
            ctx.ws.edges().iter().any(|e| e.from_id == class
                && e.to_id == call
                && e.kind.as_str() == EdgeKind::HAS_CALL_SITE),
            "the class must own the call site"
        );
    }

    #[test]
    fn build_file_call_site_owner_falls_back_to_the_file_node() {
        let mut facts = SyntaxFacts::default();
        facts.call_sites = vec![CallSiteFact {
            owner_fqn: "app\\Ghost::pay".into(),
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
        let file = ctx.ws.file_node("app/Order.php").expect("file node");
        let call = ctx.ws.nodes_of_kind(NodeKind::CALL_SITE)[0];
        assert_eq!(ctx.ws.calls[0].owner, file, "a totally unresolvable owner lands on the File node");
        assert!(
            ctx.ws.edges().iter().any(|e| e.from_id == file
                && e.to_id == call
                && e.kind.as_str() == EdgeKind::HAS_CALL_SITE),
            "the file must own the orphan call site rather than it floating unattached"
        );
    }

    #[test]
    fn build_file_call_site_properties_collapse_to_null_when_empty() {
        let mut facts = SyntaxFacts::default();
        facts.call_sites = vec![
            CallSiteFact {
                owner_fqn: "app\\Order".into(),
                owner_class: None,
                callee_text: "plain".into(),
                receiver: None,
                method: None,
                args: vec![],
                span: Span::default(),
                snippet: None,
                db_table: None,
                in_loop: false,
                entity: None,
            },
            CallSiteFact {
                owner_fqn: "app\\Order".into(),
                owner_class: None,
                callee_text: "looping".into(),
                receiver: None,
                method: None,
                args: vec![],
                span: Span::default(),
                snippet: None,
                db_table: None,
                in_loop: true,
                entity: None,
            },
        ];
        let ctx = run_on(facts);
        let nodes = ctx.ws.nodes_of_kind(NodeKind::CALL_SITE);
        assert_eq!(nodes.len(), 2);
        let node_named = |callee: &str| {
            nodes
                .iter()
                .copied()
                .find(|id| ctx.ws.node(*id).and_then(|n| Some(n.name == callee)).unwrap_or(false))
                .expect("call node")
        };
        let plain = ctx.ws.node(node_named("plain")).unwrap().properties.clone();
        assert_eq!(
            plain,
            serde_json::Value::Null,
            "a call carrying neither snippet nor loop flag must store Null, not an empty object"
        );
        let looping = ctx.ws.node(node_named("looping")).unwrap().properties.clone();
        assert_eq!(looping.get("in_loop").and_then(|v| v.as_bool()), Some(true));
        assert!(looping.get("snippet").is_none(), "no snippet key when there is no snippet");
    }

    #[test]
    fn build_file_keeps_the_first_import_for_a_colliding_short_name() {
        let mut a = SyntaxFacts::default();
        a.imports = vec![ImportFact {
            alias: None,
            name: "app\\a\\Client".into(),
            span: Span::default(),
        }];
        let mut b = SyntaxFacts::default();
        b.imports = vec![ImportFact {
            alias: None,
            name: "app\\b\\Client".into(),
            span: Span::default(),
        }];
        let ctx = run_paths(vec![(1, "app/A.php", a), (2, "app/B.php", b)]);
        // The global short-name table is first-come-first-served and must never be silently
        // overwritten by a later file ...
        let global = ctx
            .ws
            .get_symbol("imports", "client")
            .and_then(|v| v.get("fqn"))
            .and_then(|v| v.as_str());
        assert_eq!(global, Some("app\\a\\Client"));
        // ... which is exactly why the per-file table exists: P7 resolves receivers per file.
        assert_eq!(
            ctx.ws.imports_of_file(1).and_then(|m| m.get("client")).map(String::as_str),
            Some("app\\a\\Client")
        );
        assert_eq!(
            ctx.ws.imports_of_file(2).and_then(|m| m.get("client")).map(String::as_str),
            Some("app\\b\\Client")
        );
    }

    #[test]
    fn build_file_constructor_injection_wins_over_a_declared_assign_type() {
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
                name: "__construct".into(),
                fqn: "app\\Order::__construct".into(),
                parent_fqn: Some("app\\Order".into()),
                span: Span::default(),
                extra: serde_json::json!({
                    "parameters": [{ "name": "svc", "type": "RealService" }],
                    "this_assigns": [{ "prop": "svc", "var": "svc" }],
                    "this_assign_types": [{ "prop": "svc", "class": "Declared" }]
                }),
            },
        ];
        let ctx = run_on(facts);
        assert_eq!(
            ctx.ws.prop_type("app\\Order", "svc"),
            Some("app\\RealService"),
            "the injected parameter type must win over the declaration-site type already in the index"
        );
    }

    #[test]
    fn build_file_assign_facts_missing_their_fields_are_skipped() {
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
                name: "save".into(),
                fqn: "app\\Order::save".into(),
                parent_fqn: Some("app\\Order".into()),
                span: Span::default(),
                extra: serde_json::json!({
                    "parameters": [{ "name": "svc", "type": "Svc" }],
                    "this_assigns": [
                        { "prop": "svc", "var": "nope" },
                        { "prop": "orphan" },
                        { "var": "svc" },
                        42
                    ],
                    "local_assign_types": [
                        { "var": "m", "class": "Model" },
                        { "class": "NoVar" },
                        { "var": "x" },
                        "nope"
                    ]
                }),
            },
        ];
        let ctx = run_on(facts);
        assert_eq!(
            ctx.ws.prop_type("app\\Order", "svc"),
            None,
            "`$this->svc = $nope` with no parameter of that name records nothing (and must not panic)"
        );
        assert_eq!(
            ctx.ws.param_type("app\\Order::save", "svc"),
            Some("app\\Svc"),
            "the parameter itself is unaffected"
        );
        assert_eq!(
            ctx.ws.local_type("app\\Order::save", "m"),
            Some("app\\Model"),
            "the well-formed local assignment still lands"
        );
        assert_eq!(ctx.ws.local_type("app\\Order::save", "x"), None, "the malformed ones are dropped");
    }

    #[test]
    fn build_file_this_assign_without_a_parent_class_skips_everything_below_it() {
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
                name: "__construct".into(),
                fqn: "app\\Order::__construct".into(),
                parent_fqn: None,
                span: Span::default(),
                extra: serde_json::json!({
                    "parameters": [{ "name": "svc", "type": "Real" }],
                    "this_assigns": [{ "prop": "svc", "var": "svc" }],
                    "returns_class": ["Dao"]
                }),
            },
        ];
        let ctx = run_on(facts);
        let ctor = ctx
            .ws
            .find_by_name("app\\Order::__construct")
            .expect("the node itself is still created");
        assert!(
            !ctx.ws.edges().iter().any(|e| e.to_id == ctor),
            "known wart, pinned: `let Some(class_fqn) = ... else {{ continue }}` targets the *declarations* loop, \
             so a method carrying `this_assigns` without a `parent_fqn` loses every step below it, including the owner edge"
        );
        assert!(ctx.ws.pending_links.is_empty(), "the `returns_class` step is skipped too");
    }

    #[test]
    fn build_file_magic_methods_ignore_non_strings_and_empty_names() {
        let mut facts = SyntaxFacts::default();
        facts.namespace = Some("app".into());
        facts.declarations = vec![
            Declaration {
                kind: NodeKind(NodeKind::CLASS.to_string()),
                name: "Order".into(),
                fqn: "app\\Order".into(),
                parent_fqn: None,
                span: Span::default(),
                extra: serde_json::json!({ "magic_methods": ["find", 42, null, "", "pay"] }),
            },
            Declaration {
                kind: NodeKind(NodeKind::CLASS.to_string()),
                name: "Plain".into(),
                fqn: "app\\Plain".into(),
                parent_fqn: None,
                span: Span::default(),
                extra: serde_json::Value::Null,
            },
        ];
        let ctx = run_on(facts);
        assert!(ctx.ws.declares_magic_method("app\\Order", "find"));
        assert!(ctx.ws.declares_magic_method("app\\Order", "pay"));
        assert!(!ctx.ws.declares_magic_method("app\\Order", ""), "empty names must be filtered out");
        assert!(
            !ctx.ws.declares_magic_method("app\\Plain", "anything"),
            "a class without `@method` declares nothing"
        );
    }

    #[test]
    fn build_file_returns_class_needs_a_known_owner_and_string_entries() {
        // (a) no owning class node -> nothing can be queued.
        let mut facts = SyntaxFacts::default();
        facts.namespace = Some("app".into());
        facts.declarations = vec![Declaration {
            kind: NodeKind(NodeKind::METHOD.to_string()),
            name: "dao".into(),
            fqn: "app\\Ghost::dao".into(),
            parent_fqn: None,
            span: Span::default(),
            extra: serde_json::json!({ "returns_class": ["Dao"] }),
        }];
        let ctx = run_on(facts);
        assert!(ctx.ws.pending_links.is_empty(), "no owner node -> no link may be queued");

        // (b) non-string entries are dropped; well-formed ones are resolved against the namespace.
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
                name: "dao".into(),
                fqn: "app\\Order::dao".into(),
                parent_fqn: Some("app\\Order".into()),
                span: Span::default(),
                extra: serde_json::json!({ "returns_class": [42, "Dao", null, true] }),
            },
        ];
        let ctx = run_on(facts);
        assert_eq!(ctx.ws.pending_links.len(), 1, "only the string entry produces a link");
        assert_eq!(ctx.ws.pending_links[0].raw, "app\\Dao");
    }

    #[test]
    fn build_file_config_entry_carries_the_stem_and_no_locale_without_an_adapter() {
        let mut facts = SyntaxFacts::default();
        facts.config_entries = vec![ConfigEntryFact {
            key_path: "listen.order".into(),
            value: FactValue::Null,
            span: Span::default(),
        }];
        let ctx = run_on(facts);
        assert_eq!(ctx.ws.configs[0].file, "app/Order.php");
        assert_eq!(ctx.ws.configs[0].file_stem.as_deref(), Some("Order"));
        assert_eq!(
            ctx.ws.configs[0].locale,
            None,
            "with no tech-stack adapter for the language no locale layout may be guessed — the old \
             hard-coded `lang/{{locale}}/*.php` never matched anything and silently left locale empty"
        );
    }
}
