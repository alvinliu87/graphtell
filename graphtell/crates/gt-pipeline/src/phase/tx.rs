//! P13 Tx：同一方法内**跨多张表写库、但未识别到事务边界**。
//!
//! # 动机
//!
//! 一个方法里写订单又写库存（或写主表又写流水），中间任何一步失败都会留下
//! **部分成功**的脏数据。这是电商 / 后台最常见的一致性事故来源，靠 code review
//! 很难发现 —— 写操作往往分散在若干被调服务里。
//!
//! # 判据（刻意保守）
//!
//! * 同一方法对 **≥2 张不同的表**有直接 `WritesDb` 边（"直接"= P7 动词分类落下的边，
//!   不含 P8 沿调用链传播来的间接边 —— 间接边会把"调用了两个服务"也算进来，
//!   那属于调用方的事务边界，判据会失控）；
//! * 且该方法内没有任何 `tx_calls` 声明的事务标记（`transaction` / `startTrans` /
//!   `commit` …）。
//!
//! 为什么用"表数"而不是"写动词调用点数"：后者会把同一张表的 `if/else` 两分支各写一次
//! （`CartLogic::add` 的 `update` / `insert`）也算成两次写 —— 那是**互斥分支**，
//! 不存在部分成功的问题。实测口径从"≥2 次写动词"改成"≥2 张表"后，
//! likeshop 的命中从 105 降到更可信的量级，且互斥分支的误报自然消失。
//!
//! 保守方向是**宁可漏报**：事务可能开在更外层的调用方（跨过程），图上判不到，
//! 所以文案写"未识别到事务边界"，而不是"没有事务"。
//!
//! 标注落在**方法节点**上：事务是方法级的边界问题，不是某一行的问题。

use std::collections::{HashMap, HashSet};

use gt_domain::model::{
    AnnotationChannel, EdgeKind, Language, MergeStrategy, NewAnnotation, NodeId, Phase,
};
use serde_json::json;

use crate::context::PipelineContext;

/// 多表写无事务标注（规则用 `has_annotation: multi-write-without-tx` 命中）。
const MULTI_WRITE: &str = "multi-write-without-tx";

/// 触发阈值：同一方法直接写的**不同表**数量。
const MIN_TABLES: usize = 2;

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Tx".to_string());
    let mut count = 0usize;

    // ① 每个方法直接写了哪些表（只看 P7 落下的直接边）。
    let mut tables_by_owner: HashMap<i64, HashSet<i64>> = HashMap::new();
    for e in ctx.ws.edges() {
        if e.kind.as_str() != EdgeKind::WRITES_DB {
            continue;
        }
        // P8 传播边带 `via: "propagate"`；只保留直接写，避免把"调用了两个写服务"
        // 也算成"这个方法自己写了两张表"。
        if e.properties.get("via").is_some() {
            continue;
        }
        tables_by_owner
            .entry(e.from_id.get())
            .or_default()
            .insert(e.to_id.get());
    }

    // ② 每个方法：是否出现事务标记 + 代表位置（取最靠前的写，便于顺着读下去）。
    struct Meta {
        owner_fqn: String,
        file: String,
        line: u32,
        in_tx: bool,
    }
    let mut meta: HashMap<i64, Meta> = HashMap::new();
    for call in ctx.ws.calls.iter() {
        if call.language.0 != Language::PHP {
            continue;
        }
        let entry = meta.entry(call.owner.get()).or_insert_with(|| Meta {
            owner_fqn: call.owner_fqn.clone(),
            file: call.file.clone(),
            line: call.span.start_line,
            in_tx: false,
        });
        if let Some(method) = call.method.as_deref() {
            if ctx
                .tx_calls
                .iter()
                .any(|p| method.eq_ignore_ascii_case(p) || call.callee.eq_ignore_ascii_case(p))
            {
                entry.in_tx = true;
            }
        }
        if call.span.start_line < entry.line {
            entry.line = call.span.start_line;
            entry.file = call.file.clone();
        }
    }

    // ③ 命中：写 ≥2 张表且方法内没有事务标记。
    let mut targets: Vec<(NodeId, String, u32, usize, String)> = Vec::new();
    for (owner, tables) in tables_by_owner {
        if tables.len() < MIN_TABLES {
            continue;
        }
        let Some(m) = meta.get(&owner) else { continue };
        if m.in_tx {
            continue;
        }
        targets.push((
            NodeId(owner),
            m.file.clone(),
            m.line,
            tables.len(),
            m.owner_fqn.clone(),
        ));
    }

    for (node, file, line, n_tables, owner_fqn) in targets {
        ctx.ws.annotate(NewAnnotation {
            node_id: node,
            channel: AnnotationChannel("Tx".to_string()),
            kind: MULTI_WRITE.to_string(),
            subkind: Some("PartialWriteRisk".to_string()),
            confidence: 0.75,
            evidence: json!({
                "file": file,
                "line": line,
                "owner_fqn": owner_fqn,
                "tables": n_tables,
            }),
            phase: phase.clone(),
            merge: MergeStrategy::Coexist,
        });
        count += 1;
    }

    tracing::info!("P13 事务完成：跨多表写但无事务边界 {} 处", count);
}

#[cfg(test)]
mod tests {
    use crate::context::PipelineContext;
    use gt_domain::model::{Project, ProjectStatus};

    fn ctx_with(tx: Vec<&str>) -> PipelineContext {
        let mut ctx = PipelineContext::new(Project {
            id: gt_domain::model::ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.tx_calls = tx.into_iter().map(|s| s.to_string()).collect();
        ctx
    }

    #[test]
    fn tx_markers_are_case_insensitive() {
        let ctx = ctx_with(vec!["startTrans", "transaction"]);
        // 判据本身在 run() 里，这里只锁住名单的大小写不敏感约定
        assert!(ctx.tx_calls.iter().any(|p| p.eq_ignore_ascii_case("STARTTRANS")));
        assert!(ctx.tx_calls.iter().any(|p| p.eq_ignore_ascii_case("Transaction")));
        assert!(!ctx.tx_calls.iter().any(|p| p.eq_ignore_ascii_case("save")));
    }
}
