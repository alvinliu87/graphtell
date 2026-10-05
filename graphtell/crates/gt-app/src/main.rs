//! GraphTell — Codebase graph analysis platform.
//!
//! ```text
//! graphtell serve                       start the HTTP service (shared by Tauri / Web UI)
//! graphtell create --name X --path /p   create a project and build its graph automatically
//! graphtell run    --project 1          rebuild the graph for an existing project
//! graphtell stats  --project 1          show graph size
//! ```

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

/// MCP(stdio) bridge: IDEs connect to the resident service through it.
mod mcp;
use gt_adapter_fkb::YamlKnowledgeBase;
use gt_app::{AppConfig, Container};
use gt_domain::model::fkb::Action;
use gt_domain::model::kinds::{EdgeKind, NodeKind};
use gt_domain::model::{NewProject, ProjectId};
use gt_domain::port::{DiagnosticSink, GraphQuery, NodeFilter, ProjectReader};

#[derive(Debug, Parser)]
#[command(name = "graphtell", version, about = "Codebase graph analysis platform")]
struct Cli {
    /// Data directory.
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// FKB directory (defaults to the built-in knowledge base).
    #[arg(long)]
    fkb_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the HTTP API service.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 5177)]
        port: u16,
        /// Built Web UI (React SPA) directory; when given, the server also hosts the frontend,
        /// and the root `/` becomes the SPA entry (otherwise `/` falls back to the embedded standalone page `/compose`).
        #[arg(long)]
        ui_dir: Option<PathBuf>,
    },
    /// Create a project (graph build starts automatically after creation).
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        description: Option<String>,
    },
    /// List all projects.
    List,
    /// Delete a project.
    Delete {
        #[arg(long)]
        project: i64,
    },
    /// Build the graph for a project.
    Run {
        #[arg(long)]
        project: i64,
    },
    /// Export all nodes and edges of a project's graph (JSON), for static demos / external analysis.
    ///
    /// Outputs `{project_id, nodes, edges, files}`: `files` maps `file_id -> path`,
    /// so consumers can locate a node back to its source file without another DB query.
    Export {
        #[arg(long)]
        project: i64,
    },
    /// Show graph size and diagnostics.
    Stats {
        #[arg(long)]
        project: i64,
    },
    /// List loaded check rules.
    Rules,
    /// Check a project for rule violations.
    Check {
        #[arg(long)]
        project: i64,
        /// Run only the specified rules (repeatable).
        #[arg(long)]
        rule: Vec<String>,
        /// Preview only, do not write to the database.
        #[arg(long)]
        dry_run: bool,
        /// Output the full report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Recall relevant code on the graph by prompt.
    ///
    /// Recall encoder: with the `model-candle` feature enabled, the real bge-m3 semantic vectors are used
    /// (needs local weights; directory set by env var `GT_BGE_MODEL`, default `models/bge-m3-safetensors`);
    /// without weights configured, or when built with `--no-default-features`, it falls back to the local hash encoder (offline, zero-dependency).
    Recall {
        #[arg(long)]
        project: i64,
        /// Prompt (natural language mixed with identifiers).
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value_t = 2)]
        hops: u32,
        /// Output only the Markdown context bundle that can be pasted straight to an LLM.
        #[arg(long)]
        markdown: bool,
        /// Also attach the full source of files involved in the hits (with `--markdown`, spares you reading the whole file).
        #[arg(long)]
        include_body: bool,
    },
    /// Validate an FKB directory (syntax + conventions), no DB, no graph build.
    Validate,
    /// Precompute and persist a project's node vectors (does not affect graph build; can run in the background, e.g. `graphtell embed --project 1 &`).
    ///
    /// After that, all recalls and restarts hit the cache instantly, without recomputing bge vectors over the whole graph. Weight directory is the same as recall:
    /// default `models/bge-m3-safetensors`, overridable via `GT_BGE_MODEL`.
    Embed {
        #[arg(long)]
        project: i64,
    },
    /// Diagnostics: output the cosine between the query and node vectors, to tell whether "Chinese query can't find the target" is a model-capability issue
    /// or a threshold / ranking issue.
    ///
    /// Example: `graphtell cosine --project 1 --query "商品库存扣减失败回滚" --names stock
    /// if the target symbol's cosine is already below threshold -> model / node-text issue; if the cosine is high enough but the target isn't recalled -> threshold / ranking issue.
    Cosine {
        #[arg(long)]
        project: i64,
        #[arg(long)]
        query: String,
        /// Only nodes whose name contains these substrings (comma-separated); if omitted, encode everything (slow on large projects)
        #[arg(long)]
        names: Option<String>,
        #[arg(long, default_value = "20")]
        top: usize,
    },
    /// Expose to the IDE as MCP(stdio): connect to the resident service over HTTP, providing `recall_code` (can carry
    /// `include_body` to return hit files' full text) / `compose_prompt` / `check_compliance` /
    /// `list_violations` / `warmup_status` (query background warmup progress) tools.
    /// stdout is the JSON-RPC channel; logs go to stderr.
    Mcp {
        /// Resident service address, default http://127.0.0.1:5177
        #[arg(long, default_value = "http://127.0.0.1:5177")]
        base_url: String,
        /// Target project id (falls back to env var GRAPHTELL_PROJECT_ID when absent)
        #[arg(long)]
        project: Option<i64>,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // MCP mode uses stdout as the JSON-RPC channel; any stdout log would corrupt the protocol;
        // so we write to stderr uniformly, which is harmless for serve as well.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let mut config = AppConfig {
        data_dir: cli.data_dir.clone(),
        fkb_dir: cli.fkb_dir.clone(),
        ..Default::default()
    };

    match cli.command {
        Command::Serve { bind, port, ui_dir } => {
            config.bind = bind;
            config.port = port;
            config.ui_dir = ui_dir;
            let container = Container::new(config)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let addr = format!("{}:{}", container.config.bind, container.config.port)
                    .parse()?;
                gt_adapter_http::serve(container.router(), addr).await
            })
        }
        Command::Validate => validate_fkbs(&config.resolve_fkb_dir()),
        other => {
            let container = Container::new(config)?;
            let projects = gt_application::ProjectService::new(
                container.store.clone(),
                std::sync::Arc::new(gt_domain::port::SystemClock),
            );
            let pipeline = gt_application::PipelineService::new(
                container.store.clone(),
                std::sync::Arc::clone(&container.deps),
                std::sync::Arc::clone(&container.rules)
                    as std::sync::Arc<dyn gt_domain::port::RuleProvider>,
            );

            match other {
                Command::Create { name, path, description } => {
                    let project = projects.create(NewProject {
                        name,
                        root_path: path,
                        description,
                        config: None,
                    })?;
                    println!("Created project #{} {}", project.id, project.name);
                    let out = pipeline.run(project.id, &gt_domain::port::NoopObserver)?;
                    print_outcome(&out, &container);
                }
                Command::List => {
                    for p in projects.list()? {
                        println!("#{} {:<24} {:<10} {}", p.id, p.name, p.status, p.root_path.display());
                    }
                }
                Command::Delete { project } => {
                    projects.delete(gt_domain::model::ProjectId(project))?;
                    println!("Deleted project #{project}");
                }
                Command::Run { project } => {
                    let out = pipeline.run(gt_domain::model::ProjectId(project), &gt_domain::port::NoopObserver)?;
                    print_outcome(&out, &container);
                }
                Command::Export { project } => {
                    let pid = ProjectId(project);
                    const PAGE: u32 = 5000;
                    let mut nodes = Vec::new();
                    let mut offset = 0u32;
                    loop {
                        let batch = container.store.query_nodes(&NodeFilter {
                            project_id: pid,
                            kind: None,
                            name_contains: None,
                            limit: Some(PAGE),
                            offset: Some(offset),
                        })?;
                        let got = batch.len() as u32;
                        nodes.extend(batch);
                        if got < PAGE {
                            break;
                        }
                        offset += got;
                    }
                    // Each node's out-edges already cover the whole graph (in the undirected view, in-edges are others' out-edges),
                    // so fetch in one batch to avoid per-node round trips.
                    let ids: Vec<_> = nodes.iter().map(|n| n.id).collect();
                    let outgoing = container.store.edges_outgoing(&ids)?;
                    let mut edges = Vec::new();
                    for es in outgoing.values() {
                        edges.extend(es.iter().cloned());
                    }
                    let files = container.store.list_files(pid, None)?;
                    let file_paths: std::collections::BTreeMap<i64, String> = files
                        .iter()
                        .map(|f| (f.id.get(), f.path.clone()))
                        .collect();
                    let payload = serde_json::json!({
                        "project_id": project,
                        "nodes": nodes,
                        "edges": edges,
                        "files": file_paths,
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
                Command::Stats { project } => {
                    let id = gt_domain::model::ProjectId(project);
                    let stats = container.store.stats(id)?;
                    println!("nodes {} / edges {} / annotations {}", stats.nodes, stats.edges, stats.annotations);
                    for (kind, count) in stats.by_kind {
                        println!("  {:<16} {}", kind, count);
                    }
                    for d in container.store.list_diagnostics(id, 20)? {
                        println!("  [{}] {} {}", d.phase, d.code, d.message);
                    }
                }
                Command::Rules => {
                    for r in container.rule_service().rules() {
                        println!(
                            "{:<32} {:<8} {:<14} {}",
                            r.id,
                            severity_name(r.severity),
                            r.category,
                            r.title
                        );
                    }
                }
                Command::Check { project, rule, dry_run, json } => {
                    let only = if rule.is_empty() { None } else { Some(rule.clone()) };
                    let report = container.rule_service().check(
                        gt_domain::model::ProjectId(project),
                        only.as_deref(),
                        !dry_run,
                    )?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                        return Ok(());
                    }
                    println!(
                        "Rules {}/{}: {} violation(s) matched, {}ms{}",
                        report.rules_run,
                        report.rules_total,
                        report.violations.len(),
                        report.duration_ms,
                        if dry_run { "(preview, nothing persisted)" } else { "" }
                    );
                    for (sev, n) in &report.by_severity {
                        println!("  {:<10} {}", sev, n);
                    }
                    if !report.rules_silent.is_empty() {
                        println!();
                        println!(
                            "  ⚠ {} rules ran but matched 0 — before concluding\"the code is clean\", suspect the rule has gone blind:",
                            report.rules_silent.len()
                        );
                        for s in &report.rules_silent {
                            println!("      - {}", s);
                        }
                    }
                    if !report.rules_unavailable.is_empty() {
                        println!();
                        println!(
                            "  ⊘ {} rules have unmet criteria and are disabled (running them would only yield vacuously-true positives):",
                            report.rules_unavailable.len()
                        );
                        for s in &report.rules_unavailable {
                            println!("      - {}", s);
                        }
                    }
                    if !report.rules_not_applicable.is_empty() {
                        println!();
                        println!(
                            "  · {} rules are not applicable to this project's tech stack (expected, not a fault):",
                            report.rules_not_applicable.len()
                        );
                        for s in &report.rules_not_applicable {
                            println!("      - {}", s);
                        }
                    }
                    println!();
                    for v in report.violations.iter().take(50) {
                        let loc = v
                            .location()
                            .unwrap_or_else(|| v.node_id.get().to_string());
                        println!("  [{}] {} · {}", severity_name(v.severity), v.rule_id, loc);
                        println!("        {}", v.message);
                    }
                    if report.violations.len() > 50 {
                        println!("  … {} more not shown (use --json to see all)", report.violations.len() - 50);
                    }
                }
                Command::Embed { project } => {
                    let n = container
                        .recall_service()
                        .warm_up(gt_domain::model::ProjectId(project))?;
                    println!(
                        "Computed and persisted {n} node vectors for project #{project} (data/embeddings/{project}.rmp)"
                    );
                }
                Command::Cosine {
                    project,
                    query,
                    names,
                    top,
                } => {
                    let filter: Vec<String> = names
                        .unwrap_or_default()
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let rows = container.recall_service().debug_cosine(
                        gt_domain::model::ProjectId(project),
                        &query,
                        &filter,
                        top,
                    )?;
                    let th = gt_application::recall_service::VECTOR_THRESHOLD;
                    println!("Cosine between query '{query}' and node vectors (threshold {th}):");
                    if rows.is_empty() {
                        println!("  (no matching nodes: relax --names or drop the flag)");
                    }
                    for (kind, name, c) in rows {
                        let flag = if c >= th { "above threshold" } else { "below threshold" };
                        println!("  {c:.4}  [{flag}]  {kind}  {name}");
                    }
                }
                Command::Recall { project, query, limit, hops, markdown, include_body } => {
                    let result = container.recall_service().recall(
                        gt_domain::model::ProjectId(project),
                        &gt_application::RecallQuery {
                            query: query.clone(),
                            limit,
                            hops,
                            kinds: Vec::new(),
                            with_snippets: true,
                            include_body,
                        },
                    )?;
                    if markdown {
                        println!("{}", result.markdown);
                        return Ok(());
                    }
                    println!(
                        "Query terms: {}   structural hints: {}   {} seed(s)   {} hit(s)",
                        if result.terms.is_empty() {
                            "(none)".to_string()
                        } else {
                            result.terms.join(", ")
                        },
                        if result.kind_hints.is_empty() {
                            "(none)".to_string()
                        } else {
                            result.kind_hints.join(", ")
                        },
                        result.seeds.len(),
                        result.hits.len()
                    );
                    for (i, h) in result.hits.iter().enumerate() {
                        let loc = match (&h.file, h.line) {
                            (Some(f), Some(l)) => format!("{f}:{l}"),
                            (Some(f), None) => f.clone(),
                            _ => "—".to_string(),
                        };
                        println!(
                            "  {:>2}. {:<14} {:<40} {:>8.1}  hops {}  {}",
                            i + 1,
                            h.kind,
                            h.name,
                            h.score,
                            h.hop,
                            loc
                        );
                    }
                }
                Command::Mcp { base_url, project } => {
                    let project = project
                        .or_else(|| {
                            std::env::var("GRAPHTELL_PROJECT_ID")
                                .ok()
                                .and_then(|s| s.parse::<i64>().ok())
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!("missing --project or the GRAPHTELL_PROJECT_ID environment variable")
                        })?;
                    let bridge = mcp::McpBridge::new(base_url, project);
                    return bridge.run();
                }
                _ => unreachable!(),
            }
            Ok(())
        }
    }
}

fn severity_name(s: gt_domain::model::Severity) -> &'static str {
    match s {
        gt_domain::model::Severity::Critical => "critical",
        gt_domain::model::Severity::Error => "error",
        gt_domain::model::Severity::Warning => "warning",
        gt_domain::model::Severity::Info => "info",
    }
}

fn print_outcome(out: &gt_pipeline::runner::PipelineOutcome, container: &Container) {
    println!("Sub-projects {} / files {}", out.sub_projects.len(), out.files.len());
    for r in &out.reports {
        println!(
            "  {:<14} nodes {:>7}  edges {:>7}  annotations {:>6}  {:>6}ms",
            r.phase, r.nodes_created, r.edges_created, r.annotations_created, r.duration_ms
        );
    }
    let _ = container;
}


/// Validate an FKB directory: parse file by file plus convention checks, no DB, no graph build.
///
/// More useful than a bare `load_dir` is the **convention** layer of checks:
/// * a node kind produced by some rule is in neither the kernel `SYNTHESIZED` list nor any FKB's `semantic_kinds`
///   — the folded view would hide it as a syntax node ("wrote it but nothing happened", a classic cause);
/// * an edge kind is in neither the kernel `SEMANTIC` / `BRIDGE` list nor declared in **any** FKB's
///   `semantic_edge_kinds` / `bridge_edge_kinds` — it won't be rendered as a semantic / bridge edge.
fn validate_fkbs(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        anyhow::bail!("FKB directory does not exist: {}", dir.display());
    }
    let mut files = Vec::new();
    collect_yaml(dir, &mut files);
    if files.is_empty() {
        println!("No *.yaml/*.yml found under {}", dir.display());
        return Ok(());
    }

    // Kinds known to the kernel (open-string newtype; the constant is documentation, not a restriction).
    let builtin_node: std::collections::HashSet<String> =
        NodeKind::SYNTHESIZED.iter().map(|s| s.to_string()).collect();
    let builtin_edge: std::collections::HashSet<String> = EdgeKind::SEMANTIC
        .iter()
        .chain(EdgeKind::BRIDGE.iter())
        .map(|s| s.to_string())
        .collect();

    // First pass: load every file, aggregating across files the "edge kinds explicitly declared by some FKB".
    // An edge kind may be declared in file A and used in file B, so it must be aggregated before it counts as "known".
    let mut declared_edge: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Rule ids are namespaced at load time (`<fkb-id>-<local-id>`), so a collision can only happen when two
    // knowledge bases declare the same qualified id — the second one is then dropped silently by `dedup_rules`.
    let mut rule_id_owner: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    for path in &files {
        if let Ok(fk) = YamlKnowledgeBase::load_file(path) {
            for k in fk.semantic_edge_kinds.iter().chain(fk.bridge_edge_kinds.iter()) {
                declared_edge.insert(k.clone());
            }
            for r in &fk.rules {
                rule_id_owner.entry(r.id.clone()).or_insert_with(|| path.clone());
            }
        }
    }

    let mut ok = 0usize;
    let mut broken = 0usize;
    for path in &files {
        match YamlKnowledgeBase::load_file(path) {
            Ok(fk) => {
                ok += 1;
                let mut warns = Vec::new();
                for r in &fk.rules {
                    // Same qualified id declared by another knowledge base: one of the two never runs.
                    if let Some(other) = rule_id_owner.get(&r.id) {
                        if *other != *path {
                            warns.push(format!(
                                "Rule id `{}` is also declared by {} — ids are deduplicated globally, so the later one is silently dropped",
                                r.id,
                                other.display()
                            ));
                        }
                    }

                    for a in &r.binding {
                        // A synthesized node's effective kind = subtype (if any), otherwise node.
                        if let Action::Synthesize(s) = a {
                            let kind = s
                                .subtype
                                .clone()
                                .filter(|x| !x.is_empty())
                                .unwrap_or_else(|| s.node.0.clone());
                            let declared = fk
                                .semantic_kinds
                                .iter()
                                .any(|k| k == &kind);
                            if !builtin_node.contains(&kind) && !declared {
                                warns.push(format!(
                                    "Node kind `{kind}` is in neither the kernel list nor semantic_kinds — the folded view will hide it"
                                ));
                            }
                        }
                        // Whether the edge kind is in the kernel's semantic / bridge list.
                        let edge_kind = match a {
                            Action::Synthesize(s) => {
                                s.link.as_ref().map(|l| l.kind.0.clone())
                            }
                            Action::Link(l) => Some(l.kind.0.clone()),
                            Action::Project(p) => Some(p.kind.0.clone()),
                            Action::Annotate(_) => None,
                        };
                        if let Some(ek) = edge_kind {
                            if !builtin_edge.contains(&ek) && !declared_edge.contains(&ek) {
                                warns.push(format!(
                                    "Edge kind `{ek}` is not in the kernel SEMANTIC/BRIDGE and is not declared by any FKB's semantic_edge_kinds/bridge_edge_kinds — it will not be rendered as a semantic / bridge edge"
                                ));
                            }
                        }
                    }
                }
                let n_synth = fk
                    .rules
                    .iter()
                    .filter(|r| {
                        r.binding.iter().any(|a| matches!(a, Action::Synthesize(_)))
                    })
                    .count();
                let extra = if fk.semantic_kinds.is_empty() {
                    String::new()
                } else {
                    format!(", semantic_kinds={:?}", fk.semantic_kinds)
                };
                let edge_extra = if fk.semantic_edge_kinds.is_empty()
                    && fk.bridge_edge_kinds.is_empty()
                {
                    String::new()
                } else {
                    format!(
                        ", edge_kinds(sem={:?},bridge={:?})",
                        fk.semantic_edge_kinds, fk.bridge_edge_kinds
                    )
                };
                println!(
                    "✓ {}  (id={}, language={}, {} rule(s), {} synthesis rule(s){}{})",
                    path.display(),
                    fk.id,
                    fk.language.0,
                    fk.rules.len(),
                    n_synth,
                    extra,
                    edge_extra
                );
                for w in warns {
                    println!("    ⚠ {w}");
                }
            }
            Err(e) => {
                broken += 1;
                println!("✗ {}  — failed to parse: {e}", path.display());
            }
        }
    }
    println!();
    println!("{} file(s) in total: {} passed, {} failed", files.len(), ok, broken);
    println!(
        "Rule / loader / root_rule / resolver ids are namespaced as `<id>-<local id>` (see docs/fkb-authoring.md)."
    );
    if broken > 0 {
        anyhow::bail!("Some FKB files could not be parsed");
    }
    Ok(())
}

/// Recursively collect *.yaml / *.yml under a directory.
fn collect_yaml(dir: &Path, out: &mut Vec<PathBuf>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_yaml(&p, out);
        } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") {
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gt_main_it_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- collect_yaml: real recursion + extension filtering ----

    #[test]
    fn collect_yaml_finds_all_extensions_recursively() {
        let dir = temp_root("collect");
        std::fs::write(dir.join("a.yaml"), b"").unwrap();
        std::fs::write(dir.join("b.yml"), b"").unwrap();
        std::fs::write(dir.join("c.YAML"), b"").unwrap(); // case-insensitive
        std::fs::write(dir.join("d.YML"), b"").unwrap();
        std::fs::write(dir.join("e.txt"), b"").unwrap(); // excluded
        std::fs::write(dir.join("f.json"), b"").unwrap(); // excluded
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/g.yaml"), b"").unwrap();
        std::fs::write(dir.join("sub/h.md"), b"").unwrap();
        let mut out = Vec::new();
        collect_yaml(&dir, &mut out);
        assert_eq!(out.len(), 5, "应递归收集 *.yaml/*.yml（含大小写变体），排除其他扩展名");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn collect_yaml_handles_missing_dir_gracefully() {
        let dir = std::env::temp_dir().join(format!("gt_main_it_{}_missing", std::process::id()));
        let mut out = Vec::new();
        collect_yaml(&dir, &mut out); // must not panic on a non-existent root
        assert!(out.is_empty());
    }

    // ---- validate_fkbs: exit-code guards (CI runs `graphtell validate` and fails on non-zero) ----

    #[test]
    fn validate_fkbs_errors_on_missing_dir() {
        let dir = std::env::temp_dir().join(format!("gt_main_it_{}_no_fkb", std::process::id()));
        assert!(validate_fkbs(&dir).is_err(), "不存在的 FKB 目录必须 bail（非零退出）");
    }

    #[test]
    fn validate_fkbs_ok_on_empty_dir() {
        let dir = temp_root("empty_fkb");
        assert!(validate_fkbs(&dir).is_ok(), "空目录（无 yaml）应返回 Ok");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_fkbs_errors_on_unparseable_yaml() {
        let dir = temp_root("broken_fkb");
        std::fs::write(dir.join("bad.yaml"), b"key: [unclosed").unwrap();
        assert!(
            validate_fkbs(&dir).is_err(),
            "解析失败的 FKB 文件必须使 validate 返回 Err（CI 非零退出）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
