//! P2 CfAst：从语言语法创建语法级节点与边。
//!
//! 产物：`Class` / `Interface` / `Trait` / `Enum` / `Method` / `Function` /
//! `Property` / `Const` / `Namespace` / `CallSite` 节点，
//! `imports` 表（短名 → FQN）、`by_name` 索引（FQN → NodeId），
//! 以及继承 / 实现 / trait 边。
//!
//! 本阶段**完全语言无关**：它只消费 `SyntaxFacts`。

use std::collections::HashMap;

use em_domain::model::{
    Declaration, EdgeKind, FactValue, NewEdge, NewNode, NodeId, NodeKind, Phase, ProjectId,
    ResolveAs, Severity, SourceFile, Span, SyntaxFacts,
};
use em_domain::port::{FileSystem, ParserRegistry};
use tracing::{debug, warn};

use crate::context::PipelineContext;

/// 执行 CfAst。
pub fn run(ctx: &mut PipelineContext, parsers: &dyn ParserRegistry, fs: &dyn FileSystem) {
    let phase = Phase(Phase::CF_AST.to_string());
    let project_id = ctx.project.id;
    let root = ctx.project.root_path.clone();
    let files: Vec<SourceFile> = ctx.files.clone();

    for file in files {
        let abs = root.join(&file.path);
        let source = match fs.read_to_string(&abs) {
            Ok(s) => s,
            Err(e) => {
                debug!("跳过无法读取的文件 {}: {e}", file.path);
                continue;
            }
        };
        let Some(parser) = parsers.parser_for(&file.language) else {
            continue;
        };
        let facts: SyntaxFacts = match parser.parse(&file.path, &source) {
            Ok(f) => f,
            Err(e) => {
                warn!("解析失败 {}: {e}", file.path);
                continue;
            }
        };
        build_file(ctx, project_id, &file, &facts, &phase);
    }
}

fn build_file(
    ctx: &mut PipelineContext,
    project_id: ProjectId,
    file: &SourceFile,
    facts: &SyntaxFacts,
    phase: &Phase,
) {
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

    // 导入表：短名(小写) → FQN（class 短名映射到 fully qualified name）
    let mut imports: HashMap<String, String> = HashMap::new();
    for imp in &facts.imports {
        let fqn = imp.name.trim_start_matches('\\').to_string();
        let short = match &imp.alias {
            Some(a) => a.clone(),
            None => fqn.rsplit('\\').next().unwrap_or(&fqn).to_string(),
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
    // 留一份**本文件**的导入表供 P7 解析调用接收者：全局 `imports` 符号表是先到先得的，
    // 同名短名在不同文件里指向不同类时它必然出错，只有按文件存才符合 PHP 的解析规则。
    ctx.ws
        .record_file_imports(file.id.get(), &file.path, imports.clone());

    // Namespace 节点（按 FQN 去重）
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

    // 类型节点（先建类，再建成员，保证 by_name 可用）
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

    // 成员节点：Method / Function / Property / Const / EnumCase
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
            // 参数类型：`__construct(LoginServices $services)` → 变量 services : LoginServices。
            // 供 P7 解析 `$services->appAuth()`（按变量类型）。
            if let Some(params) = d.extra.get("parameters").and_then(|v| v.as_array()) {
                for p in params {
                    let (Some(var), Some(ty)) = (
                        p.get("name").and_then(|v| v.as_str()),
                        p.get("type").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    if is_builtin_type(ty) {
                        continue;
                    }
                    let fqn = resolve_type(
                        facts.namespace.as_deref(),
                        &imports,
                        ty.trim_start_matches('?'),
                    );
                    // 变量名统一去掉 `$` 前缀，便于按 `$var` 查表。
                    ctx.ws
                        .add_param_type(&d.fqn, var.trim_start_matches('$'), &fqn);
                }
            }
            // 构造器注入：`$this->services = $services` → 属性 services 的类型 = 参数 services 的类型。
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
            // `return X::class;` → 声明式联系：所属类 → X（如 `Dao::setModel()` → Model）。
            // 跨文件引用交给 P7 统一解析（此时目标类可能尚未建节点）。
            if let Some(list) = d.extra.get("returns_class").and_then(|v| v.as_array()) {
                if let Some(owner_id) = d.parent_fqn.as_ref().and_then(|p| local.get(p).copied()) {
                    for c in list {
                        let Some(name) = c.as_str() else { continue };
                        let fqn = resolve_type(facts.namespace.as_deref(), &imports, name);
                        ctx.ws.pending_links.push(crate::workspace::PendingLink {
                            from: owner_id,
                            kind: EdgeKind(EdgeKind::RESOLVES_TO.to_string()),
                            raw: fqn,
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

    // 继承 / 实现 / trait
    for inh in &facts.inheritances {
        let base_fqn = resolve_type(facts.namespace.as_deref(), &imports, &inh.base_name);
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
        // 基类可能在 vendor 等被排除目录里（不在图内）。继承关系本身**必须**
        // 被记录——下游「继承链」推导（如模型→表）依赖它，否则整条都丢。
        // 因此为被排除的基类建一个占位节点并连边。
        let target = match target {
            Some(t) => t,
            None => {
                ctx.ws.diagnose(
                    phase,
                    "AliasTargetMissing",
                    Severity::Info,
                    format!(
                        "基类 {} 不在图内（可能被 Ingest 排除），已记占位节点",
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
                        .rsplit('\\')
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

    // 调用点：细化到 CallSite 节点
    for call in &facts.call_sites {
        let owner = local
            .get(&call.owner_fqn)
            .copied()
            .or_else(|| ctx.ws.find_by_name(&call.owner_fqn))
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
            // 调用语句原文：供 UI 在链路的"调用处"直接显示，便于一眼核验。
            properties: match &call.snippet {
                Some(s) => serde_json::json!({ "snippet": s }),
                None => serde_json::Value::Null,
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
            callee: call.callee_text.clone(),
            receiver: call.receiver.clone(),
            method: call.method.clone(),
            args: call.args.clone(),
            span: call.span,
            file: file.path.clone(),
            sub: file.sub_project_id,
            language: file.language.clone(),
        });
    }

    // 配置条目（`return [...]` 型文件）
    let locale = crate::engine::capture_locale("lang/{locale}/*.php", &file.path);
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
}

fn property_value(d: &Declaration) -> FactValue {
    d.extra
        .get("default")
        .and_then(|v| serde_json::from_value::<FactValue>(v.clone()).ok())
        .unwrap_or(FactValue::Null)
}

/// 把源码里写的类型名解析为完全限定名。
pub fn resolve_type(ns: Option<&str>, imports: &HashMap<String, String>, name: &str) -> String {
    let raw = name.trim();
    if raw.starts_with('\\') {
        return raw.trim_start_matches('\\').to_string();
    }
    if raw.contains('\\') {
        return raw.to_string();
    }
    if let Some(fqn) = imports.get(&raw.to_ascii_lowercase()) {
        return fqn.clone();
    }
    match ns {
        Some(ns) if !ns.is_empty() => format!("{}\\{}", ns, raw),
        _ => raw.to_string(),
    }
}

/// 是否为语言内建类型（`int` / `string` / `array`…），不是类名，跳过后不参与类型推断。
fn is_builtin_type(t: &str) -> bool {
    matches!(
        t.trim_start_matches('?').trim().to_ascii_lowercase().as_str(),
        "int"
            | "integer"
            | "string"
            | "bool"
            | "boolean"
            | "float"
            | "double"
            | "array"
            | "void"
            | "mixed"
            | "object"
            | "callable"
            | "iterable"
            | "null"
            | "false"
            | "true"
            | "self"
            | "static"
            | "parent"
            | "never"
    )
}
