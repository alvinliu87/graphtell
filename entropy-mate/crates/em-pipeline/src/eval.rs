//! [`ValueSource`] 求值：把 YAML 里声明的"值从哪来"变成实际字符串/字面量。

use em_domain::model::{FactValue, NodeId, ResolveAs, ValueSource};

use crate::normalize::{apply_normalize, apply_table_prefix_steps, apply_transform};
use crate::workspace::{CallRecord, ConfigRecord, GraphWorkspace, InheritRecord};

/// 规则匹配时的上下文。
#[derive(Debug, Clone, Copy)]
pub enum MatchCtx<'a> {
    Call(&'a CallRecord),
    Config(&'a ConfigRecord),
    Inherit(&'a InheritRecord),
    Node(NodeId),
}

/// 求值结果。
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

/// 求值器。
pub struct Evaluator<'a> {
    ws: &'a GraphWorkspace,
    ctx: MatchCtx<'a>,
}

impl<'a> Evaluator<'a> {
    pub fn new(ws: &'a GraphWorkspace, ctx: MatchCtx<'a>) -> Self {
        Self { ws, ctx }
    }

    pub fn ws(&self) -> &'a GraphWorkspace {
        self.ws
    }

    /// 当前匹配上下文（供 identity 计算读取「调用点所在文件 / 行号」等环境信息，
    /// 例如补齐 `Route::group` 的路由组前缀）。
    pub fn ctx(&self) -> MatchCtx<'a> {
        self.ctx
    }

    /// 求值后取字符串（依次应用 resolve → transform → normalize）。
    pub fn string(&self, src: &ValueSource) -> Option<String> {
        let raw = self.raw(src)?;
        // `require_literal`：值必须来自**字面量**实参，不能是变量或表达式文本。
        // 非字面量（`Cache::get($name)`、`Cache::get(A . $b)`）被 `raw` 记成
        // `FactValue::Unknown(Some(原文))`，若直接采信就会造出 `$name` 这类垃圾身份；
        // 判否 → 返回 None → 触发 `value_fallback`（与 `require_class` 同一套语义）。
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
                    // `require_class`：解析结果必须是代码库中真实存在的类，否则整体视为
                    // 取不到（触发 `value_fallback`）。避免 `$action` 之类变量名被当类用。
                    if src.require_class == Some(true)
                        && self.ws.find_by_name(&resolved).is_none()
                        && self.ws.resolve_short_name(&resolved).is_none()
                    {
                        return None;
                    }
                    resolved
                }
                ResolveAs::AsIs => self.resolve_name(&s),
                ResolveAs::HandlerPattern | ResolveAs::ByAlias => s,
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

    /// 求值后取一组值（`array_values` —— 一对多，如事件监听器）。
    pub fn list(&self, src: &ValueSource) -> Vec<FactValue> {
        match self.raw(src) {
            Some(EvalValue::List(items)) => items,
            Some(other) => vec![other.as_fact()],
            None => Vec::new(),
        }
    }

    /// 把短名/别名解析成完全限定名（能查到就用查到的，查不到就原样返回）。
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
                    // 按下标取数组元素：`[Ctrl::class, 'method']` → 第 0 / 1 项。
                    if let Some(idx) = src.element {
                        if let FactValue::Array(items) = a {
                            return items.get(idx).map(|(_, v)| EvalValue::Fact(v.clone()));
                        }
                        return None;
                    }
                    return Some(EvalValue::Fact(a.clone()));
                }
                if src.owner_class == Some(true) {
                    // 优先用 parser 显式记录的所属类：方法级注解的 `owner_fqn` 是
                    // `Class.method`、类级注解的 `owner_fqn` 已是 `Class`，靠分隔符切分
                    // 会把类级注解误切成包名。PHP 侧未填此字段，退回字符串切分。
                    if let Some(cls) = &c.owner_class {
                        if !cls.is_empty() {
                            return Some(EvalValue::Str(cls.clone()));
                        }
                    }
                    if c.owner_fqn.is_empty() {
                        return None;
                    }
                    // 去掉末尾的成员部分，保留类 FQN。
                    // 成员分隔符随语言而变：PHP `::`、Java `.`
                    // （`com.example.Ctrl.list` → `com.example.Ctrl`）。
                    let class = c
                        .owner_fqn
                        .rfind("::")
                        .or_else(|| c.owner_fqn.rfind('.'))
                        .map(|idx| &c.owner_fqn[..idx])
                        .unwrap_or(&c.owner_fqn[..]);
                    return Some(EvalValue::Str(class.to_string()));
                }
                if src.receiver_class == Some(true) {
                    let recv = c.receiver.as_ref()?;
                    // 经 import 别名还原（如 `QueueThink` → `think\facade\Queue`）。
                    let fqn = self
                        .ws
                        .resolve_import_alias(recv)
                        .unwrap_or_else(|| recv.clone());
                    return Some(EvalValue::Str(fqn));
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
