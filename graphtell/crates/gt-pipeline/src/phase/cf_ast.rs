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

    // CORS header-assignment fact: add the file path, so `phase::cors` aligns with the call site by (file, line).
    for h in &facts.header_assignments {
        ctx.ws.header_assignments.push(gt_domain::model::syntax::HeaderAssignFact {
            key: h.key.clone(),
            rhs_snippet: h.rhs_snippet.clone(),
            file: file.path.clone(),
            span: h.span,
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
    match ns {
        Some(ns) if !ns.is_empty() => format!("{}{}{}", ns, policy.ns_separator, raw),
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
