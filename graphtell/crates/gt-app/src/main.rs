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
                Command::Serve { .. } => unreachable!(),
            }
            Ok(())
        }
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
