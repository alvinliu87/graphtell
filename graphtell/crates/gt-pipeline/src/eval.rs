//! Evaluation of [`ValueSource`]: turning the YAML declaration of "where a value comes from" into an actual string / literal.

use gt_domain::model::{ExpandVariant, FactValue, NodeId, ResolveAs, ValueSource};

use crate::normalize::{apply_normalize, apply_table_prefix_steps, apply_transform};
use crate::workspace::{CallRecord, ConfigRecord, GraphWorkspace, InheritRecord};

/// The context in which a rule matches.
#[derive(Debug, Clone, Copy)]
pub enum MatchCtx<'a> {
    Call(&'a CallRecord),
    Config(&'a ConfigRecord),
    Inherit(&'a InheritRecord),
    Node(NodeId),
}

/// The evaluation result.
#[derive(Debug, Clone)]
pub enum EvalValue {
    Str(String),
    Fact(FactValue),
    List(Vec<FactValue>),
}

impl EvalValue {
    pub fn as_string(&self) -> String {
        match self {
            EvalValue::Str(s) => s.clone(),
            EvalValue::Fact(FactValue::String(s)) | EvalValue::Fact(FactValue::ClassConst(s)) => s.clone(),
            EvalValue::Fact(FactValue::Int(i)) => i.to_string(),
            EvalValue::Fact(FactValue::Float(f)) => f.to_string(),
            EvalValue::Fact(FactValue::Bool(b)) => b.to_string(),
            EvalValue::Fact(FactValue::Null) => String::new(),
            EvalValue::Fact(FactValue::Array(_)) => String::new(),
            EvalValue::Fact(FactValue::Unknown(v)) => v.clone().unwrap_or_default(),
            EvalValue::List(_) => String::new(),
        }
    }

    pub fn as_fact(&self) -> FactValue {
        match self {
            EvalValue::Fact(f) => f.clone(),
            other => FactValue::String(other.as_string()),
        }
    }
}

/// The evaluator.
pub struct Evaluator<'a> {
    ws: &'a GraphWorkspace,
    ctx: MatchCtx<'a>,
    /// The current expansion variant (`Synthesize.expand`): when non-empty, `{ expand_method: true }` /
    /// `{ expand_entry: true }` have values.
    variant: Option<ExpandVariant>,
}

impl<'a> Evaluator<'a> {
    pub fn new(ws: &'a GraphWorkspace, ctx: MatchCtx<'a>) -> Self {
        Self { ws, ctx, variant: None }
    }

    /// Bind the current expansion variant (set as the expansion table is executed row by row).
    pub fn with_variant(mut self, v: Option<ExpandVariant>) -> Self {
        self.variant = v;
        self
    }

    pub fn ws(&self) -> &'a GraphWorkspace {
        self.ws
    }

    /// The current match context (so identity computation can read environment information such as "the file / line
    /// the call site is on", e.g. to complete the `Route::group` route-group prefix).
    pub fn ctx(&self) -> MatchCtx<'a> {
        self.ctx
    }

    /// Evaluate and take a string (applying resolve -> transform -> normalize in order).
    pub fn string(&self, src: &ValueSource) -> Option<String> {
        let raw = self.raw(src)?;
        if src.require_literal == Some(true) {
            let is_literal = matches!(raw, EvalValue::Str(_))
                || matches!(
                    raw,
                    EvalValue::Fact(
                        FactValue::String(_)
                            | FactValue::Int(_)
                            | FactValue::Float(_)
                            | FactValue::Bool(_)
                    )
                );
            if !is_literal {
                return None;
            }
        }
        let mut s = raw.as_string();
        if s.is_empty() {
            if let Some(d) = &src.default {
                s = d.clone();
            } else {
                return None;
            }
        }
        if let Some(resolve) = &src.resolve {
            s = match resolve {
                ResolveAs::ClassConst => {
                    let resolved = self.resolve_name(&s);
                    // `require_class`: the resolved result must be a class that really exists in the codebase,
                    // otherwise the whole thing counts as unavailable (triggering `value_fallback`). Stops a
                    // variable name like `$action` from being used as a class.
                    if src.require_class == Some(true)
                        && self.ws.find_by_name(&resolved).is_none()
                        && self.ws.resolve_short_name(&resolved).is_none()
                    {
                        return None;
                    }
                    resolved
                }
                ResolveAs::AsIs => self.resolve_name(&s),
                ResolveAs::MethodRef | ResolveAs::ByAlias => s,
            };
        }
        if let Some(t) = &src.transform {
            s = apply_transform(&s, t);
        }
        if let Some(steps) = &src.normalize {
            let resolved = apply_table_prefix_steps(steps, self.ws.table_prefixes());
            s = apply_normalize(&s, &resolved);
        }
        Some(s)
    }

    /// Evaluate and take a set of values (`array_values` — one-to-many, e.g. event listeners).
    pub fn list(&self, src: &ValueSource) -> Vec<FactValue> {
        match self.raw(src) {
            Some(EvalValue::List(items)) => items,
            Some(other) => vec![other.as_fact()],
            None => Vec::new(),
        }
    }

    /// Resolve a short name / alias into a fully qualified name (use the lookup result when found, otherwise return it as-is).
    pub fn resolve_name(&self, name: &str) -> String {
        let trimmed = name.trim_start_matches('\\');
        if self.ws.find_by_name(trimmed).is_some() {
            return trimmed.to_string();
        }
        self.ws
            .resolve_short_name(trimmed)
            .unwrap_or_else(|| trimmed.to_string())
    }

    fn raw(&self, src: &ValueSource) -> Option<EvalValue> {
        // The expansion variant's injected values: when one call becomes N semantic nodes, the method / entry method vary per variant.
        if src.expand_method == Some(true) {
            return self
                .variant
                .as_ref()
                .and_then(|v| v.method.clone())
                .map(EvalValue::Str);
        }
        if src.expand_entry == Some(true) {
            return self
                .variant
                .as_ref()
                .and_then(|v| v.entry.clone())
                .map(EvalValue::Str);
        }
        if let Some(lit) = &src.literal {
            return Some(EvalValue::Str(lit.clone()));
        }
        if let Some(parts) = &src.path {
            let join = src.join.clone().unwrap_or_default();
            let mut out = Vec::new();
            for p in parts {
                if let Some(v) = self.raw(p) {
                    out.push(v.as_string());
                }
            }
            if out.is_empty() {
                return None;
            }
            return Some(EvalValue::Str(out.join(&join)));
        }
        if let Some(inner) = &src.source {
            let v = self.raw(inner)?;
            if let Some(field) = &src.field {
                return match v {
                    EvalValue::Fact(FactValue::Array(items)) => items
                        .iter()
                        .find(|(k, _)| k == field || k.trim_matches('\'') == field)
                        .map(|(_, v)| EvalValue::Fact(v.clone())),
                    other => Some(other),
                };
            }
            return Some(v);
        }

        match self.ctx {
            MatchCtx::Call(c) => {
                if src.method_name == Some(true) {
                    return c.method.clone().map(EvalValue::Str);
                }
                if let Some(i) = src.arg {
                    let a = c.args.get(i)?;
                    if let Some(field) = &src.field {
                        if let FactValue::Array(items) = a {
                            return items
                                .iter()
                                .find(|(k, _)| k == field || k.trim_matches('\'') == field)
                                .map(|(_, v)| EvalValue::Fact(v.clone()));
                        }
                        return None;
                    }
                    // Take an array element by index: `[Ctrl::class, 'method']` -> items 0 / 1.
                    if let Some(idx) = src.element {
                        if let FactValue::Array(items) = a {
                            return items.get(idx).map(|(_, v)| EvalValue::Fact(v.clone()));
                        }
                        return None;
                    }
                    return Some(EvalValue::Fact(a.clone()));
                }
                if src.owner_class == Some(true) {
                    if let Some(cls) = &c.owner_class {
                        if !cls.is_empty() {
                            return Some(EvalValue::Str(cls.clone()));
                        }
                    }
                    if c.owner_fqn.is_empty() {
                        return None;
                    }
                    // Drop the trailing member part, keeping the class FQN.
                    // The member separator varies by language: PHP `::`, Java `.`
                    // （`com.example.Ctrl.list` → `com.example.Ctrl`）。
                    let class = c
                        .owner_fqn
                        .rfind("::")
                        .or_else(|| c.owner_fqn.rfind('.'))
                        .map(|idx| &c.owner_fqn[..idx])
                        .unwrap_or(&c.owner_fqn[..]);
                    return Some(EvalValue::Str(class.to_string()));
                }
                if src.owner_member == Some(true) {
                    if c.owner_fqn.is_empty() {
                        return None;
                    }
                    let member = &c.owner_fqn[c
                        .owner_fqn
                        .rfind("::")
                        .or_else(|| c.owner_fqn.rfind('.'))
                        .map(|idx| idx + 1)
                        .unwrap_or(0)..];
                    if member.is_empty() {
                        return None;
                    }
                    return Some(EvalValue::Str(member.to_string()));
                }
                if src.receiver_class == Some(true) {
                    let recv = c.receiver.as_ref()?;
                    // Restore via the import alias (e.g. `QueueThink` -> `think\facade\Queue`).
                    let fqn = self
                        .ws
                        .resolve_import_alias(recv)
                        .unwrap_or_else(|| recv.clone());
                    return Some(EvalValue::Str(fqn));
                }
                if src.entity == Some(true) {
                    // The event / primary domain type (e.g. `OrderPlacedEvent`): publisher and subscriber merge
                    // onto the same semantic node from this. Returns None when unavailable, triggering `value_fallback`.
                    let e = c.entity.as_ref()?;
                    if e.is_empty() {
                        return None;
                    }
                    return Some(EvalValue::Str(e.clone()));
                }
                None
            }
            MatchCtx::Config(c) => {
                if src.key_path == Some(true) {
                    return Some(EvalValue::Str(c.key_path.clone()));
                }
                if src.entry_value == Some(true) {
                    return Some(EvalValue::Fact(c.value.clone()));
                }
                if src.file_stem == Some(true) {
                    return Some(EvalValue::Str(c.file_stem.clone().unwrap_or_default()));
                }
                if src.locale == Some(true) {
                    return Some(EvalValue::Str(c.locale.clone().unwrap_or_default()));
                }
                if src.array_values == Some(true) {
                    return Some(EvalValue::List(
                        c.value.array_values().into_iter().cloned().collect(),
                    ));
                }
                if src.array_length == Some(true) {
                    return Some(EvalValue::Str(c.value.array_len().to_string()));
                }
                if let Some(n) = src.path_segment {
                    let segs: Vec<&str> = c.key_path.split('.').collect();
                    if n < segs.len() {
                        return Some(EvalValue::Str(segs[n..].join(".")));
                    }
                    return None;
                }
                None
            }
            MatchCtx::Inherit(i) => {
                if src.self_value == Some(true) {
                    return Some(EvalValue::Str(i.child_fqn.clone()));
                }
                if let Some(p) = &src.property {
                    return self.ws.property_of(i.child, p).map(EvalValue::Fact);
                }
                None
            }
            MatchCtx::Node(id) => {
                if src.self_value == Some(true) {
                    return self
                        .ws
                        .node(id)
                        .map(|n| EvalValue::Str(n.fqn.clone().unwrap_or_else(|| n.name.clone())));
                }
                if let Some(p) = &src.property {
                    return self.ws.property_of(id, p).map(EvalValue::Fact);
                }
                None
            }
        }
    }
}
