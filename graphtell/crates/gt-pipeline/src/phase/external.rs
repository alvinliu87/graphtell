//! P12 External：循环内的**外部系统调用**（HTTP / 短信 / 邮件 / RPC）。
//!
//! # 动机
//!
//! 一次网络往返比一次数据库查询贵一个量级。把它放进循环，等于把接口的耗时
//! 从"一次远程调用"放大成"N 次"，而且是**串行**放大 —— 比 N+1 更容易拖垮接口。
//! 典型现场：循环给用户发短信、循环查物流轨迹、循环拉取远程图片。
//!
//! 判据与 N+1 完全同构：调用点带 `in_loop` + callee 命中 FKB 的 `external_calls`。
//! 差别只在名单来源 —— 动词清单放 FKB，内核不认识任何一个名字。
//!
//! # 为什么不做「有没有重试 / 超时」
//!
//! 那需要知道调用参数与 SDK 配置，图上没有；这里只报"循环里发生了远程调用"这个
//! 确凿事实，改法由人判断（批量接口 / 合并请求 / 丢进队列）。

use gt_domain::model::{AnnotationChannel, Language, MergeStrategy, NewAnnotation, NodeId, Phase};
use serde_json::json;

use crate::context::PipelineContext;

/// 循环内外部调用标注（规则用 `has_annotation: ext-call-in-loop` 命中）。
const EXT_IN_LOOP: &str = "ext-call-in-loop";

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("External".to_string());
    let mut count = 0usize;
    let mut targets: Vec<(NodeId, String, u32, String)> = Vec::new();

    for call in ctx.ws.calls.iter() {
        if call.language.0 != Language::PHP || !call.in_loop {
            continue;
        }
        if !is_external_call(ctx, &call.callee, call.method.as_deref()) {
            continue;
        }
        targets.push((call.node, call.file.clone(), call.span.start_line, call.callee.clone()));
    }

    for (node, file, line, callee) in targets {
        ctx.ws.annotate(NewAnnotation {
            node_id: node,
            channel: AnnotationChannel("External".to_string()),
            kind: EXT_IN_LOOP.to_string(),
            subkind: Some("NetworkInLoop".to_string()),
            confidence: 0.85,
            evidence: json!({ "file": file, "line": line, "callee": callee }),
            phase: phase.clone(),
            merge: MergeStrategy::Coexist,
        });
        count += 1;
    }

    tracing::info!("P12 外部调用完成：循环内远程调用 {} 处", count);
}

/// callee 是否是外部系统调用：完整 callee（`Http::get`）或方法名（`curl_exec`）命中
/// FKB 声明的 `external_calls` 之一。
fn is_external_call(ctx: &PipelineContext, callee: &str, method: Option<&str>) -> bool {
    if ctx.external_calls.is_empty() {
        return false;
    }
    if ctx
        .external_calls
        .iter()
        .any(|p| callee.eq_ignore_ascii_case(p))
    {
        return true;
    }
    method
        .map(|m| ctx.external_calls.iter().any(|p| m.eq_ignore_ascii_case(p)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PipelineContext;
    use gt_domain::model::ProjectStatus;

    fn ctx_with(calls: Vec<&str>) -> PipelineContext {
        let mut ctx = PipelineContext::new(gt_domain::model::Project {
            id: gt_domain::model::ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.external_calls = calls.into_iter().map(|s| s.to_string()).collect();
        ctx
    }

    #[test]
    fn matches_scoped_and_plain_callees() {
        let ctx = ctx_with(vec!["curl_exec", "Http::get"]);
        assert!(is_external_call(&ctx, "curl_exec", Some("curl_exec")));
        assert!(is_external_call(&ctx, "Http::get", Some("get")));
        assert!(!is_external_call(&ctx, "Db::name", Some("name")));
    }

    #[test]
    fn empty_fkb_list_matches_nothing() {
        let ctx = ctx_with(vec![]);
        assert!(!is_external_call(&ctx, "curl_exec", Some("curl_exec")));
    }
}
