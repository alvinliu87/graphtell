//! GraphTell —— 代码库图化分析平台。
//!
//! ```text
//! graphtell serve                      启动 HTTP 服务（Tauri / Web UI 共用）
//! graphtell create --name X --path /p   创建工程并自动建图
//! graphtell run    --project 1          对已有工程重新建图
//! graphtell stats  --project 1          查看图规模
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gt_app::{AppConfig, Container};
use gt_domain::model::NewProject;
use gt_domain::port::{DiagnosticSink, GraphQuery};

#[derive(Debug, Parser)]
#[command(name = "graphtell", version, about = "代码库图化分析平台")]
struct Cli {
    /// 数据目录。
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// FKB 目录（默认取内置知识库）。
    #[arg(long)]
    fkb_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 启动 HTTP API 服务。
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 5177)]
        port: u16,
    },
    /// 创建工程（创建后自动开始建图）。
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        description: Option<String>,
    },
    /// 列出全部工程。
    List,
    /// 删除工程。
    Delete {
        #[arg(long)]
        project: i64,
    },
    /// 对工程执行建图。
    Run {
        #[arg(long)]
        project: i64,
    },
    /// 查看图规模与诊断。
    Stats {
        #[arg(long)]
        project: i64,
    },
    /// 列出已装载的检查规则。
    Rules,
    /// 按规则检查工程是否违规。
    Check {
        #[arg(long)]
        project: i64,
        /// 只跑指定规则（可重复）。
        #[arg(long)]
        rule: Vec<String>,
        /// 只预览不写库。
        #[arg(long)]
        dry_run: bool,
        /// 以 JSON 输出完整报告。
        #[arg(long)]
        json: bool,
    },
    /// 按提示词在图上召回相关代码。
    Recall {
        #[arg(long)]
        project: i64,
        /// 提示词（自然语言 + 标识符混写）。
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 2)]
        hops: u32,
        /// 只输出可直接粘给 LLM 的 Markdown 上下文包。
        #[arg(long)]
        markdown: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut config = AppConfig {
        data_dir: cli.data_dir.clone(),
        fkb_dir: cli.fkb_dir.clone(),
        ..Default::default()
    };

    match cli.command {
        Command::Serve { bind, port } => {
            config.bind = bind;
            config.port = port;
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
                    println!("已创建工程 #{} {}", project.id, project.name);
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
                    println!("已删除工程 #{project}");
                }
                Command::Run { project } => {
                    let out = pipeline.run(gt_domain::model::ProjectId(project), &gt_domain::port::NoopObserver)?;
                    print_outcome(&out, &container);
                }
                Command::Stats { project } => {
                    let id = gt_domain::model::ProjectId(project);
                    let stats = container.store.stats(id)?;
                    println!("节点 {} / 边 {} / 标注 {}", stats.nodes, stats.edges, stats.annotations);
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
                        "规则 {}/{}，命中 {} 条违规，耗时 {}ms{}",
                        report.rules_run,
                        report.rules_total,
                        report.violations.len(),
                        report.duration_ms,
                        if dry_run { "（预览，未写库）" } else { "" }
                    );
                    for (sev, n) in &report.by_severity {
                        println!("  {:<10} {}", sev, n);
                    }
                    if !report.rules_silent.is_empty() {
                        println!();
                        println!(
                            "  ⚠ {} 条规则跑了但 0 命中 —— 在排除\"代码真干净\"之前，先怀疑规则瞎了：",
                            report.rules_silent.len()
                        );
                        for s in &report.rules_silent {
                            println!("      - {}", s);
                        }
                    }
                    if !report.rules_unavailable.is_empty() {
                        println!();
                        println!(
                            "  ⊘ {} 条规则判据不成立，已停用（跑下去只会产出恒真误报）：",
                            report.rules_unavailable.len()
                        );
                        for s in &report.rules_unavailable {
                            println!("      - {}", s);
                        }
                    }
                    if !report.rules_not_applicable.is_empty() {
                        println!();
                        println!(
                            "  · {} 条规则不适用于本工程技术栈（预期行为，非故障）：",
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
                        println!("  … 另有 {} 条未显示（用 --json 查看全部）", report.violations.len() - 50);
                    }
                }
                Command::Recall { project, query, limit, hops, markdown } => {
                    let result = container.recall_service().recall(
                        gt_domain::model::ProjectId(project),
                        &gt_application::RecallQuery {
                            query: query.clone(),
                            limit,
                            hops,
                            kinds: Vec::new(),
                            with_snippets: true,
                        },
                    )?;
                    if markdown {
                        println!("{}", result.markdown);
                        return Ok(());
                    }
                    println!(
                        "查询词：{}   结构提示：{}   种子 {} 个   命中 {} 条",
                        if result.terms.is_empty() {
                            "（无）".to_string()
                        } else {
                            result.terms.join(", ")
                        },
                        if result.kind_hints.is_empty() {
                            "（无）".to_string()
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
                            "  {:>2}. {:<14} {:<40} {:>8.1}  跳数 {}  {}",
                            i + 1,
                            h.kind,
                            h.name,
                            h.score,
                            h.hop,
                            loc
                        );
                    }
                }
                Command::Serve { .. } => unreachable!(),
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
    println!("子工程 {} / 文件 {}", out.sub_projects.len(), out.files.len());
    for r in &out.reports {
        println!(
            "  {:<14} 节点 {:>7}  边 {:>7}  标注 {:>6}  {:>6}ms",
            r.phase, r.nodes_created, r.edges_created, r.annotations_created, r.duration_ms
        );
    }
    let _ = container;
}
