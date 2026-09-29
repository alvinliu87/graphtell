//! GraphTell —— 代码库图化分析平台。
//!
//! ```text
//! graphtell serve                      启动 HTTP 服务（Tauri / Web UI 共用）
//! graphtell create --name X --path /p   创建工程并自动建图
//! graphtell run    --project 1          对已有工程重新建图
//! graphtell stats  --project 1          查看图规模
//! ```

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

/// MCP(stdio) 桥：IDE 经由它连接常驻服务。
mod mcp;
use gt_adapter_fkb::YamlKnowledgeBase;
use gt_app::{AppConfig, Container};
use gt_domain::model::fkb::Action;
use gt_domain::model::kinds::{EdgeKind, NodeKind};
use gt_domain::model::{NewProject, ProjectId};
use gt_domain::port::{DiagnosticSink, GraphQuery, NodeFilter, ProjectReader};

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
        /// 构建好的 Web UI（React SPA）目录；指定后后端会顺带托管前端，
        /// 根路径 `/` 即 SPA 入口（未指定则 `/` 回退到内嵌独立页 `/compose`）。
        #[arg(long)]
        ui_dir: Option<PathBuf>,
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
    /// 导出工程图的全部节点与边（JSON），供静态 demo / 外部分析使用。
    ///
    /// 输出 `{project_id, nodes, edges, files}`：`files` 是 `file_id -> 路径` 的映射，
    /// 便于消费方把节点定位回源码文件，而不必再查库。
    Export {
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
    ///
    /// 召回编码器：默认编译开启 `model-candle` 时走真实的 bge-m3 语义向量
    /// （需本地权重，目录由环境变量 `GT_BGE_MODEL` 指定，默认 `models/bge-m3-safetensors`）；
    /// 未配置权重或 `--no-default-features` 构建时自动退回本地哈希编码器（离线、零依赖）。
    Recall {
        #[arg(long)]
        project: i64,
        /// 提示词（自然语言 + 标识符混写）。
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value_t = 2)]
        hops: u32,
        /// 只输出可直接粘给 LLM 的 Markdown 上下文包。
        #[arg(long)]
        markdown: bool,
        /// 把命中涉及的完整文件源码也附上（配合 `--markdown` 使用，省去自行读取全文）。
        #[arg(long)]
        include_body: bool,
    },
    /// 校验 FKB 目录（语法 + 约定），不连库、不建图。
    Validate,
    /// 预计算并持久化某工程的节点向量（不影响建图；可后台运行，如 `graphtell embed --project 1 &`）。
    ///
    /// 之后所有召回与重启都瞬时命中缓存，无需再对全图重算 bge 向量。权重目录同召回：
    /// 默认 `models/bge-m3-safetensors`，可用 `GT_BGE_MODEL` 覆盖。
    Embed {
        #[arg(long)]
        project: i64,
    },
    /// 诊断：输出查询与节点向量的余弦，用于判定"中文查不到目标"是模型能力问题
    /// 还是阈值 / 排序问题。
    ///
    /// 例：`graphtell cosine --project 1 --query "商品库存扣减失败回滚" --names stock`
    /// 若目标符号余弦本就低于阈值 → 模型 / 节点文本问题；若余弦够高却没召回 → 阈值 / 排序问题。
    Cosine {
        #[arg(long)]
        project: i64,
        #[arg(long)]
        query: String,
        /// 只看名字含这些子串的节点（逗号分隔）；不给则全量编码（大工程会很慢）
        #[arg(long)]
        names: Option<String>,
        #[arg(long, default_value = "20")]
        top: usize,
    },
    /// 以 MCP(stdio) 方式暴露给 IDE：经 HTTP 连接常驻服务，提供 `recall_code`（可带
    /// `include_body` 直接返回命中文件全文）/ `compose_prompt` / `check_compliance` /
    /// `list_violations` / `warmup_status`（查后台预热进度）工具。
    /// stdout 是 JSON-RPC 通道，日志走 stderr。
    Mcp {
        /// 常驻服务地址，默认 http://127.0.0.1:5177
        #[arg(long, default_value = "http://127.0.0.1:5177")]
        base_url: String,
        /// 目标工程 id（缺省时回退到环境变量 GRAPHTELL_PROJECT_ID）
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
        // MCP 模式把 stdout 当作 JSON-RPC 通道，任何 stdout 日志都会破坏协议；
        // 统一写 stderr，对 serve 也无害。
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
                Command::Export { project } => {
                    let pid = ProjectId(project);
                    // `query_nodes` 在 `limit: None` 时**默认只返回 100 条**（store 里
                    // `limit.unwrap_or(100)`）。大工程（CRMEB 约 9 万节点）若直接取一次
                    // 会被静默截断 —— 图导出必须是全量，故分页取到不足一页为止。
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
                    // 每个节点的出边已覆盖全图（无向视角下入边即别人的出边），
                    // 一次批量取回，避免逐节点往返。
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
                Command::Embed { project } => {
                    let n = container
                        .recall_service()
                        .warm_up(gt_domain::model::ProjectId(project))?;
                    println!(
                        "已为工程 #{project} 计算并持久化 {n} 个节点向量（data/embeddings/{project}.rmp）"
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
                    println!("查询「{query}」与节点向量的余弦（阈值 {th}）：");
                    if rows.is_empty() {
                        println!("  （无匹配节点：放宽 --names 或去掉该参数）");
                    }
                    for (kind, name, c) in rows {
                        let flag = if c >= th { "过阈值" } else { "低于阈值" };
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
                Command::Mcp { base_url, project } => {
                    let project = project
                        .or_else(|| {
                            std::env::var("GRAPHTELL_PROJECT_ID")
                                .ok()
                                .and_then(|s| s.parse::<i64>().ok())
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!("缺少 --project 或环境变量 GRAPHTELL_PROJECT_ID")
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
    println!("子工程 {} / 文件 {}", out.sub_projects.len(), out.files.len());
    for r in &out.reports {
        println!(
            "  {:<14} 节点 {:>7}  边 {:>7}  标注 {:>6}  {:>6}ms",
            r.phase, r.nodes_created, r.edges_created, r.annotations_created, r.duration_ms
        );
    }
    let _ = container;
}

/// 校验 FKB 目录：逐文件解析 + 约定检查，不连库、不建图。
///
/// 比单纯 `load_dir` 更有用的是**约定层**检查：
/// * 某规则造出的节点种类既不在内核 `SYNTHESIZED` 清单、也不在 FKB 的 `semantic_kinds`
///   里 —— 折叠视图会把它当语法节点藏起来（"写了没反应"的经典原因）；
/// * 某条边种类不在内核 `SEMANTIC` / `BRIDGE` 清单、也不在**任何** FKB 的
///   `semantic_edge_kinds` / `bridge_edge_kinds` 声明里 —— 不会被当语义/桥边渲染。
fn validate_fkbs(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        anyhow::bail!("FKB 目录不存在: {}", dir.display());
    }
    let mut files = Vec::new();
    collect_yaml(dir, &mut files);
    if files.is_empty() {
        println!("未在 {} 找到任何 *.yaml/*.yml", dir.display());
        return Ok(());
    }

    // 内核已知种类（开放字符串 newtype，常量即文档而非限制）。
    let builtin_node: std::collections::HashSet<String> =
        NodeKind::SYNTHESIZED.iter().map(|s| s.to_string()).collect();
    let builtin_edge: std::collections::HashSet<String> = EdgeKind::SEMANTIC
        .iter()
        .chain(EdgeKind::BRIDGE.iter())
        .map(|s| s.to_string())
        .collect();

    // 第一遍：加载所有文件，跨文件汇总「被某份 FKB 显式声明的边种类」。
    // 边种类可在文件 A 声明、在文件 B 使用，所以必须汇总后才算「已知」。
    let mut declared_edge: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in &files {
        if let Ok(fk) = YamlKnowledgeBase::load_file(path) {
            for k in fk.semantic_edge_kinds.iter().chain(fk.bridge_edge_kinds.iter()) {
                declared_edge.insert(k.clone());
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
                    for a in &r.binding {
                        // 合成节点的有效种类 = subtype（若有）否则 node。
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
                                    "节点种类 `{kind}` 既不在内核清单也不在 semantic_kinds —— 折叠视图会隐藏它"
                                ));
                            }
                        }
                        // 边种类是否在内核语义/桥清单。
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
                                    "边种类 `{ek}` 不在内核 SEMANTIC/BRIDGE，也没被任何 FKB 的 semantic_edge_kinds/bridge_edge_kinds 声明 —— 不会被当语义/桥边渲染"
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
                    "✓ {}  (id={}, 语言={}, 规则 {} 条, 合成 {} 条{}{})",
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
                println!("✗ {}  —— 解析失败: {e}", path.display());
            }
        }
    }
    println!();
    println!("共 {} 个文件: {} 通过, {} 失败", files.len(), ok, broken);
    if broken > 0 {
        anyhow::bail!("存在无法解析的 FKB 文件");
    }
    Ok(())
}

/// 递归收集目录下的 *.yaml / *.yml。
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
