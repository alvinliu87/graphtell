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
        // Checked **after** normalisation: `leading_slash` turns `https://x/y` into `/https://x/y`, so testing
        // the raw value would miss exactly the case this guard exists for.
        if let Some(needles) = &src.reject_contains {
            if needles.iter().any(|n| !n.is_empty() && s.contains(n.as_str())) {
                return None;
            }
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
                    // The member separator is **1 or 2 characters** (`::` for PHP, `.` for Java / JS), and
                    // `rfind` yields the index of its *first* character — so the slice must start after the
                    // whole separator, otherwise PHP yields `:create` instead of `create`.
                    let sep = c
                        .owner_fqn
                        .rfind("::")
                        .map(|i| (i, 2))
                        .or_else(|| c.owner_fqn.rfind('.').map(|i| (i, 1)));
                    let member = &c.owner_fqn[sep.map(|(i, len)| i + len).unwrap_or(0)..];
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

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::{
        EdgeKind, ExpandVariant, Language, NewNode, NodeKind, NormalizeStep, Phase, ProjectId, Span,
        TransformSpec,
    };

    fn ws() -> GraphWorkspace {
        GraphWorkspace::new(ProjectId(1))
    }

    fn add_node(ws: &mut GraphWorkspace, kind: &str, name: &str, fqn: Option<&str>) -> NodeId {
        ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind::from(kind),
            name: name.to_string(),
            fqn: fqn.map(|s| s.to_string()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::default(),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    fn call_with(
        owner_fqn: &str,
        owner_class: Option<&str>,
        receiver: Option<&str>,
        entity: Option<&str>,
    ) -> CallRecord {
        CallRecord {
            node: NodeId(1),
            owner: NodeId(2),
            owner_fqn: owner_fqn.to_string(),
            owner_class: owner_class.map(|s| s.to_string()),
            callee: "Db::name".to_string(),
            receiver: receiver.map(|s| s.to_string()),
            method: Some("name".to_string()),
            args: Vec::new(),
            db_table: None,
            in_loop: false,
            entity: entity.map(|s| s.to_string()),
            span: Span::default(),
            file: "app/Svc.php".to_string(),
            sub: None,
            language: Language::default(),
        }
    }

    fn call(args: Vec<FactValue>) -> CallRecord {
        let mut rec = call_with("app\\Svc::run", None, Some("Db"), None);
        rec.args = args;
        rec
    }

    fn cfg(value: FactValue) -> ConfigRecord {
        ConfigRecord {
            file: "app/lang/zh-cn.php".to_string(),
            key_path: "user.greeting.title".to_string(),
            value,
            span: Span::default(),
            sub: None,
            locale: Some("zh-cn".to_string()),
            file_stem: Some("zh-cn".to_string()),
        }
    }

    fn inherit() -> InheritRecord {
        InheritRecord {
            child: NodeId(3),
            child_fqn: "app\\Model\\User".to_string(),
            base: "Model".to_string(),
            kind: EdgeKind("Extends".to_string()),
            sub: None,
            file: "app/Model/User.php".to_string(),
            span: Span::default(),
        }
    }

    // ---------------------------------------------------------------- EvalValue

    #[test]
    fn eval_value_as_string_covers_every_fact_form() {
        assert_eq!(EvalValue::Str("x".to_string()).as_string(), "x");
        assert_eq!(
            EvalValue::Fact(FactValue::String("s".into())).as_string(),
            "s"
        );
        assert_eq!(EvalValue::Fact(FactValue::ClassConst("C".into())).as_string(), "C");
        assert_eq!(EvalValue::Fact(FactValue::Int(-3)).as_string(), "-3");
        assert_eq!(EvalValue::Fact(FactValue::Float(1.5)).as_string(), "1.5");
        assert_eq!(EvalValue::Fact(FactValue::Bool(true)).as_string(), "true");
        // An unevaluated variable still carries its source text — that is what `Unknown` is for.
        assert_eq!(
            EvalValue::Fact(FactValue::Unknown(Some("$sql".into()))).as_string(),
            "$sql"
        );
        // Nothing usable to show: null / array / an unnamed unknown / a list all render empty.
        assert_eq!(EvalValue::Fact(FactValue::Null).as_string(), "");
        assert_eq!(EvalValue::Fact(FactValue::Unknown(None)).as_string(), "");
        assert_eq!(EvalValue::Fact(FactValue::Array(vec![])).as_string(), "");
        assert_eq!(EvalValue::List(vec![FactValue::Int(1)]).as_string(), "");
    }

    #[test]
    fn eval_value_as_fact_preserves_facts_and_wraps_the_rest() {
        assert_eq!(EvalValue::Fact(FactValue::Int(7)).as_fact(), FactValue::Int(7));
        assert_eq!(
            EvalValue::Str("s".to_string()).as_fact(),
            FactValue::String("s".to_string())
        );
        // A list has no single fact: it collapses to an empty string rather than panicking.
        assert_eq!(
            EvalValue::List(vec![FactValue::Int(1)]).as_fact(),
            FactValue::String(String::new())
        );
    }

    // ---------------------------------------------------------------- `string()` gates

    #[test]
    fn string_reads_literals_and_applies_the_default_only_when_empty() {
        let ws = ws();
        let ev = Evaluator::new(&ws, MatchCtx::Node(NodeId(1)));

        assert_eq!(
            ev.string(&ValueSource {
                literal: Some("orders".into()),
                ..Default::default()
            }),
            Some("orders".to_string())
        );
        // Nothing readable at all -> unavailable, so `value_fallback` can take over.
        assert_eq!(ev.string(&ValueSource::default()), None);
        // The default covers "evaluated but empty", never "could not be evaluated".
        assert_eq!(
            ev.string(&ValueSource {
                default: Some("fb".into()),
                ..Default::default()
            }),
            None,
            "a default must not replace a value that cannot be obtained"
        );
        assert_eq!(
            ev.string(&ValueSource {
                literal: Some("".into()),
                default: Some("fb".into()),
                ..Default::default()
            }),
            Some("fb".to_string())
        );
    }

    /// `require_literal` is what stops source text like `$name` from becoming a semantic identity.
    #[test]
    fn string_require_literal_rejects_variables() {
        let ws = ws();
        let rec = call(vec![
            FactValue::String("orders".into()),
            FactValue::Unknown(Some("$name".into())),
            FactValue::Int(3),
            FactValue::Bool(false),
        ]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let src = |i: usize, require: bool| ValueSource {
            arg: Some(i),
            require_literal: if require { Some(true) } else { None },
            ..Default::default()
        };

        assert_eq!(ev.string(&src(0, true)), Some("orders".to_string()));
        assert_eq!(ev.string(&src(1, true)), None, "a variable cannot serve as an identity");
        assert_eq!(ev.string(&src(1, false)), Some("$name".to_string()));
        // Scalars count as literals.
        assert_eq!(ev.string(&src(2, true)), Some("3".to_string()));
        assert_eq!(ev.string(&src(3, true)), Some("false".to_string()));
    }

    /// `reject_contains` is a **scope** guard, checked after normalisation: the canonical case is a front end
    /// calling a third-party API — the URL is a literal, so `require_literal` happily accepts it, and only this
    /// test keeps an invented `HttpContract` off the graph. It must run after `leading_slash`, which rewrites
    /// `https://x/y` into `/https://x/y`, i.e. the scheme stops being at the start of the string.
    #[test]
    fn string_reject_contains_drops_out_of_scope_values_after_normalisation() {
        let ws = ws();
        let rec = call(vec![
            FactValue::String("https://api.paypal.com/v2/checkout/orders".into()),
            FactValue::String("/api/delete".into()),
        ]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let src = |i: usize, reject: bool| ValueSource {
            arg: Some(i),
            require_literal: Some(true),
            reject_contains: if reject { Some(vec!["://".to_string()]) } else { None },
            normalize: Some(vec![NormalizeStep::LeadingSlash]),
            ..Default::default()
        };

        assert_eq!(
            ev.string(&src(0, true)),
            None,
            "a URL with a scheme names a third-party endpoint, not one this repository declares"
        );
        assert_eq!(ev.string(&src(0, false)), Some("/https://api.paypal.com/v2/checkout/orders".to_string()));
        assert_eq!(ev.string(&src(1, true)), Some("/api/delete".to_string()), "a same-origin path is kept");
    }

    #[test]
    fn string_resolves_names_and_honours_require_class() {
        let mut ws = ws();
        add_node(&mut ws, "Class", "User", Some("app\\model\\User"));
        let rec = call(vec![
            FactValue::ClassConst("app\\model\\User".into()),
            FactValue::String("User".into()),
            FactValue::String("app\\Nope".into()),
        ]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let src = |i: usize, resolve: Option<ResolveAs>, require_class: Option<bool>| ValueSource {
            arg: Some(i),
            resolve,
            require_class,
            ..Default::default()
        };
        let cc = Some(ResolveAs::ClassConst);

        // Already an FQN: kept as-is. A short name is expanded through the short-name index.
        assert_eq!(
            ev.string(&src(0, cc, None)),
            Some("app\\model\\User".to_string())
        );
        assert_eq!(
            ev.string(&src(1, cc, None)),
            Some("app\\model\\User".to_string())
        );
        // Unknown without `require_class`: passed through (the link degrades to a PendingLink).
        assert_eq!(ev.string(&src(2, cc, None)), Some("app\\Nope".to_string()));
        // With `require_class` the very same value counts as unavailable, so `value_fallback` runs.
        assert_eq!(ev.string(&src(2, cc, Some(true))), None);
        // `method_ref` / `by_alias` deliberately leave the string untouched.
        assert_eq!(
            ev.string(&src(1, Some(ResolveAs::MethodRef), None)),
            Some("User".to_string())
        );
        assert_eq!(
            ev.string(&src(1, Some(ResolveAs::ByAlias), None)),
            Some("User".to_string())
        );
        // `as_is` still resolves, but never applies the `require_class` gate.
        assert_eq!(
            ev.string(&src(1, Some(ResolveAs::AsIs), None)),
            Some("app\\model\\User".to_string())
        );
        assert_eq!(
            ev.string(&src(2, Some(ResolveAs::AsIs), Some(true))),
            Some("app\\Nope".to_string())
        );
    }

    /// The documented order: resolve -> transform -> normalize.
    #[test]
    fn string_applies_resolve_then_transform_then_normalize() {
        let ws = ws();
        let rec = call(vec![FactValue::String("app\\model\\UserModel".into())]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let src = ValueSource {
            arg: Some(0),
            transform: Some(TransformSpec {
                strip_namespace: Some(true),
                lower: Some(true),
                ..Default::default()
            }),
            normalize: Some(vec![NormalizeStep::Replace {
                from: "model".to_string(),
                to: "tbl".to_string(),
            }]),
            ..Default::default()
        };
        // strip_namespace -> "UserModel", lower -> "usermodel", then normalize rewrites that result.
        assert_eq!(ev.string(&src), Some("usertbl".to_string()));
    }

    // ---------------------------------------------------------------- `list()` / `resolve_name()`

    #[test]
    fn list_returns_array_values_or_a_single_item() {
        let ws = ws();
        let c = cfg(FactValue::Array(vec![
            ("0".into(), FactValue::String("a".into())),
            ("1".into(), FactValue::String("b".into())),
        ]));
        let ev = Evaluator::new(&ws, MatchCtx::Config(&c));
        let many = ValueSource {
            array_values: Some(true),
            ..Default::default()
        };
        assert_eq!(
            ev.list(&many),
            vec![
                FactValue::String("a".to_string()),
                FactValue::String("b".to_string())
            ]
        );

        // A scalar entry yields an **empty** list, not a single-element one.
        let scalar = cfg(FactValue::String("x".into()));
        let ev2 = Evaluator::new(&ws, MatchCtx::Config(&scalar));
        assert!(ev2.list(&many).is_empty());
        // A non-list source degrades to exactly one item.
        assert_eq!(
            ev2.list(&ValueSource {
                literal: Some("only".into()),
                ..Default::default()
            }),
            vec![FactValue::String("only".to_string())]
        );
        assert!(ev2.list(&ValueSource::default()).is_empty());
    }

    #[test]
    fn resolve_name_prefers_the_exact_fqn_then_the_short_name() {
        let mut ws = ws();
        add_node(&mut ws, "Class", "User", Some("app\\model\\User"));
        let rec = call(vec![]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));

        assert_eq!(ev.resolve_name("app\\model\\User"), "app\\model\\User");
        // A leading `\` (global-namespace form) is dropped before the lookup.
        assert_eq!(ev.resolve_name("\\app\\model\\User"), "app\\model\\User");
        assert_eq!(ev.resolve_name("User"), "app\\model\\User");
        // Unknown: passed through unchanged, never invented.
        assert_eq!(ev.resolve_name("app\\Nope"), "app\\Nope");
    }

    // ---------------------------------------------------------------- call context

    #[test]
    fn call_ctx_reads_arguments_by_index_element_and_field() {
        let ws = ws();
        let rec = call(vec![
            FactValue::String("/users".into()),
            FactValue::Array(vec![
                ("0".into(), FactValue::ClassConst("app\\Ctrl\\User".into())),
                ("1".into(), FactValue::String("index".into())),
            ]),
            FactValue::Array(vec![("url".into(), FactValue::String("/api".into()))]),
        ]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));

        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(0),
                ..Default::default()
            }),
            Some("/users".to_string())
        );
        // `[Ctrl::class, 'method']` -> element 0 / 1.
        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(1),
                element: Some(0),
                ..Default::default()
            }),
            Some("app\\Ctrl\\User".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(1),
                element: Some(1),
                ..Default::default()
            }),
            Some("index".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(2),
                field: Some("url".into()),
                ..Default::default()
            }),
            Some("/api".to_string())
        );
        // Out of range -> unavailable.
        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(9),
                ..Default::default()
            }),
            None
        );
        // `field` / `element` on a non-array argument is unavailable — not the whole value.
        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(0),
                field: Some("url".into()),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(0),
                element: Some(0),
                ..Default::default()
            }),
            None
        );
    }

    /// `owner_class` / `owner_member` split on the member separator found in the FQN (`::` for PHP, `.` for
    /// Java / JS) — the kernel still assumes no specific language.
    #[test]
    fn call_ctx_extracts_owner_class_and_member() {
        let ws = ws();
        let rec = call_with("app\\services\\Order::create", None, None, None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                owner_class: Some(true),
                ..Default::default()
            }),
            Some("app\\services\\Order".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                owner_member: Some(true),
                ..Default::default()
            }),
            Some("create".to_string())
        );

        let rec = call_with("com.example.Ctrl.list", None, None, None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                owner_class: Some(true),
                ..Default::default()
            }),
            Some("com.example.Ctrl".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                owner_member: Some(true),
                ..Default::default()
            }),
            Some("list".to_string())
        );

        // The parser's explicit `owner_class` wins over splitting the FQN.
        let rec = call_with(
            "app\\services\\Order::create",
            Some("app\\services\\Explicit"),
            None,
            None,
        );
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                owner_class: Some(true),
                ..Default::default()
            }),
            Some("app\\services\\Explicit".to_string())
        );

        // No owner at all -> unavailable, so `value_fallback` can cover it.
        let rec = call_with("", None, None, None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                owner_class: Some(true),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            ev.string(&ValueSource {
                owner_member: Some(true),
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn call_ctx_resolves_the_receiver_through_the_import_alias() {
        let mut ws = ws();
        ws.put_symbol(
            ProjectId(1),
            "imports",
            "queuethink",
            serde_json::json!({ "fqn": "think\\facade\\Queue" }),
        );
        let rec = call_with("app\\Svc::run", None, Some("QueueThink"), None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                receiver_class: Some(true),
                ..Default::default()
            }),
            Some("think\\facade\\Queue".to_string())
        );

        // Without an alias the receiver is used as written.
        let rec = call_with("app\\Svc::run", None, Some("Queue"), None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                receiver_class: Some(true),
                ..Default::default()
            }),
            Some("Queue".to_string())
        );

        // No receiver (a free function) -> unavailable.
        let rec = call_with("app\\Svc::run", None, None, None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                receiver_class: Some(true),
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn call_ctx_reads_method_name_and_entity() {
        let ws = ws();
        let rec = call_with("app\\Svc::run", None, Some("Db"), Some("OrderPlacedEvent"));
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                method_name: Some(true),
                ..Default::default()
            }),
            Some("name".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                entity: Some(true),
                ..Default::default()
            }),
            Some("OrderPlacedEvent".to_string())
        );

        // An absent entity is "unavailable", which is what lets `value_fallback` cover it.
        let rec = call_with("app\\Svc::run", None, Some("Db"), None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                entity: Some(true),
                ..Default::default()
            }),
            None
        );
    }

    // ---------------------------------------------------------------- config / inherit / node contexts

    #[test]
    fn config_ctx_reads_the_entry_forms() {
        let ws = ws();
        let c = cfg(FactValue::Array(vec![
            ("0".into(), FactValue::String("a".into())),
            ("1".into(), FactValue::String("b".into())),
        ]));
        let ev = Evaluator::new(&ws, MatchCtx::Config(&c));

        assert_eq!(
            ev.string(&ValueSource {
                key_path: Some(true),
                ..Default::default()
            }),
            Some("user.greeting.title".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                array_length: Some(true),
                ..Default::default()
            }),
            Some("2".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                locale: Some(true),
                ..Default::default()
            }),
            Some("zh-cn".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                file_stem: Some(true),
                ..Default::default()
            }),
            Some("zh-cn".to_string())
        );
        // `path_segment` skips the first n segments of the key path.
        assert_eq!(
            ev.string(&ValueSource {
                path_segment: Some(1),
                ..Default::default()
            }),
            Some("greeting.title".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                path_segment: Some(9),
                ..Default::default()
            }),
            None
        );

        // A record without a locale yields "", i.e. "empty" — which `default` (and only `default`) covers.
        let bare = ConfigRecord {
            locale: None,
            file_stem: None,
            ..cfg(FactValue::Null)
        };
        let ev2 = Evaluator::new(&ws, MatchCtx::Config(&bare));
        assert_eq!(
            ev2.string(&ValueSource {
                locale: Some(true),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            ev2.string(&ValueSource {
                locale: Some(true),
                default: Some("und".into()),
                ..Default::default()
            }),
            Some("und".to_string())
        );
    }

    #[test]
    fn inherit_and_node_ctx_read_self_value_and_property() {
        let mut ws = ws();
        let cls = add_node(&mut ws, "Class", "User", Some("app\\Model\\User"));
        ws.record_property(cls, "table", FactValue::String("eb_user".into()));
        let i = inherit();
        ws.record_property(i.child, "connection", FactValue::String("main".into()));

        let ev = Evaluator::new(&ws, MatchCtx::Inherit(&i));
        assert_eq!(
            ev.string(&ValueSource {
                self_value: Some(true),
                ..Default::default()
            }),
            Some("app\\Model\\User".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                property: Some("connection".into()),
                ..Default::default()
            }),
            Some("main".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                property: Some("nope".into()),
                ..Default::default()
            }),
            None
        );

        let ev = Evaluator::new(&ws, MatchCtx::Node(cls));
        assert_eq!(
            ev.string(&ValueSource {
                self_value: Some(true),
                ..Default::default()
            }),
            Some("app\\Model\\User".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                property: Some("table".into()),
                ..Default::default()
            }),
            Some("eb_user".to_string())
        );
        // A synthesised node has no FQN: the name stands in.
        let synth = add_node(&mut ws, "Table", "eb_user", None);
        let ev = Evaluator::new(&ws, MatchCtx::Node(synth));
        assert_eq!(
            ev.string(&ValueSource {
                self_value: Some(true),
                ..Default::default()
            }),
            Some("eb_user".to_string())
        );
    }

    // ---------------------------------------------------------------- composition

    #[test]
    fn path_joins_parts_and_nested_source_reads_a_field() {
        let ws = ws();
        let c = cfg(FactValue::Null);
        let ev = Evaluator::new(&ws, MatchCtx::Config(&c));
        let joined = ValueSource {
            path: Some(vec![
                ValueSource {
                    file_stem: Some(true),
                    ..Default::default()
                },
                ValueSource {
                    key_path: Some(true),
                    ..Default::default()
                },
            ]),
            join: Some(".".into()),
            ..Default::default()
        };
        assert_eq!(
            ev.string(&joined),
            Some("zh-cn.user.greeting.title".to_string())
        );

        // A part that cannot be read is skipped; only when *every* part is missing is the result unavailable.
        let partial = ValueSource {
            path: Some(vec![
                ValueSource {
                    arg: Some(0),
                    ..Default::default()
                },
                ValueSource {
                    literal: Some("x".into()),
                    ..Default::default()
                },
            ]),
            join: Some("/".into()),
            ..Default::default()
        };
        assert_eq!(ev.string(&partial), Some("x".to_string()));
        let all_missing = ValueSource {
            path: Some(vec![ValueSource {
                arg: Some(0),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(ev.string(&all_missing), None);

        // `{ source: { arg: 0 }, field: 'url' }` picks a member out of an object literal...
        let rec = call(vec![FactValue::Array(vec![(
            "url".into(),
            FactValue::String("/api".into()),
        )])]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let nested = ValueSource {
            source: Some(Box::new(ValueSource {
                arg: Some(0),
                ..Default::default()
            })),
            field: Some("url".into()),
            ..Default::default()
        };
        assert_eq!(ev.string(&nested), Some("/api".to_string()));

        // ...but on a non-array this nested form yields the **whole** value, while the flat `arg` + `field`
        // form reports unavailable. Pinned as a known divergence between the two branches.
        let plain = call(vec![FactValue::String("plain".into())]);
        let ev2 = Evaluator::new(&ws, MatchCtx::Call(&plain));
        assert_eq!(
            ev2.string(&ValueSource {
                arg: Some(0),
                field: Some("url".into()),
                ..Default::default()
            }),
            None,
            "arg + field: a non-array argument counts as unobtainable"
        );
        assert_eq!(
            ev2.string(&nested),
            Some("plain".to_string()),
            "known divergence: source + field returns the whole value when it is not an array"
        );
    }

    /// The expansion variant is what makes one call site become N semantic nodes (a REST resource route).
    #[test]
    fn variant_injection_feeds_expand_method_and_entry() {
        let ws = ws();
        let rec = call(vec![]);
        let variant = ExpandVariant {
            name: "index".into(),
            method: Some("GET".into()),
            path_suffix: None,
            entry: Some("index".into()),
        };
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec)).with_variant(Some(variant));
        assert_eq!(
            ev.string(&ValueSource {
                expand_method: Some(true),
                ..Default::default()
            }),
            Some("GET".to_string())
        );
        assert_eq!(
            ev.string(&ValueSource {
                expand_entry: Some(true),
                ..Default::default()
            }),
            Some("index".to_string())
        );

        // Without a bound variant (a plain, non-expanded rule) there is nothing to inject.
        let plain = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            plain.string(&ValueSource {
                expand_method: Some(true),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            plain.string(&ValueSource {
                expand_entry: Some(true),
                ..Default::default()
            }),
            None
        );
        // A variant that leaves the field unset is unavailable too.
        let partial = ExpandVariant {
            name: "x".into(),
            method: None,
            path_suffix: None,
            entry: None,
        };
        let ev2 = Evaluator::new(&ws, MatchCtx::Call(&rec)).with_variant(Some(partial));
        assert_eq!(
            ev2.string(&ValueSource {
                expand_method: Some(true),
                ..Default::default()
            }),
            None
        );
    }

    // ---- residual branches the 16 tests above leave open: the Config-context `entry_value` form (the one
    // `ValueSource` field not yet read), the two pass-through accessors `ws()` / `ctx()`, and the
    // `owner_class` empty-string guard (`!cls.is_empty()`) that falls back to splitting the FQN. ----

    /// `entry_value` hands back the whole config value as a fact (string form), distinct from the key-path /
    /// locale / array forms. The accessors `ws()` / `ctx()` expose the live evaluation context that identity
    /// computation reads (e.g. the call-site file / line).
    #[test]
    fn config_ctx_entry_value_and_evaluator_accessors() {
        let ws = ws();
        let c = cfg(FactValue::String("hello".into()));
        let ev = Evaluator::new(&ws, MatchCtx::Config(&c));

        assert_eq!(
            ev.string(&ValueSource {
                entry_value: Some(true),
                ..Default::default()
            }),
            Some("hello".to_string())
        );
        // A numeric value renders through `as_string` rather than being dropped.
        let num = ConfigRecord {
            value: FactValue::Int(42),
            ..cfg(FactValue::Null)
        };
        assert_eq!(
            Evaluator::new(&ws, MatchCtx::Config(&num))
                .string(&ValueSource {
                    entry_value: Some(true),
                    ..Default::default()
                }),
            Some("42".to_string())
        );

        // The pass-through accessors return the same live references.
        assert!(std::ptr::eq(ev.ws(), &ws));
        match ev.ctx() {
            MatchCtx::Config(c2) => assert!(std::ptr::eq(c2, &c)),
            _ => panic!("ctx() must pass the Config variant back"),
        }
    }

    /// When the parser supplies an *empty* `owner_class` string, the guard still falls back to splitting the
    /// FQN (otherwise the empty string would slip through as the identity and then be "unavailable").
    #[test]
    fn call_ctx_owner_class_empty_string_falls_back_to_fqn() {
        let ws = ws();
        let rec = call_with("app\\Svc::run", Some(""), None, None);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                owner_class: Some(true),
                ..Default::default()
            }),
            Some("app\\Svc".to_string())
        );
    }

    // ===== residual defensive / non-obvious-default branches =====

    /// `field` looks an argument / config array up by key. PHP literals quote their keys (`'url'`), so the
    /// lookup must tolerate the surrounding single quotes rather than demanding an exact string match — the
    /// `k.trim_matches('\'') == field` arms at lines 191 and 210. Pinned so the quote-stripping cannot be
    /// "simplified" into a bare `k == field || k.trim_matches('\'') == field`, which would silently break PHP config / argument key matching.
    #[test]
    fn value_source_field_matches_quote_wrapped_keys() {
        let ws = ws();
        // `arg` form: the array key is quoted `'url'`.
        let rec = call(vec![FactValue::Array(vec![(
            "'url'".into(),
            FactValue::String("/api".into()),
        )])]);
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        assert_eq!(
            ev.string(&ValueSource {
                arg: Some(0),
                field: Some("url".into()),
                ..Default::default()
            }),
            Some("/api".to_string()),
            "the quoted key `'url'` must still match field `url` (arg form)"
        );
        // The nested `{ source: { arg: 0 }, field }` form has the identical quote tolerance.
        let nested = ValueSource {
            source: Some(Box::new(ValueSource {
                arg: Some(0),
                ..Default::default()
            })),
            field: Some("url".into()),
            ..Default::default()
        };
        assert_eq!(
            ev.string(&nested),
            Some("/api".to_string()),
            "the quoted key `'url'` must still match field `url` (nested source form)"
        );
    }

    /// When `path` lists several parts but `join` is omitted, the parts are concatenated with an **empty**
    /// separator — the `unwrap_or_default` on the `join` field (line 173). A rule author who forgets `join`
    /// gets a bare concatenation, not a panic or a space. Non-obvious default; pinned so it cannot change
    /// without a test failing.
    #[test]
    fn path_join_defaults_to_empty_when_omitted() {
        let ws = ws();
        let c = cfg(FactValue::Null);
        let ev = Evaluator::new(&ws, MatchCtx::Config(&c));
        let joined = ValueSource {
            path: Some(vec![
                ValueSource { file_stem: Some(true), ..Default::default() },
                ValueSource { key_path: Some(true), ..Default::default() },
            ]),
            // `join` intentionally omitted.
            ..Default::default()
        };
        assert_eq!(
            ev.string(&joined),
            Some("zh-cnuser.greeting.title".to_string()),
            "missing `join` concatenates parts with an empty separator"
        );
    }
}
