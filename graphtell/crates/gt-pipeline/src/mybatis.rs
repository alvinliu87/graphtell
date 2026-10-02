//! MyBatis mapper XML -> pseudo call sites (fed into the P5 generic rule pipeline).
//!
//! In native MyBatis (no MyBatis-Plus `@TableName` / no JPA `@Table` annotation) every table access is written in
//! `resources/mapper/*.xml`: `<select id="findCarouselList">… from tb_x …</select>`. Those files are not source
//! code and the language parser never touches them — so the graphs of projects like newbee-mall / litemall / mall
//! contain not one piece of table semantics (the table perspective has 0 candidates, the route badge reads
//! "semantic dependencies 0 · in-edges 0"), while MyBatis-Plus projects like crmeb-java / snowy do get tables from
//! the annotation rules.
//!
//! Approach: scan the mapper XMLs and synthesise, for each statement x each table, a **pseudo call site**
//! `mybatis::select|insert|update|delete(table name)` with `owner_fqn = namespace.statementId` — exactly the FQN of
//! the Mapper interface method, whose node P2 has already built. After that, two rules in the spring-boot FKB
//! reuse the generic Table synthesis + ReadsDb/WritesDb edges, isomorphic to the PHP side's `DB::table()` rule.
//!
//! The regex that reads table names only recognises `from|join|into|update + identifier`:
//! * `from (` sub-queries and `from ${tableName}` dynamic table names **do not match** — skipped faithfully, not
//!   guessed;
//! * several tables in one statement (a join) each produce a pseudo call site; the dedup key is
//!   `(statement, table)`.

use std::collections::HashSet;
use std::path::PathBuf;

use regex::Regex;
use tracing::info;

use gt_domain::model::{
    EdgeKind, FactValue, Language, NewEdge, NewNode, NodeKind, Phase, Span, SubProjectId,
};

use crate::context::PipelineContext;
use crate::workspace::CallRecord;

/// Maximum number of XMLs scanned per sub-project (so a weird third-party library cannot slow things down; mappers are usually <= 100).
const MAX_XML_PER_SUB: usize = 400;

pub fn run(ctx: &mut PipelineContext) {
    let subs = ctx.sub_projects.clone();
    let project_root = ctx.project.root_path.clone();
    let phase = Phase(Phase::CF_AST.to_string());

    // Rust's regex does not support back-references: the closing tag is captured under a generic name and compared with the opening tag afterwards.
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
        // Only for Java sub-projects — the pseudo call site's callee carries a language-agnostic prefix, but there is
        // no reason to run the scan itself pointlessly over PHP / frontend sub-projects.
        if sub.language.as_str() != Language::JAVA {
            continue;
        }
        let root: PathBuf = if sub.root_path.is_absolute() {
            sub.root_path.clone()
        } else {
            project_root.join(&sub.root_path)
        };

        // Collect this sub-project's mapper XMLs (only files whose content contains `<mapper` count; no assumption is made about file names).
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

        // Several tables in one statement (a join) each get one; deduped by `statement x table`.
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
                // The closing tag does not match the opening tag (nested same-named blocks and other oddities) — skip, do not guess.
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
                // Owner resolution: prefer the Mapper interface method (already built by P2); the interface itself is the fallback.
                let owner = ctx
                    .ws
                    .find_by_name(&owner_fqn)
                    .or_else(|| ctx.ws.find_by_name(&ns));
                let Some(owner) = owner else {
                    continue;
                };
                // The statement's starting line (used by the UI to locate the "call site").
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
                        // Evidence: the mapper file and statement id, so the UI can verify at a glance.
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
                        // The matcher splits an `A::b` pattern into receiver + method to compare (see
                        // `callee_matches`), so a pseudo call site must carry both fields faithfully.
                        receiver: Some("mybatis".to_string()),
                        method: Some(kind.clone()),
                        args: vec![FactValue::String(table.clone())],
                        db_table: None,
                        // MyBatis pseudo call sites come from XML, which has no notion of "inside a loop body".
                        in_loop: false,
                        entity: None,
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
            "MyBatis mapper scan: injected {} pseudo call sites, across {} tables",
            total_calls,
            total_tables.len()
        );
    }
}
