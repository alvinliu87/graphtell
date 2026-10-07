//! MyBatis mapper XML resource adapter.
//!
//! In native MyBatis (no MyBatis-Plus `@TableName` / no JPA `@Table` annotation) every table access is written in
//! `resources/mapper/*.xml`: `<select id="findCarouselList">… from tb_x …</select>`. Those files are not source
//! code and the language parser never touches them — so the graphs of native MyBatis projects
//! contain not one piece of table semantics (the table perspective has 0 candidates), while MyBatis-Plus projects
//! do get tables from the annotation rules.
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

#[cfg(test)]
mod tests {
    use super::*;
    use gt_adapter_fs::StdFileSystem;
    use gt_domain::model::{Language, ProjectId, SubProjectId};

    fn sub(root: PathBuf) -> SubProject {
        SubProject {
            id: SubProjectId::new(1),
            project_id: ProjectId::new(1),
            name: "test".into(),
            root_path: root,
            language: Language::new("java"),
            role: "backend".into(),
            detected_by: "pom.xml".into(),
            frameworks: vec!["mybatis".into()],
            facts: serde_json::Value::Null,
        }
    }

    /// Write `files` under a fresh temp dir and run a real scan over it.
    fn scan_files(files: &[(&str, &str)]) -> Vec<ResourceFact> {
        let dir = std::env::temp_dir().join(format!(
            "gt_mybatis_it_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, text) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
        }
        let out = MyBatisMapperAdapter::new()
            .scan(&sub(dir.clone()), &dir, &StdFileSystem::new())
            .expect("scan should succeed");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn pseudo(facts: &[ResourceFact]) -> Vec<&PseudoCall> {
        // `ResourceFact` has exactly one variant today; matching it without a wildcard keeps this compiling
        // loudly when a second variant is added.
        facts
            .iter()
            .filter_map(|f| match f {
                ResourceFact::PseudoCall(p) => Some(p),
            })
            .collect()
    }

    /// The whole point of the adapter: a mapper XML becomes a pseudo call site hanging off the **Mapper
    /// interface method FQN** (`namespace.statementId`), carrying the table as its argument — which is what lets
    /// the generic Table / ReadsDb-WritesDb rules fire for native-MyBatis projects.
    #[test]
    fn mapper_statement_becomes_a_pseudo_call_on_the_interface_method() {
        let facts = scan_files(&[(
            "resources/mapper/CarouselMapper.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<mapper namespace="com.example.mapper.CarouselMapper">
    <select id="findCarouselList" resultType="map">
        select * from tb_carousel where id = #{id}
    </select>
</mapper>
"#,
        )]);
        let calls = pseudo(&facts);
        assert_eq!(calls.len(), 1, "expected one pseudo call, got: {calls:?}");
        let c = calls[0];
        assert_eq!(c.owner_fqn, "com.example.mapper.CarouselMapper.findCarouselList");
        assert_eq!(c.owner_class.as_deref(), Some("com.example.mapper.CarouselMapper"));
        assert_eq!(c.callee, "mybatis::select");
        // `callee_matches` splits an `A::b` pattern into receiver + method, so both must be faithful.
        assert_eq!(c.receiver.as_deref(), Some("mybatis"));
        assert_eq!(c.method.as_deref(), Some("select"));
        assert_eq!(c.args, vec![FactValue::String("tb_carousel".to_string())]);
        assert!((c.confidence - PSEUDO_CALL_CONFIDENCE).abs() < f32::EPSILON);
        assert_eq!(c.file, "resources/mapper/CarouselMapper.xml");
        assert_eq!(c.props.get("mapper").and_then(|v| v.as_str()), Some("resources/mapper/CarouselMapper.xml"));
        assert_eq!(c.span.start_line, 3, "the line number must point at the line of that statement so the UI can locate it");
    }

    /// All four statement kinds map to their verb, and a join yields one call per table.
    #[test]
    fn every_verb_and_every_table_of_a_join_is_captured() {
        let facts = scan_files(&[(
            "m.xml",
            r#"<mapper namespace="ns.M">
    <select id="q">select * from tb_a a join tb_b b on a.id = b.id</select>
    <insert id="i">insert into tb_c (x) values (1)</insert>
    <update id="u">update tb_d set x = 1</update>
    <delete id="d">delete from tb_e where id = 1</delete>
</mapper>
"#,
        )]);
        let calls = pseudo(&facts);
        let key = |c: &PseudoCall| {
            (
                c.method.clone().unwrap_or_default(),
                c.args.first().and_then(|a| match a {
                    FactValue::String(s) => Some(s.clone()),
                    _ => None,
                }).unwrap_or_default(),
            )
        };
        let mut got: Vec<(String, String)> = calls.iter().map(|c| key(c)).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("delete".to_string(), "tb_e".to_string()),
                ("insert".to_string(), "tb_c".to_string()),
                ("select".to_string(), "tb_a".to_string()),
                // The join's second table also becomes its own call site.
                ("select".to_string(), "tb_b".to_string()),
                ("update".to_string(), "tb_d".to_string()),
            ]
        );
    }

    /// Backtick-quoted identifiers are common in MySQL SQL; the backticks must not end up in the table name.
    #[test]
    fn backquoted_table_names_are_unquoted() {
        let facts = scan_files(&[(
            "m.xml",
            "<mapper namespace=\"ns.M\">\n<select id=\"q\">select * from `tb_order`</select>\n</mapper>\n",
        )]);
        let calls = pseudo(&facts);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].args, vec![FactValue::String("tb_order".to_string())]);
    }

    /// `statement x table` is deduped across the whole sub-project — the same table in the same statement must
    /// not produce two identical edges.
    #[test]
    fn duplicate_statement_table_pairs_are_deduped() {
        let facts = scan_files(&[
            (
                "a.xml",
                "<mapper namespace=\"ns.M\">\n<select id=\"q\">select * from tb_x join tb_x on 1=1</select>\n</mapper>\n",
            ),
            (
                "b.xml",
                "<mapper namespace=\"ns.M\">\n<select id=\"q\">select * from tb_x</select>\n</mapper>\n",
            ),
        ]);
        let calls = pseudo(&facts);
        assert_eq!(calls.len(), 1, "the same statement x table must appear only once: {calls:?}");
    }

    /// Malformed shapes must be skipped rather than guessed: mismatched closing tag, no `id`, no namespace.
    #[test]
    fn malformed_statements_are_skipped_not_guessed() {
        let facts = scan_files(&[
            (
                "mismatch.xml",
                "<mapper namespace=\"ns.M\">\n<select id=\"q\">select * from tb_a</insert>\n</mapper>\n",
            ),
            (
                "noid.xml",
                "<mapper namespace=\"ns.M\">\n<select>select * from tb_b</select>\n</mapper>\n",
            ),
            (
                "nons.xml",
                "<mapper>\n<select id=\"q\">select * from tb_c</select>\n</mapper>\n",
            ),
            // Not a mapper at all: never collected.
            ("other.xml", "<beans><bean id=\"x\"/></beans>"),
            ("pom.xml", "<project><modelVersion>4</modelVersion></project>"),
        ]);
        assert!(facts.is_empty(), "a malformed / non-mapper file must not produce facts: {facts:?}");
    }

    /// Dependency / build directories are pruned: a mapper vendored into `target/` must not be scanned.
    #[test]
    fn build_and_dependency_dirs_are_pruned() {
        let facts = scan_files(&[(
            "target/classes/mapper/Stale.xml",
            "<mapper namespace=\"ns.Stale\">\n<select id=\"q\">select * from tb_stale</select>\n</mapper>\n",
        )]);
        assert!(facts.is_empty(), "a mapper under target/ must not be scanned: {facts:?}");
    }

    #[test]
    fn adapter_id_is_the_knowledge_id() {
        assert_eq!(MyBatisMapperAdapter::new().id(), "mybatis");
    }

    /// A relative `root_path` must be resolved against `project_root` (the absolute case is exercised by every other
    /// test, which hands `scan` an absolute temp dir). This is the `else` branch of the `is_absolute()` check.
    #[test]
    fn relative_root_path_is_resolved_against_project_root() {
        let dir = std::env::temp_dir().join(format!("gt_mybatis_rel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mappers")).unwrap();
        std::fs::write(
            dir.join("mappers/M.xml"),
            "<mapper namespace=\"ns.M\">\n<select id=\"q\">select * from tb_a</select>\n</mapper>\n",
        )
        .unwrap();
        // `sub` sets `root_path` to whatever we pass; here it is relative.
        let s = sub("mappers".into());
        let out = MyBatisMapperAdapter::new()
            .scan(&s, &dir, &StdFileSystem::new())
            .expect("scan should succeed");
        let _ = std::fs::remove_dir_all(&dir);
        let calls = pseudo(&out);
        assert_eq!(calls.len(), 1, "relative root_path must be joined with project_root: {calls:?}");
        assert_eq!(calls[0].file, "M.xml");
    }

    /// The verb is matched case-insensitively by the regex but normalised to lowercase before becoming the callee /
    /// method — `<SELECT>` must come out as `select`, not `SELECT`.
    #[test]
    fn verb_is_normalized_to_lowercase() {
        let facts = scan_files(&[(
            "m.xml",
            "<mapper namespace=\"ns.M\">\n<SELECT id=\"q\">select * from tb_a</SELECT>\n</mapper>\n",
        )]);
        let calls = pseudo(&facts);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].callee, "mybatis::select", "the verb must be lowercased");
        assert_eq!(calls[0].method.as_deref(), Some("select"));
    }

    /// An empty `namespace=""` or empty `id=""` must be skipped (the `is_empty()` guards), not synthesised as a
    /// pseudo call with a blank owner FQN.
    #[test]
    fn empty_namespace_and_empty_id_are_skipped() {
        let facts = scan_files(&[
            (
                "emptyns.xml",
                "<mapper namespace=\"\">\n<select id=\"q\">select * from tb_a</select>\n</mapper>\n",
            ),
            (
                "emptyid.xml",
                "<mapper namespace=\"ns.M\">\n<select id=\"\">select * from tb_b</select>\n</mapper>\n",
            ),
        ]);
        assert!(
            facts.is_empty(),
            "empty namespace / empty id must not produce facts: {facts:?}"
        );
    }

    /// The safety cap must actually bound the scan: with one mapper past `MAX_XML_PER_SUB`, only `MAX_XML_PER_SUB`
    /// are read — a weird third-party library dumping thousands of mappers must not slow the scan down.
    #[test]
    fn mapper_scan_is_capped_at_max_xml_per_sub() {
        let dir = std::env::temp_dir().join(format!("gt_mybatis_cap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(MAX_XML_PER_SUB + 1) {
            std::fs::write(
                dir.join(format!("m{}.xml", i)),
                format!(
                    "<mapper namespace=\"ns.M\">\n<select id=\"q{0}\">select * from tb_{0}</select>\n</mapper>\n",
                    i
                ),
            )
            .unwrap();
        }
        let s = sub(dir.clone());
        let out = MyBatisMapperAdapter::new()
            .scan(&s, &dir, &StdFileSystem::new())
            .expect("scan should succeed");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            pseudo(&out).len(),
            MAX_XML_PER_SUB,
            "only up to MAX_XML_PER_SUB mappers must be scanned, not all of them"
        );
    }
}
