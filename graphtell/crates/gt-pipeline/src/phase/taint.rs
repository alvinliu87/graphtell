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
//! 以上两类恰好覆盖 CRMEB 实测的全部 SQLi 形态（`SystemAdmin::searchRolesAttr`、
//! `SystemDatabackup`、`SystemCrud::599`、`UpgradeController` 的 `$findSql`）。

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

        let (kind, counter) = if is_raw {
            ("tainted_raw", &mut raw_count)
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
        "P9 污点完成：原始SQL注入 {} 处，where 条件插值注入 {} 处",
        raw_count,
        where_count
    );
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

fn text_has_var(t: &str) -> bool {
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            match bytes.get(i + 1) {
                Some(b'{') => return true,
                Some(c) if c.is_ascii_alphabetic() || *c == b'_' => return true,
                _ => {}
            }
        }
        i += 1;
    }
    false
}

fn contains_sql_keyword(t: &str) -> bool {
    const KW: &[&str] = &[
        "SELECT", "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "TRUNCATE", "REPLACE",
        "WHERE", "FROM", "SHOW", "SET", "INTO", "VALUES", "JOIN", "ORDER", "GROUP", "HAVING",
        "LIKE", "LIMIT", "CONCAT", "AND", "OR", "NOT", "IN", "BY",
    ];
    let up = t.to_uppercase();
    KW.iter().any(|k| up.contains(k))
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
}
