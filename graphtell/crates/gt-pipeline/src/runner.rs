//! Pipeline orchestration (P0 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7).

use std::time::Instant;

use gt_domain::error::Result;
use gt_domain::model::{
    GraphDelta, NamespacePolicy, NewSourceFile, NewSubProject, Phase, PhaseReport, Project,
    Severity, SourceFile, SubProject, SubProjectId,
};
use gt_domain::port::{
    FileScanner, FileSystem, GraphSink, KnowledgeProvider, MarkerProvider, ParserRegistry,
    PipelineObserver, ProjectWriter, ResourceAdapterRegistry, TechStackRegistry,
};
use serde_json::Value;
use tracing::{error, info};

use crate::context::PipelineContext;
use crate::phase::{
    annotate, cf_ast, cors, external, guard, ingest, prepare, propagate, resolve, sign, taint, tx,
};

/// The set of infrastructure a pipeline needs (dependency inversion: injected by the composition root).
pub trait PipelineInfrastructure {
    fn fs(&self) -> &dyn FileSystem;
    fn scanner(&self) -> &dyn FileScanner;
    fn parsers(&self) -> &dyn ParserRegistry;
    /// The tech-stack registry, for tech-stack-specific prepare logic (PSR-4, PHP config, Laravel migrations, …).
    fn techstack(&self) -> &dyn TechStackRegistry;
    /// The sub-project detection markers (bootstrap knowledge needed before any FKB is loaded).
    fn markers(&self) -> &dyn MarkerProvider;
    /// The resource adapters, which turn files that are **not source code** (MyBatis mapper XML, …) into facts.
    fn resources(&self) -> &dyn ResourceAdapterRegistry;
    fn kb(&self) -> &dyn KnowledgeProvider;
    fn projects(&self) -> &dyn ProjectWriter;
    fn graph(&self) -> &dyn GraphSink;
}

/// The pipeline's output (for the application layer to persist and report).
#[derive(Debug, Default)]
pub struct PipelineOutcome {
    pub sub_projects: Vec<SubProject>,
    pub files: Vec<SourceFile>,
    pub reports: Vec<PhaseReport>,
    pub facts: Vec<(SubProjectId, Value)>,
}

/// One complete graph-building run.
pub fn run(
    project: &Project,
    infra: &dyn PipelineInfrastructure,
    observer: &dyn PipelineObserver,
) -> Result<PipelineOutcome> {
    let mut ctx = PipelineContext::new(project.clone());
    ctx.ws.set_table_prefixes(project.config.table_prefixes.clone());
    let mut outcome = PipelineOutcome::default();

    // Language namespace / member notation rules: extracted from the parser registry.
    // When a new language is added these rules register together with its `LanguageParser` implementation — the kernel needs no change.
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
    let ingested = ingest::run(
        project,
        infra.scanner(),
        infra.parsers(),
        infra.fs(),
        infra.markers(),
        infra.techstack(),
        infra.kb(),
    )?;
    let subs: Vec<SubProject> = infra
        .projects()
        .replace_sub_projects(project.id, ingested.sub_projects.clone())?;
    let mut files: Vec<SourceFile> =
        infra.projects().replace_files(project.id, ingested.files.clone())?;
    ingest::assign_files(&mut files, &subs, &project.root_path);
    ctx.sub_projects = subs.clone();
    ctx.files = files.clone();
    // P3 rebuilds a sub-project's facts from the workspace, so the exclusions resolved here have to be
    // seeded into the workspace too — otherwise the answer to "why is this directory missing?" is lost
    // as soon as Prepare runs.
    for sub in &subs {
        if let Some(excludes) = sub.facts.get("excludes") {
            ctx.ws.set_fact(sub.id, "excludes", excludes.clone());
        }
    }
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

    // A new build: clear the previous round's graph data
    infra.graph().apply(&GraphDelta {
        reset_project: true,
        project_id: Some(project.id),
        ..Default::default()
    })?;

    // Reported **after** the reset: `reset_project` deletes the previous round's diagnostics, so
    // anything written before it would be wiped.
    let phase = Phase(Phase::INGEST.to_string());
    for d in &ingested.diagnostics {
        ctx.ws.diagnose(
            &phase,
            &d.code,
            Severity::Warning,
            &d.message,
            d.location.clone(),
        );
    }

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
    prepare::run(
        &mut ctx,
        infra.kb(),
        infra.fs(),
        infra.parsers(),
        infra.techstack(),
    )?;
    // Pseudo call sites injected from resource files that are not source code (MyBatis mapper XML today).
    // Must run after P3 for two reasons: the owner nodes come from P2, and **whether an adapter applies at all is
    // decided by P3's framework detection** (`ctx.frameworks`) rather than by the kernel — so this stays before
    // P5, whose synthesis rules consume the injected calls.
    crate::resource::run(&mut ctx, infra.resources(), infra.fs());
    // Write the framework identifiers back
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
        if phase.0 == Phase::SYNTHESIZE {
            let cp = Phase("GuardCapability".to_string());
            let started = Instant::now();
            observer.on_phase_start(project.id, &cp);
            guard::run_capabilities(&mut ctx);
            flush(
                infra.graph(),
                &mut ctx,
                &mut outcome,
                &cp,
                started,
                observer,
                project.id,
            )?;
        }
    }

    let phase = Phase(Phase::RESOLVE.to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    resolve::run(&mut ctx, infra.kb());
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    // ---------------------------------------------------------- P8 Propagate
    // Must run after P7: it depends on the `Calls` edges P7 built; seeds are collected in P5 and staged in ctx.
    let phase = Phase(Phase::PROPAGATE.to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    propagate::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    let phase = Phase("Taint".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    taint::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    let phase = Phase("Cors".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    cors::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    let phase = Phase("Sign".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    sign::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    let phase = Phase("External".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    external::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    let phase = Phase("Tx".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    tx::run(&mut ctx);
    flush(infra.graph(), &mut ctx, &mut outcome, &phase, started, observer, project.id)?;

    let phase = Phase("Guard".to_string());
    let started = Instant::now();
    observer.on_phase_start(project.id, &phase);
    guard::run(&mut ctx);
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
    if let Err(e) = sink.apply(&delta) {
        error!(
            "persisting phase {} failed (project {}): {e} — dropping this phase's {} nodes / {} edges",
            phase, project_id, report.nodes_created, report.edges_created
        );
        return Err(e);
    }
    info!(
        "phase {}: {} nodes / {} edges / {} annotations / {}ms",
        phase, report.nodes_created, report.edges_created, report.annotations_created, report.duration_ms
    );
    observer.on_phase_end(project_id, &report);
    outcome.reports.push(report);
    Ok(())
}

/// Exported for external reference, to avoid an unused warning.
pub fn _assert_types(_: Option<NewSubProject>, _: Option<NewSourceFile>) {}
