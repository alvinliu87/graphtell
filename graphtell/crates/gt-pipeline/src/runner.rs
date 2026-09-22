//! 流水线编排（P0 → P2 → P3 → P4 → P5 → P6 → P7）。

use std::time::Instant;

use gt_domain::error::Result;
use gt_domain::model::{
    GraphDelta, NamespacePolicy, NewSourceFile, NewSubProject, Phase, PhaseReport, Project,
    SourceFile, SubProject, SubProjectId,
};
use gt_domain::port::{
    FileScanner, FileSystem, GraphSink, KnowledgeProvider, ParserRegistry, PipelineObserver,
    ProjectWriter,
};
use serde_json::Value;
use tracing::{error, info};

use crate::context::PipelineContext;
use crate::phase::{
    annotate, cf_ast, cors, external, ingest, prepare, propagate, resolve, sign, taint, tx,
};

/// 流水线所需的基础设施集合（依赖倒置：由组装根注入）。
pub trait PipelineInfrastructure {
    fn fs(&self) -> &dyn FileSystem;
    fn scanner(&self) -> &dyn FileScanner;
    fn parsers(&self) -> &dyn ParserRegistry;
    fn kb(&self) -> &dyn KnowledgeProvider;
    fn projects(&self) -> &dyn ProjectWriter;
    fn graph(&self) -> &dyn GraphSink;
}

/// 流水线产出（供应用层落库与上报）。
#[derive(Debug, Default)]
pub struct PipelineOutcome {
    pub sub_projects: Vec<SubProject>,
    pub files: Vec<SourceFile>,
    pub reports: Vec<PhaseReport>,
    pub facts: Vec<(SubProjectId, Value)>,
}

/// 一次完整的建图运行。
pub fn run(
    project: &Project,
    infra: &dyn PipelineInfrastructure,
    observer: &dyn PipelineObserver,
) -> Result<PipelineOutcome> {
    let mut ctx = PipelineContext::new(project.clone());
    // 节点 id 由 `GraphWorkspace` 按工程分段分配（`NODE_ID_STRIDE`），不再依赖
    // "启动时读一次全局 MAX(id)" —— 那样并发建图时多条流水线会读到同一个起点，
    // 分配出重叠 id 并互相 `INSERT OR REPLACE` 掉对方的节点。
    ctx.ws.set_table_prefixes(project.config.table_prefixes.clone());
    let mut outcome = PipelineOutcome::default();

    // 语言的命名空间 / 成员书写规则：从解析器注册表抽取。
    // 新增一门语言时，这些规则随 `LanguageParser` 实现一起注册进来 —— 内核无需改动。
    for lang in infra.parsers().supported_languages() {
        if let Some(p) = infra.parsers().parser_for(&lang) {
            ctx.lang_policies
                .insert(lang.as_str().to_string(), NamespacePolicy::from_parser(p));
        }
    }
    if let Some(first) = infra.parsers().supported_languages().first() {
        if let Some(p) = infra.parsers().parser_for(first) {
            ctx.lang_policy_default = NamespacePolicy::from_parser(p);
        }
    }

    // ---------------------------------------------------------- P0 Ingest
    let started = Instant::now();
    observer.on_phase_start(project.id, &Phase(Phase::INGEST.to_string()));
    let root = ingest::validate_root(&project.root_path)?;
    let _ = root;
    let ingested = ingest::run(project, infra.scanner(), infra.parsers(), infra.fs())?;
    let subs: Vec<SubProject> = infra
        .projects()
        .replace_sub_projects(project.id, ingested.sub_projects.clone())?;
    let mut files: Vec<SourceFile> =
        infra.projects().replace_files(project.id, ingested.files.clone())?;
    ingest::assign_files(&mut files, &subs, &project.root_path);
    ctx.sub_projects = subs.clone();
    ctx.files = files.clone();
    outcome.sub_projects = subs;
    outcome.files = files;
    flush(
        infra.graph(),
        &mut ctx,
        &mut outcome,
        &Phase(Phase::INGEST.to_string()),
        started,
        observer,
        project.id,
    )?;

    // 新一次建图：清掉上一轮的图数据
    infra.graph().apply(&GraphDelta {
        reset_project: true,
        project_id: Some(project.id),
        ..Default::default()
    })?;

    // ---------------------------------------------------------- P2 CfAst
    let started = Instant::now();
    observer.on_phase_start(project.id, &Phase(Phase::CF_AST.to_string()));
    cf_ast::run(&mut ctx, infra.parsers(), infra.fs());
    flush(
        infra.graph(),
        &mut ctx,
        &mut outcome,
        &Phase(Phase::CF_AST.to_string()),
        started,
        observer,
        project.id,
    )?;

    // ---------------------------------------------------------- P3 Prepare
    let started = Instant::now();
    observer.on_phase_start(project.id, &Phase(Phase::PREPARE.to_string()));
    prepare::run(&mut ctx, infra.kb(), infra.fs(), infra.parsers())?;
    // MyBatis mapper XML → 伪调用点（原生 MyBatis 工程的表语义来源）。
    // 必须在 P2 之后（Mapper 接口方法节点已建）、P5 之前（伪调用点要喂给合成规则）。
    crate::mybatis::run(&mut ctx);
    // 框架标识回写
    for sub in &ctx.sub_projects {
        let ids = ctx.frameworks.get(&sub.id.get()).cloned().unwrap_or_default();
        let facts = ctx.ws.facts_snapshot(sub.id).unwrap_or(Value::Null);
        infra.projects().set_sub_project_frameworks(sub.id, ids)?;
        infra.projects().update_sub_project_facts(sub.id, facts.clone())?;
        outcome.facts.push((sub.id, facts));
    }
    flush(
        infra.graph(),
        &mut ctx,
        &mut outcome,
        &Phase(Phase::PREPARE.to_string()),
        started,
        observer,
        project.id,
    )?;

    if !project.config.full_pipeline {
        return Ok(outcome);
    }

    // ---------------------------------------------------------- P4 / P5 / P6 / P7
    let stages: Vec<(Phase, fn(&mut PipelineContext))> = vec![
        (Phase(Phase::ANNOTATE_PRE.to_string()), annotate::run_pre as _),
        (Phase(Phase::SYNTHESIZE.to_string()), annotate::run_synthesize as _),
        (Phase(Phase::ANNOTATE_POST.to_string()), annotate::run_post as _),
    ];
    for (phase, f) in stages {
        let started = Instant::now();
        observer.on_phase_start(project.id, &phase);
        f(&mut ctx);
        flush(
            infra.graph(),
            &mut ctx,
            &mut outcome,
            &phase,
            started,
            observer,
            project.id,
        )?;
    }

    let phase = Phase(Phase::RESOLVE.to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    resolve::run(&mut ctx, infra.kb());
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    // ---------------------------------------------------------- P8 Propagate
    // 必须在 P7 之后：依赖 P7 建好的 `Calls` 边；种子在 P5 收集、暂存于 ctx。
    let phase = Phase(Phase::PROPAGATE.to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    propagate::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    // ---------------------------------------------------------- P9 Taint
    // 必须在 P7 之后：依赖已解析的调用点（ctx.ws.calls）与各调用点的参数文本。
    // 与 P4 的 `Taint(sink)` 标注互补 —— 这里进一步判定"用户数据是否真的流入了 SQL 字符串"。
    let phase = Phase("Taint".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    taint::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    // ---------------------------------------------------------- P10 Cors
    // 必须在 P9 之后（亦仅依赖 ctx.ws.calls 与解析期捕获的 header_assignments）。
    // 检测「CORS 反射源站」：Access-Control-Allow-Origin 被设为请求 Origin。
    let phase = Phase("Cors".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    cors::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    // ---------------------------------------------------------- P11 Sign
    // 必须在 P9 之后（同样只依赖 ctx.ws.calls 与解析期捕获的 sign_compares）。
    // 检测「验签质量」：签名被 == / != 松散比较、签名用 md5 / sha1。
    // 注意：**不判**"有没有验签" —— 那样必须跨过程追 SDK，而 vendor 不在图里，必误报。
    let phase = Phase("Sign".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    sign::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    // ---------------------------------------------------------- P12 External
    // 依赖 P2 的 `in_loop` 与 P3 装载的 FKB `external_calls`（名单在 FKB，内核不认识名字）。
    // 检测「循环内发起远程调用」：网络往返比查询贵一个量级，串行 N 次更致命。
    let phase = Phase("External".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    external::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    // ---------------------------------------------------------- P13 Tx
    // 必须在 P7 之后：写动词名单来自 FKB `db_verbs`（与读 / 写分类同源）。
    // 检测「同一方法多次写库但未识别到事务边界」—— 部分成功会留下脏数据。
    let phase = Phase("Tx".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    tx::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn flush(
    sink: &dyn GraphSink,
    ctx: &mut PipelineContext,
    outcome: &mut PipelineOutcome,
    phase: &Phase,
    started: Instant,
    observer: &dyn PipelineObserver,
    project_id: gt_domain::model::ProjectId,
) -> Result<()> {
    let mut delta = ctx.ws.take_delta();
    delta.diagnostics.extend(ctx.ws.remaining_diagnostics());
    let report = PhaseReport {
        phase: phase.to_string(),
        nodes_created: delta.nodes.len() as u64,
        edges_created: delta.edges.len() as u64,
        annotations_created: delta.annotations.len() as u64,
        aliases_created: delta.aliases.len() as u64,
        diagnostics: delta.diagnostics.clone(),
        duration_ms: started.elapsed().as_millis() as u64,
    };
    // 落库失败必须让整条流水线失败，而不是继续往下跑。
    //
    // 曾经这里只记一条内存诊断就继续：后续阶段照常把边/标注写进库，工程状态
    // 也照常置为 ready，于是"节点 0 / 边 3 万"看起来像一次成功的建图。
    // 半个图比没有图更难发现 —— 直接让上层把工程标成 failed。
    if let Err(e) = sink.apply(&delta) {
        error!(
            "阶段 {} 落库失败（工程 {}）: {e} —— 丢弃本阶段 {} 节点 / {} 边",
            phase, project_id, report.nodes_created, report.edges_created
        );
        return Err(e);
    }
    info!(
        "阶段 {}: {} 节点 / {} 边 / {} 标注 / {}ms",
        phase, report.nodes_created, report.edges_created, report.annotations_created, report.duration_ms
    );
    observer.on_phase_end(project_id, &report);
    outcome.reports.push(report);
    Ok(())
}

/// 供外部引用，避免未使用告警。
pub fn _assert_types(_: Option<NewSubProject>, _: Option<NewSourceFile>) {}
