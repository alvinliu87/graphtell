//! MyBatis mapper XML → 伪调用点（喂给 P5 通用规则管道）。
//!
//! 原生 MyBatis（无 MyBatis-Plus `@TableName` / JPA `@Table` 注解）的表访问全部写在
//! `resources/mapper/*.xml` 里：`<select id="findCarouselList">… from tb_x …</select>`。
//! 这些文件不是源码、语言解析器不碰它们 —— newbee-mall / litemall / mall 这类工程的图里
//! 因此一条表语义都没有（表视角 0 候选、路由徽标「语义依赖 0 · 入边 0」），而
//! crmeb-java / snowy 这类 MyBatis-Plus 工程靠注解规则就有表。
//!
//! 做法：扫描 mapper XML，把每条语句 × 每张表合成**伪调用点**
//! `mybatis::select|insert|update|delete(表名)`，`owner_fqn = namespace.statementId`
//! —— 正好是 Mapper 接口方法的 FQN，节点已由 P2 建好。之后 spring-boot FKB 的两条
//! 规则即可复用通用的 Table 合成 + ReadsDb/WritesDb 边，与 PHP 侧 `DB::table()` 规则同构。
//!
//! 读表名的正则只认 `from|join|into|update + 标识符`：
//! * `from (` 子查询、`from \${tableName}` 动态表名**不匹配** —— 如实跳过，不猜；
//! * 同一语句里的多表（join）各生成一条伪调用点，去重键为 `(语句, 表)`。

use std::collections::HashSet;
use std::path::PathBuf;

use regex::Regex;
use tracing::info;

use gt_domain::model::{
    EdgeKind, FactValue, Language, NewEdge, NewNode, NodeKind, Phase, Span, SubProjectId,
};

use crate::context::PipelineContext;
use crate::workspace::CallRecord;

/// 每子工程最多扫描的 XML 数（防第三方怪库拖慢；mapper 一般 ≤ 100）。
const MAX_XML_PER_SUB: usize = 400;

pub fn run(ctx: &mut PipelineContext) {
    let subs = ctx.sub_projects.clone();
    let project_root = ctx.project.root_path.clone();
    let phase = Phase(Phase::CF_AST.to_string());

    // rust 的 regex 不支持反向引用：闭合标签用通用名捕获、事后与开标签比对。
    let stmt_re = Regex::new(
        r#"(?is)<\s*(select|insert|update|delete)\b([^>]*)>(.*?)</\s*([a-zA-Z]+)\s*>"#,
    )
    .expect("stmt regex");
    let id_re = Regex::new(r#"\bid\s*=\s*"([^"]+)""#).expect("id regex");
    let ns_re = Regex::new(r#"<mapper\b[^>]*namespace\s*=\s*"([^"]+)""#).expect("ns regex");
    let table_re =
        Regex::new(r#"(?i)\b(?:from|join|into|update)\s+`?([a-zA-Z_][\w]*)`?"#).expect("table regex");

    let mut total_tables: HashSet<String> = HashSet::new();
    let mut total_calls = 0usize;

    for sub in &subs {
        // 只对 Java 子工程做 —— 伪调用点的 callee 带语言无关前缀，但扫描本身没必要
        // 在 PHP / 前端子工程上白跑一遍。
        if sub.language.as_str() != Language::JAVA {
            continue;
        }
        let root: PathBuf = if sub.root_path.is_absolute() {
            sub.root_path.clone()
        } else {
            project_root.join(&sub.root_path)
        };

        // 收集本子工程的 mapper XML（内容含 `<mapper` 才算，文件名不做假设）。
        let mut xmls: Vec<(String, String)> = Vec::new();
        let mut walker = walkdir::WalkDir::new(&root).follow_links(false).into_iter();
        while let Some(entry) = walker.next() {
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if matches!(
                    name.as_str(),
                    "vendor" | "node_modules" | "target" | ".git" | "dist" | "build" | "runtime"
                ) {
                    walker.skip_current_dir();
                }
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "xml" {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            if !text.contains("<mapper") {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            xmls.push((rel, text));
            if xmls.len() >= MAX_XML_PER_SUB {
                break;
            }
        }

        // 同一语句多表（join）各一条；`语句×表` 去重。
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for (rel, text) in &xmls {
            let Some(ns_cap) = ns_re.captures(text) else {
                continue;
            };
            let ns = ns_cap[1].trim().to_string();
            if ns.is_empty() {
                continue;
            }
            for stmt in stmt_re.captures_iter(text) {
                let kind = stmt[1].to_ascii_lowercase();
                // 闭合标签与开标签不一致（嵌套同名块等怪异写法）——跳过，不猜。
                if !stmt[4].eq_ignore_ascii_case(&stmt[1]) {
                    continue;
                }
                let Some(id_cap) = id_re.captures(&stmt[2]) else {
                    continue;
                };
                let stmt_id = id_cap[1].trim().to_string();
                if stmt_id.is_empty() {
                    continue;
                }
                let owner_fqn = format!("{}.{}", ns, stmt_id);
                // owner 解析：优先 Mapper 接口方法（P2 已建）；接口本身兜底。
                let owner = ctx
                    .ws
                    .find_by_name(&owner_fqn)
                    .or_else(|| ctx.ws.find_by_name(&ns));
                let Some(owner) = owner else {
                    continue;
                };
                // 语句起始行（UI 的「调用处」定位用）。
                let line = text[..stmt.get(0).map(|m| m.start()).unwrap_or(0)]
                    .bytes()
                    .filter(|&b| b == b'\n')
                    .count() as u32
                    + 1;
                let span = Span {
                    start_line: line,
                    end_line: line,
                    start_byte: 0,
                    end_byte: 0,
                };
                let callee = format!("mybatis::{}", kind);
                for table_cap in table_re.captures_iter(&stmt[3]) {
                    let table = table_cap[1].to_string();
                    if !seen.insert((owner_fqn.clone(), table.clone())) {
                        continue;
                    }
                    let call_node = ctx.ws.add_node(NewNode {
                        id: None,
                        project_id: ctx.project.id,
                        sub_project_id: Some(sub.id),
                        kind: NodeKind(NodeKind::CALL_SITE.to_string()),
                        name: callee.clone(),
                        fqn: Some(format!("{}#{}:{}", owner_fqn, callee, line)),
                        identity: None,
                        file_id: None,
                        span,
                        language: Language(Language::JAVA.to_string()),
                        phase: phase.clone(),
                        confidence: 0.9,
                        // 证据：mapper 文件与语句 id，供 UI 一眼核对。
                        properties: serde_json::json!({
                            "snippet": format!("<{} id=\"{}\"> … {} …", kind, stmt_id, table),
                            "mapper": rel,
                        }),
                    });
                    ctx.ws.add_edge(NewEdge {
                        project_id: ctx.project.id,
                        kind: EdgeKind(EdgeKind::HAS_CALL_SITE.to_string()),
                        from_id: owner,
                        to_id: call_node,
                        phase: phase.clone(),
                        confidence: 1.0,
                        properties: serde_json::Value::Null,
                    });
                    ctx.ws.calls.push(CallRecord {
                        node: call_node,
                        owner,
                        owner_fqn: owner_fqn.clone(),
                        owner_class: Some(ns.clone()),
                        callee: callee.clone(),
                        // 匹配器把 `A::b` 模式拆成 receiver + method 来比（见
                        // `callee_matches`），伪调用点必须如实带上这两个字段。
                        receiver: Some("mybatis".to_string()),
                        method: Some(kind.clone()),
                        args: vec![FactValue::String(table.clone())],
                        db_table: None,
                        // MyBatis 的伪调用点来自 XML，没有"循环体内"这个概念。
                        in_loop: false,
                        span,
                        file: rel.clone(),
                        sub: Some(SubProjectId(sub.id.get())),
                        language: Language(Language::JAVA.to_string()),
                    });
                    total_tables.insert(table);
                    total_calls += 1;
                }
            }
        }
    }

    if total_calls > 0 {
        info!(
            "MyBatis mapper 扫描：注入 {} 条伪调用点，涉及 {} 张表",
            total_calls,
            total_tables.len()
        );
    }
}
