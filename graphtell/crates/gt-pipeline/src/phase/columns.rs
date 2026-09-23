//! 把**权威 schema 的列**沉淀成 `Column` 图节点。
//!
//! 列信息此前只活在符号表里（`schema` 表，P3 从 SQL 安装脚本 / 表名调用点装载），
//! 只能被 `ColumnsMatch` 谓词"问一句"（如 CRMEB 给含手机号的表打 PII），
//! 在图上**既不可见也不可遍历** —— 影响面分析走到表就断了，下不到字段级。
//!
//! 这里把它物化到图上：`Table --HasColumn--> Column`。
//! 与 TypeORM `@Column` 建的列**同形**（`Column` 节点 + `HasColumn` 边），只是来源不同：
//! 那边来自 ORM 的字段声明，这边来自 DDL 这份权威结构。
//!
//! 语言 / 框架无关：只看「`Table` 节点 + schema 符号表里有没有它的列」，
//! 因此任何有 SQL 安装脚本的工程（PHP / 其它）都自动获得字段级图节点。
//! 没有 schema 的表（如只有 ORM 声明、无 DDL）不产出列 —— 宁可缺不可猜。

use gt_domain::model::{
    EdgeKind, IdentityKey, Language, NewEdge, NodeId, NodeKind, Phase, Span, SubProjectId,
};
use serde_json::json;

use crate::context::PipelineContext;
use crate::engine::schema_columns;
use crate::workspace::synthesized_node;

/// 沉淀列的阶段归属：挂在 P6（此时 P5 的 `Table` 节点与 P3 的 schema 都已就位）。
pub fn materialize(ctx: &mut PipelineContext) {
    let phase = Phase(Phase::ANNOTATE_POST.to_string());

    // 先收集「表 → 列」再落图：`add_edge` / `get_or_create_synthesized` 都要 &mut ws，
    // 不能一边遍历节点一边改图。
    let mut plan: Vec<(NodeId, String, Vec<String>, Option<SubProjectId>, Language)> = Vec::new();
    for id in ctx.ws.node_ids() {
        let Some(node) = ctx.ws.node(id) else {
            continue;
        };
        if node.kind.as_str() != NodeKind::TABLE {
            continue;
        }
        // `identity.value` 是归一化后的表名（与 schema 符号表的键同源），优先用它。
        let name = node
            .identity
            .as_ref()
            .map(|i| i.value.clone())
            .unwrap_or_else(|| node.name.clone());
        if name.is_empty() {
            continue;
        }
        // 表名口径可能是**复数**：Laravel 的 DDL 写 `CREATE TABLE users`，
        // 而表节点名经 `singularize` 后是 `user` —— 直接查会落空，故试一次复数形式。
        let cols = schema_columns(&ctx.ws, "schema", &name)
            .or_else(|| schema_columns(&ctx.ws, "schema", &format!("{name}s")))
            .unwrap_or_default();
        if cols.is_empty() {
            continue;
        }
        plan.push((
            id,
            name,
            cols,
            node.sub_project_id,
            node.language.clone(),
        ));
    }

    for (table_id, tname, cols, sub, language) in plan {
        for col in cols {
            // 列身份带表名作用域：同名列（`id` / `created`）在不同表必须各算各的，
            // 否则会幂等合并成一个错节点，把两张表的影响面串起来。
            let identity = IdentityKey::named(format!("{tname}.{col}"));
            // `Column` 是由 FKB 声明的语义种类（见 `fkb/js/typeorm.yaml` 的 `semantic_kinds`），
            // 内核常量里没有它，故直接写种类字符串。
            let mut new_node = synthesized_node(
                ctx.project.id,
                "Column",
                identity,
                sub,
                &phase,
                0.9,
                &language,
                Span::default(),
            );
            new_node.properties = json!({ "category": "Column", "sources": ["schema"], "column": col });
            let (col_id, _created) = ctx.ws.get_or_create_synthesized(new_node);
            ctx.ws.add_edge(NewEdge {
                project_id: ctx.project.id,
                kind: EdgeKind("HasColumn".to_string()),
                from_id: table_id,
                to_id: col_id,
                phase: phase.clone(),
                confidence: 0.9,
                properties: serde_json::Value::Null,
            });
        }
    }
}
