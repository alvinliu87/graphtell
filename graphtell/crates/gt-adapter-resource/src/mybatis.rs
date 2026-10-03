//! MyBatis mapper XML resource adapter.
//!
//! In native MyBatis (no MyBatis-Plus `@TableName` / no JPA `@Table` annotation) every table access is written in
//! `resources/mapper/*.xml`: `<select id="findCarouselList">… from tb_x …</select>`. Those files are not source
//! code and the language parser never touches them — so the graphs of projects like newbee-mall / litemall / mall
//! contain not one piece of table semantics (the table perspective has 0 candidates), while MyBatis-Plus projects
//! like crmeb-java / snowy do get tables from the annotation rules.
//!
//! Approach: scan the mapper XMLs and synthesise, for each statement x each table, a **pseudo call site**
//! `mybatis::select|insert|update|delete(table name)` with `owner_fqn = namespace.statementId` — exactly the FQN of
//! the Mapper interface method, whose node P2 has already built. The knowledge base (`fkb/java/mybatis.yaml`) then
//! reuses the generic Table synthesis + ReadsDb / WritesDb rules, isomorphic to the PHP side's `DB::table()` rule.
//!
//! The adapter only ever **returns data**: whether the library applies is decided by that knowledge base's
//! detectors, which node each pseudo call site hangs from is resolved by the kernel (the adapter cannot see the
//! graph), and no `PipelineContext` is touched here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;

use gt_domain::error::Result;
use gt_domain::model::{FactValue, Span, SubProject};
use gt_domain::port::{FileSystem, PseudoCall, ResourceAdapter, ResourceFact};

/// The knowledge id: the kernel runs this adapter only for sub-projects where P3 recognised it.
const KNOWLEDGE_ID: &str = "mybatis";

/// Maximum number of XMLs scanned per sub-project (so a weird third-party library cannot slow things down; mappers are usually <= 100).
const MAX_XML_PER_SUB: usize = 400;

/// How much we trust a synthesised pseudo call site. Lower than a parser fact (0.9 vs 1.0) because the table name
/// comes out of a regex rather than out of the syntax tree.
const PSEUDO_CALL_CONFIDENCE: f32 = 0.9;

pub struct MyBatisMapperAdapter {
    /// Rust's regex does not support back-references: the closing tag is captured under a generic name and compared
    /// with the opening tag afterwards.
    stmt_re: Regex,
    id_re: Regex,
    ns_re: Regex,
    table_re: Regex,
}

impl MyBatisMapperAdapter {
    pub fn new() -> Self {
        Self {
            stmt_re: Regex::new(
                r#"(?is)<\s*(select|insert|update|delete)\b([^>]*)>(.*?)</\s*([a-zA-Z]+)\s*>"#,
            )
            .expect("statement regex"),
            id_re: Regex::new(r#"\bid\s*=\s*"([^"]+)""#).expect("id regex"),
            ns_re: Regex::new(r#"<mapper\b[^>]*namespace\s*=\s*"([^"]+)""#).expect("namespace regex"),
            // The trailing `"` matches an optional closing quote (`from "tb_order"`).
            table_re: Regex::new(r#"(?i)\b(?:from|join|into|update)\s+`?([a-zA-Z_][\w]*)`?"#)
                .expect("table regex"),
        }
    }
}

impl Default for MyBatisMapperAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceAdapter for MyBatisMapperAdapter {
    fn id(&self) -> &str {
        KNOWLEDGE_ID
    }

    fn scan(
        &self,
        sub: &SubProject,
        project_root: &Path,
        fs: &dyn FileSystem,
    ) -> Result<Vec<ResourceFact>> {
        let root: PathBuf = if sub.root_path.is_absolute() {
            sub.root_path.clone()
        } else {
            project_root.join(&sub.root_path)
        };
        // Several tables in one statement (a join) each get one; deduped by `statement x table`.
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut facts: Vec<ResourceFact> = Vec::new();

        for (rel, text) in collect_mappers(&root, fs) {
            let Some(ns_cap) = self.ns_re.captures(&text) else {
                continue;
            };
            let ns = ns_cap[1].trim().to_string();
            if ns.is_empty() {
                continue;
            }
            for stmt in self.stmt_re.captures_iter(&text) {
                let kind = stmt[1].to_ascii_lowercase();
                // The closing tag does not match the opening tag (nested same-named blocks and other oddities) —
                // skip, do not guess.
                if !stmt[4].eq_ignore_ascii_case(&stmt[1]) {
                    continue;
                }
                let Some(id_cap) = self.id_re.captures(&stmt[2]) else {
                    continue;
                };
                let stmt_id = id_cap[1].trim().to_string();
                if stmt_id.is_empty() {
                    continue;
                }
                let owner_fqn = format!("{}.{}", ns, stmt_id);
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
                for table_cap in self.table_re.captures_iter(&stmt[3]) {
                    let table = table_cap[1].to_string();
                    if !seen.insert((owner_fqn.clone(), table.clone())) {
                        continue;
                    }
                    facts.push(ResourceFact::PseudoCall(PseudoCall {
                        owner_fqn: owner_fqn.clone(),
                        // Fallback for the owner lookup: the Mapper interface itself.
                        owner_class: Some(ns.clone()),
                        callee: callee.clone(),
                        // The matcher splits an `A::b` pattern into receiver + method to compare (see
                        // `callee_matches`), so a pseudo call site must carry both fields faithfully.
                        receiver: Some("mybatis".to_string()),
                        method: Some(kind.clone()),
                        args: vec![FactValue::String(table.clone())],
                        span,
                        file: rel.clone(),
                        snippet: format!("<{} id=\"{}\"> … {} …", kind, stmt_id, table),
                        // Evidence: the mapper file, so the UI can verify at a glance.
                        props: serde_json::json!({ "mapper": rel }),
                        confidence: PSEUDO_CALL_CONFIDENCE,
                    }));
                }
            }
        }

        Ok(facts)
    }
}

/// Collect this sub-project's mapper XMLs: every `.xml` whose **content** contains `<mapper` — no assumption is
/// made about file names or directory layout.
fn collect_mappers(root: &Path, fs: &dyn FileSystem) -> Vec<(String, String)> {
    let mut xmls: Vec<(String, String)> = Vec::new();
    let mut walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
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
        let Ok(text) = fs.read_to_string(entry.path()) else {
            continue;
        };
        if !text.contains("<mapper") {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        xmls.push((rel, text));
        if xmls.len() >= MAX_XML_PER_SUB {
            break;
        }
    }
    xmls
}
