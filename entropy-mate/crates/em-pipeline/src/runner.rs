//! 流水线编排（P0 → P2 → P3 → P4 → P5 → P6 → P7）。

use std::time::Instant;

use em_domain::error::Result;
use em_domain::model::{
    GraphDelta, NewSourceFile, NewSubProject, Phase, PhaseReport, Project, Severity, SourceFile,
    SubProject, SubProjectId,
};
use em_domain::port::{
    FileScanner, FileSystem, GraphSink, KnowledgeProvider, ParserRegistry, PipelineObserver,
    ProjectWriter,
};
use serde_json::Value;
use tracing::info;

use crate::context::PipelineContext;
use crate::phase::{annotate, cf_ast, ingest, prepare, propagate, resolve};

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
    ctx.ws.set_table_prefixes(project.config.table_prefixes.clone());
    let mut outcome = PipelineOutcome::default();

    // ---------------------------------------------------------- P0 Ingest
    let started = Instant::now();
    observer.on_phase_start(project.id, &Phase(Phase::INGEST.to_string()));
    let root = ingest::validate_root(&project.root_path)?;
    let _ = root;
    let ingested = ingest::run(project, infra.scanner())?;
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
    );

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
    );

    // ---------------------------------------------------------- P3 Prepare
    let started = Instant::now();
    observer.on_phase_start(project.id, &Phase(Phase::PREPARE.to_string()));
    prepare::run(&mut ctx, infra.kb(), infra.fs(), infra.parsers())?;
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
    );

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
        );
    }

    let phase = Phase(Phase::RESOLVE.to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    resolve::run(&mut ctx, infra.kb());
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id);

    // ---------------------------------------------------------- P8 Propagate
    // 必须在 P7 之后：依赖 P7 建好的 `Calls` 边；种子在 P5 收集、暂存于 ctx。
    let phase = Phase(Phase::PROPAGATE.to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    propagate::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id);

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
    project_id: em_domain::model::ProjectId,
) {
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
    if let Err(e) = sink.apply(&delta) {
        ctx.ws.diagnose(
            phase,
            "PersistFailed",
            Severity::Error,
            format!("阶段 {} 落库失败: {e}", phase),
            None,
        );
        let mut retry = ctx.ws.take_delta();
        retry.diagnostics.extend(ctx.ws.remaining_diagnostics());
        report_diagnostics_only(sink, &retry);
    }
    info!(
        "阶段 {}: {} 节点 / {} 边 / {} 标注 / {}ms",
        phase, report.nodes_created, report.edges_created, report.annotations_created, report.duration_ms
    );
    observer.on_phase_end(project_id, &report);
    outcome.reports.push(report);
}

fn report_diagnostics_only(sink: &dyn GraphSink, delta: &GraphDelta) {
    let only = GraphDelta {
        project_id: delta.project_id,
        diagnostics: delta.diagnostics.clone(),
        ..Default::default()
    };
    let _ = sink.apply(&only);
}

/// 供外部引用，避免未使用告警。
pub fn _assert_types(_: Option<NewSubProject>, _: Option<NewSourceFile>) {}
