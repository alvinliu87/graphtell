//! P9 Taint：在**调用点**层面检测"用户数据流入 SQL 字符串"的注入（SQL 注入）。
//!
//! # 动机
//!
//! FKB 已在 P4 给 `Db::query` / `whereRaw` / `Db::raw` 等调用点打了 `Taint(sink)` 标注，
//! 但那只是"存在危险 sink"的事实，并不判断入参是否真的来自用户输入 —— 于是
//! `raw-sql-sink` 只能发 `warning`、措辞是"需确认"，制造不出阻断级信号。
//!
//! 真正的注入只需要一个**局部**事实：危险 SQL 调用的 SQL 字符串参数里，直接拼进了
//! 一个来自请求/外部的变量。这个事实在解析期就已经保留下来了 —— `parser` 把带插值的
//! 字符串存成 `FactValue::Unknown(Some(原始文本))`（`encapsed_string` 的插值原文），
//! 把字符串拼接存成 `Unknown(Some(整段拼接文本))`。因此我们**不需要跨过程污点分析**，
//! 仅在调用点读参数文本即可判定，且框架无关（只认 callee 名 + 参数插值）。
//!
//! # 覆盖的注入点形态
//!
//! * 原始 SQL 执行：`Db::query("...{$t}...")` / `Db::execute(...)` / `Db::raw(...)` /
//!   `->whereRaw("...$v...")` / `->whereExp(...)` —— 第 0 参是 SQL 文本，拼接变量即注入。
//!   裸变量（如 `Db::query($sql)`，变量在别处拼接）同样判高危。
//! * where 条件插值：`->where("CONCAT(',',roles,',') LIKE '%,$roles,%'")` 这类把变量
//!   直接塞进条件字符串的写法 —— 第 0 参是字符串且含插值即注入（裸变量留给 Tier-2）。
//!
//! # P10 反向追踪：变量到底是不是来自请求
//!
//! 只看调用点参数文本（"里面有没有 `$var`"）会大量误报：`Db::execute($execSql)`
//! 这种整段变量传入的写法一律命中，而 `$execSql` 其实来自随版本包发布的本地文件
//! （实测 CRMEB 的 `sql-injection-raw` 40 条里 34 条是这类）。
//!
//! 因此这里在**同一函数内**沿赋值链反向追踪：变量 → 它的赋值右侧 → 右侧引用的
//! 其它变量 → ……，只要链上任何一环出现请求源（`request()->param()` / `input()` /
//! `$_GET` 等）才判定为污点。作用域限制在函数内，不做跨过程（Tier-2 的活）。
//!
//! 追不到来源（例如变量是函数参数）按"未见请求源"处理 —— 宁可漏报也不制造
//! 无法解释的 critical；漏报会由 `rules_silent`（跑了但 0 命中）显式暴露出来。

use std::collections::{HashMap, HashSet};

use gt_domain::model::{
    AnnotationChannel, FactValue, Language, MergeStrategy, NewAnnotation, NodeId, Phase,
};
use serde_json::json;

use crate::context::PipelineContext;

/// 原始 SQL 执行类 callee（第 0 参即 SQL 文本）。
fn is_raw_sql_sink(receiver: &Option<String>, method: &str) -> bool {
    match method {
        // `Db::query` / `Db::execute` 必须是 Db 静态调用。
        "query" | "execute" => receiver.as_deref() == Some("Db"),
        // `whereRaw` / `whereExp` / `Db::raw` 任意接收者都是原始 SQL。
        "whereRaw" | "whereExp" | "raw" => true,
        _ => false,
    }
}

/// where 系列里"条件字符串插值"属于注入的形态（whereLike 走参数化转义，跳过）。
fn is_where_interp_sink(method: &str) -> bool {
    matches!(method, "where" | "whereOr")
}

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Taint".to_string());

    let mut raw_count = 0usize;
    let mut where_count = 0usize;
    // 证实来自请求的（critical）之外的"来源不明"（warning）计数。
    let mut raw_unknown_count = 0usize;
    let mut where_unknown_count = 0usize;

    // 赋值索引：(函数 FQN, 变量名) → 该变量在本函数内的所有赋值右侧源码。
    // 与 `ctx.ws.calls` 一样先建好，避免循环里的借用冲突。
    let mut index: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for a in &ctx.ws.variable_assignments {
        index
            .entry((a.owner_fqn.as_str(), a.var.as_str()))
            .or_default()
            .push(a.rhs.as_str());
    }

    // 先收集待标注的调用点，避免 `ctx.ws.calls` 的不可变借用与 `annotate` 的可变借用冲突。
    let mut targets: Vec<(NodeId, String, String, String)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        // 目前只对 PHP 做此启发式（callee 命名与字符串插值语义以 PHP 为准）。
        if call.language.0 != Language::PHP {
            continue;
        }
        let method = match &call.method {
            Some(m) => m.as_str(),
            None => continue,
        };
        let is_raw = is_raw_sql_sink(&call.receiver, method);
        let is_where = is_where_interp_sink(method);
        if !is_raw && !is_where {
            continue;
        }

        // SQL 字符串通常是第 0 个参数。
        let sql_arg = match call.args.first() {
            Some(a) => a,
            None => continue,
        };
        if !arg_has_user_var(sql_arg) {
            continue;
        }

        // where 系列只认"变量嵌入在字符串/SQL 表达式里"（裸变量如 `->where($cond)`
        // 需要 Tier-2 跨过程追参数来源，本阶段不误报）；原始 SQL 执行类连裸变量也算高危。
        if is_where && !arg_is_embedded(sql_arg) {
            continue;
        }

        // 反向追踪：参数里的变量是否真的来自请求。
        //
        // * 证实来自请求 → `tainted_raw` / `tainted_where`（critical，可阻断）；
        // * 追不到来源（变量是参数 / 跨函数）→ `*_unknown`（warning，需确认）。
        //
        // 第二类**不能静默丢弃**：实测 `SystemAdmin::searchRolesAttr($query, $roles)`
        // 里 `$roles` 就是形参，与"来自本地升级文件"的误报同形，函数内视角分不开。
        // 全部丢掉会静默漏报真实注入，而 `rules_silent` 兜不住（规则仍有命中）。
        let vars = var_names_in(&arg_text(sql_arg));
        let proven = vars
            .iter()
            .any(|v| reaches_request(&index, call.owner_fqn.as_str(), v));
        // 参数里没有可解析的变量（例如 `$this->alias . '.uid'` 之外的形态）时维持原判据。
        let unknown = !vars.is_empty() && !proven;

        let (kind, counter) = if is_raw {
            if unknown {
                ("tainted_raw_unknown", &mut raw_unknown_count)
            } else {
                ("tainted_raw", &mut raw_count)
            }
        } else if unknown {
            ("tainted_where_unknown", &mut where_unknown_count)
        } else {
            ("tainted_where", &mut where_count)
        };
        *counter += 1;
        targets.push((
            call.node,
            kind.to_string(),
            call.callee.clone(),
            arg_text(sql_arg),
        ));
    }

    for (node_id, kind, callee, sql) in targets {
        ctx.ws.annotate(NewAnnotation {
            node_id,
            channel: AnnotationChannel("Taint".to_string()),
            kind,
            subkind: Some("SqlInjection".to_string()),
            confidence: 0.9,
            evidence: json!({
                "callee": callee,
                "sql": sql,
            }),
            phase: phase.clone(),
            merge: MergeStrategy::Coexist,
        });
    }

    tracing::info!(
        "P9 污点完成：原始SQL注入 {} 处（来源不明 {} 处），where 条件插值注入 {} 处（来源不明 {} 处）",
        raw_count,
        raw_unknown_count,
        where_count,
        where_unknown_count
    );
}

/// 反向追踪的最大深度（防止 `$a = $b; $b = $a;` 这类环与超长链）。
const TRACE_DEPTH: u8 = 4;

/// 请求源特征：出现任一即认为该表达式读到了用户输入。
///
/// 名单刻意保守（只认框架/超全局的取参写法），`$request` 也计入 —— 它被注入
/// 进来时就是 Request 对象。宁可漏（由 `rules_silent` 兜住），也不要把配置
/// 读取误判成用户输入。
const REQUEST_SOURCES: &[&str] = &[
    "$_get",
    "$_post",
    "$_request",
    "$_cookie",
    "$_files",
    "request()",
    "request::",
    "$request",
    "->param(",
    "->input(",
    "->get(",
    "->post(",
    "->all(",
    "->only(",
    "->except(",
    "input(",
];

/// 变量（在**同一函数内**沿赋值链）是否最终来自请求输入。
fn reaches_request(index: &HashMap<(&str, &str), Vec<&str>>, owner_fqn: &str, var: &str) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    trace_var(index, owner_fqn, var, TRACE_DEPTH, &mut seen)
}

fn trace_var(
    index: &HashMap<(&str, &str), Vec<&str>>,
    owner_fqn: &str,
    var: &str,
    depth: u8,
    seen: &mut HashSet<String>,
) -> bool {
    if depth == 0 || !seen.insert(var.to_string()) {
        return false;
    }
    let Some(rhss) = index.get(&(owner_fqn, var)) else {
        // 没有赋值记录：参数 / 全局 / 追不到 —— 按"未见请求源"处理。
        return false;
    };
    for rhs in rhss {
        let lower = rhs.to_ascii_lowercase();
        if REQUEST_SOURCES.iter().any(|s| lower.contains(s)) {
            return true;
        }
        for next in var_names_in(rhs) {
            if trace_var(index, owner_fqn, &next, depth - 1, seen) {
                return true;
            }
        }
    }
    false
}

/// 抽取文本里的变量名（`$sql` / `{$sql}`），不含 `$`，跳过 `$this`。
fn var_names_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > start {
                let name = &text[start..j];
                if name != "this" && !out.iter().any(|n| n == name) {
                    out.push(name.to_string());
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// 参数文本里是否含有"用户变量引用"（`$var` / `{$var}`）。
///
/// 仅识别明显的变量引用，避免把 `$` 出现在其它上下文（如 `Db::` 命名空间分隔、
/// `Env::get` 里的字面量）误判。
fn arg_has_user_var(fv: &FactValue) -> bool {
    match fv {
        FactValue::String(s) => s.contains("${"),
        FactValue::Unknown(Some(t)) => text_has_var(t),
        _ => false,
    }
}

/// 变量是否"嵌入在字符串 / SQL 表达式里"（而非单独成参的裸变量）。
fn arg_is_embedded(fv: &FactValue) -> bool {
    let t = match fv {
        FactValue::String(s) => s.as_str(),
        FactValue::Unknown(Some(t)) => t.as_str(),
        _ => return false,
    };
    text_has_var(t) && (t.contains('\'') || t.contains('"') || contains_sql_keyword(t))
}

/// 文本里是否含有"变量引用"（`$var` / `{$var}`）；`$this` 不算。
///
/// `$this` 是对象自身，永远不是用户输入。把它算进来会让
/// `->where($this->alias . '.uid', $uid)` 这种**参数化**写法被判成注入
/// （条件字符串里确实"有变量"，但那个变量是属性，值是绑定参数）——
/// 实测 CRMEB 上这正是 `sql-injection-where-interp` 残留 29 条的全部来源。
fn text_has_var(t: &str) -> bool {
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            if let Some(name) = var_name_at(t, i + 1) {
                if name != "this" {
                    return true;
                }
                i += 1 + name.len();
                continue;
            }
            // `{$var}` 形态。
            if bytes.get(i + 1) == Some(&b'{') {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 从 `text[start..]` 起读取一个变量名（字母 / 数字 / 下划线）。
fn var_name_at(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut j = start;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if j == start {
        return None;
    }
    text.get(start..j)
}

fn contains_sql_keyword(t: &str) -> bool {
    const KW: &[&str] = &[
        "SELECT", "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "TRUNCATE", "REPLACE",
        "WHERE", "FROM", "SHOW", "SET", "INTO", "VALUES", "JOIN", "ORDER", "GROUP", "HAVING",
        "LIKE", "LIMIT", "CONCAT", "AND", "OR", "NOT", "IN", "BY",
    ];
    // 先剥掉变量引用（`$where`、`{$table}`）：变量名只是标识符，不是 SQL 语法，
    // 否则 `$where` 会因名字里含 "where" 被误判成嵌了 WHERE 子句（BaseDao.php:580 误报根因）。
    let cleaned = strip_var_refs(t);
    let up = cleaned.to_uppercase();
    // 关键字必须整词出现（按非字母数字切词后精确比较），避免子串误命中。
    KW.iter()
        .any(|k| up.split(|c: char| !c.is_ascii_alphabetic()).any(|w| w == *k))
}

/// 去掉文本里的变量引用（`$var` / `{$var}`），替换为空格占位。
fn strip_var_refs(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            // 跳过变量名本身
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_alphanumeric() || nc == '_' {
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(' ');
        } else if c == '{' && chars.peek() == Some(&'$') {
            // `{$var}` 插值整体剥掉
            chars.next();
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_alphanumeric() || nc == '_' {
                    chars.next();
                } else {
                    break;
                }
            }
            if chars.peek() == Some(&'}') {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

fn arg_text(fv: &FactValue) -> String {
    match fv {
        FactValue::String(s) => s.clone(),
        FactValue::Unknown(Some(t)) => t.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_var_in_interpolated_string() {
        assert!(text_has_var("CONCAT(',',roles,',') LIKE '%,$roles,%'"));
        assert!(text_has_var("SHOW FULL COLUMNS FROM `{$table}` WHERE Field = '{$field}'"));
        assert!(text_has_var("\"DROP TABLE `\" . $info->table_name . \"`"));
        assert!(text_has_var("$findSql"));
    }

    #[test]
    fn ignores_namespace_and_literals() {
        assert!(!text_has_var("Db::query"));
        assert!(!text_has_var("Env::get('database.prefix')"));
        assert!(!text_has_var("status"));
    }

    #[test]
    fn embedded_recognizes_string_but_not_bare_var() {
        let embedded = FactValue::Unknown(Some(
            "CONCAT(',',roles,',') LIKE '%,$roles,%'".to_string(),
        ));
        assert!(arg_is_embedded(&embedded));
        let bare = FactValue::Unknown(Some("$cond".to_string()));
        assert!(!arg_is_embedded(&bare));
    }

    /// 回归：数组参数化写法 `->where($where)` 不得因变量名含 SQL 关键字而误判
    /// （BaseDao.php:580，`$where` 大写后含 "WHERE"）。
    #[test]
    fn bare_var_named_like_keyword_is_not_embedded() {
        for name in ["$where", "$order", "$limit", "$group", "$values", "$map"] {
            let bare = FactValue::Unknown(Some(name.to_string()));
            assert!(!arg_is_embedded(&bare), "{name} 不应判为嵌 SQL");
        }
        // 变量名剥除后，真正的 SQL 片段仍能靠关键字/引号命中
        let kw_only_in_var = FactValue::Unknown(Some("$orderBy . ' LIMIT 1'".to_string()));
        assert!(arg_is_embedded(&kw_only_in_var));
    }

    #[test]
    fn strip_var_refs_removes_vars_and_interpolation() {
        assert_eq!(strip_var_refs("$where"), " ");
        assert_eq!(strip_var_refs("{$table}"), " ");
        assert_eq!(strip_var_refs("a.$order.b"), "a. .b");
        assert_eq!(strip_var_refs("LIKE '%$kw%'"), "LIKE '% %'");
    }
}
