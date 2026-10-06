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
    annotate, cf_ast, external, guard, ingest, prepare, propagate, resolve, sign, taint, tx,
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
    // No `lang_policy_default` assignment here on purpose: it used to be "whatever language registered
    // first", which hands an unrelated stack's notation (`\`, `::`) to every language the registry does not
    // cover — the same class of bug as the old PHP-hard-coded default, only non-deterministic. Uncovered
    // languages now keep the empty policy, i.e. "notation unknown", and skip the steps that need it.

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
    cf_ast::run(&mut ctx, infra.parsers(), infra.fs(), infra.techstack());
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

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::error::DomainError;
    use gt_domain::model::{
        AnnotationChannel, EdgeKind, FrameworkKnowledge, Language, MergeStrategy, NewAnnotation,
        NewEdge, NewNode, NewProject, NewSubProject, NewSourceFile, NodeId, NodeKind, ProjectConfig,
        ProjectId, ProjectPatch, ProjectStatus, Severity, Span, SubProject, SubProjectId,
    };
    use gt_domain::port::{
        DefaultResourceAdapterRegistry, DefaultTechStackRegistry, FileScanner, LanguageParser,
        Marker, PipelineObserver, ScanRequest, ScannedFile,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    // ---------------------------------------------------------------- fixtures

    fn project(full_pipeline: bool) -> Project {
        Project {
            id: ProjectId(1),
            name: "t".into(),
            // `ingest::validate_root` canonicalises the path for real, so this must exist.
            root_path: std::env::temp_dir(),
            description: None,
            config: ProjectConfig {
                full_pipeline,
                ..Default::default()
            },
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ctx() -> PipelineContext {
        PipelineContext::new(project(true))
    }

    fn add_node(ctx: &mut PipelineContext, fqn: &str) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind("Method".to_string()),
            name: fqn.to_string(),
            fqn: Some(fqn.to_string()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: gt_domain::model::Language::new("java"),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: Value::Null,
        })
    }

    fn edge(from: NodeId, to: NodeId) -> NewEdge {
        NewEdge {
            project_id: ProjectId(1),
            kind: EdgeKind("Calls".to_string()),
            from_id: from,
            to_id: to,
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    /// (nodes, edges, annotations, aliases, diagnostics, reset_project) of every applied delta.
    #[derive(Default)]
    struct FakeSink {
        applied: Mutex<Vec<(usize, usize, usize, usize, usize, bool)>>,
        fail: bool,
    }

    impl FakeSink {
        fn applied(&self) -> Vec<(usize, usize, usize, usize, usize, bool)> {
            self.applied.lock().unwrap().clone()
        }
    }

    impl GraphSink for FakeSink {
        fn apply(&self, delta: &GraphDelta) -> Result<()> {
            if self.fail {
                return Err(DomainError::NotFound("sink is down".into()));
            }
            self.applied.lock().unwrap().push((
                delta.nodes.len(),
                delta.edges.len(),
                delta.annotations.len(),
                delta.aliases.len(),
                delta.diagnostics.len(),
                delta.reset_project,
            ));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeObserver {
        starts: Mutex<Vec<String>>,
        ends: Mutex<Vec<String>>,
    }

    impl FakeObserver {
        fn starts(&self) -> Vec<String> {
            self.starts.lock().unwrap().clone()
        }
        fn ends(&self) -> Vec<String> {
            self.ends.lock().unwrap().clone()
        }
    }

    impl PipelineObserver for FakeObserver {
        fn on_phase_start(&self, _: ProjectId, phase: &Phase) {
            self.starts.lock().unwrap().push(phase.0.clone());
        }
        fn on_phase_end(&self, _: ProjectId, report: &PhaseReport) {
            self.ends.lock().unwrap().push(report.phase.clone());
        }
    }

    // ---------------------------------------------------------------- `flush()`

    #[test]
    fn flush_reports_every_count_the_delta_carries() {
        let mut ctx = ctx();
        let a = add_node(&mut ctx, "com.x.A");
        let b = add_node(&mut ctx, "com.x.B");
        ctx.ws.add_edge(edge(a, b));
        ctx.ws.annotate(NewAnnotation {
            node_id: a,
            channel: AnnotationChannel("FkbMark".to_string()),
            kind: "pii".to_string(),
            subkind: None,
            confidence: 1.0,
            evidence: Value::Null,
            phase: Phase("Test".to_string()),
            merge: MergeStrategy::Coexist,
        });
        let sink = FakeSink::default();
        let obs = FakeObserver::default();
        let mut outcome = PipelineOutcome::default();
        assert!(outcome.reports.is_empty());

        flush(
            &sink,
            &mut ctx,
            &mut outcome,
            &Phase(Phase::CF_AST.to_string()),
            Instant::now(),
            &obs,
            ProjectId(1),
        )
        .unwrap();

        let r = &outcome.reports[0];
        assert_eq!(r.phase, Phase::CF_AST);
        assert_eq!(r.nodes_created, 2);
        assert_eq!(r.edges_created, 1);
        assert_eq!(r.annotations_created, 1);
        assert_eq!(r.aliases_created, 0);
        // The observer sees exactly the report that is recorded.
        assert_eq!(obs.ends(), vec![Phase::CF_AST.to_string()]);
        // ...and one delta reaches the sink.
        assert_eq!(sink.applied().len(), 1);
    }

    #[test]
    fn flush_carries_the_phase_diagnostics() {
        let mut ctx = ctx();
        let phase = Phase(Phase::CF_AST.to_string());
        ctx.ws.diagnose(
            &phase,
            "AmbiguousFqn",
            Severity::Warning,
            "two candidates",
            Some("app/A.php:1".to_string()),
        );
        let sink = FakeSink::default();
        let mut outcome = PipelineOutcome::default();

        flush(
            &sink,
            &mut ctx,
            &mut outcome,
            &phase,
            Instant::now(),
            &FakeObserver::default(),
            ProjectId(1),
        )
        .unwrap();

        let diags = &outcome.reports[0].diagnostics;
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "AmbiguousFqn");
        // They must reach the sink too, not only the report.
        assert_eq!(sink.applied()[0].4, 1);
    }

    /// The delta is *taken*, so the next phase starts from zero instead of re-reporting the same nodes.
    #[test]
    fn flush_drains_the_workspace() {
        let mut ctx = ctx();
        add_node(&mut ctx, "com.x.A");
        let sink = FakeSink::default();
        let obs = FakeObserver::default();
        let mut outcome = PipelineOutcome::default();

        for phase in [Phase::CF_AST, Phase::PREPARE] {
            flush(
                &sink,
                &mut ctx,
                &mut outcome,
                &Phase(phase.to_string()),
                Instant::now(),
                &obs,
                ProjectId(1),
            )
            .unwrap();
        }

        assert_eq!(outcome.reports.len(), 2);
        assert_eq!(outcome.reports[0].nodes_created, 1);
        assert_eq!(
            outcome.reports[1].nodes_created, 0,
            "the second phase must not report the same batch of nodes again"
        );
        assert_eq!(
            obs.ends(),
            vec![Phase::CF_AST.to_string(), Phase::PREPARE.to_string()]
        );
    }

    /// A failed persist aborts the run: no report is recorded and the observer is not told the phase ended.
    #[test]
    fn flush_propagates_a_sink_failure() {
        let mut ctx = ctx();
        add_node(&mut ctx, "com.x.A");
        let sink = FakeSink {
            fail: true,
            ..Default::default()
        };
        let obs = FakeObserver::default();
        let mut outcome = PipelineOutcome::default();

        let res = flush(
            &sink,
            &mut ctx,
            &mut outcome,
            &Phase(Phase::CF_AST.to_string()),
            Instant::now(),
            &obs,
            ProjectId(1),
        );

        assert!(res.is_err());
        assert!(outcome.reports.is_empty(), "a persistence failure must not be recorded in the report");
        assert!(obs.ends().is_empty(), "the observer must not be notified that the phase completed");
        assert!(sink.applied().is_empty());
    }

    /// `flush` must never set `reset_project`: wiping the graph is a deliberate one-off call made by `run`
    /// before the first phase, not something every phase flush does.
    #[test]
    fn flush_never_resets_the_project() {
        let mut ctx = ctx();
        add_node(&mut ctx, "com.x.A");
        let sink = FakeSink::default();
        let mut outcome = PipelineOutcome::default();

        flush(
            &sink,
            &mut ctx,
            &mut outcome,
            &Phase(Phase::CF_AST.to_string()),
            Instant::now(),
            &FakeObserver::default(),
            ProjectId(1),
        )
        .unwrap();

        assert!(!sink.applied()[0].5, "flush must not clear the project graph");
    }

    // ---------------------------------------------------------------- `run()` orchestration
    //
    // The phases themselves are exercised end to end (with real adapters) by `crates/gt-pipeline/tests`.
    // What is pinned here is the **orchestration**: which phases run, in which order, and that a partial
    // pipeline stops after P3.

    struct StubFs;
    impl FileSystem for StubFs {
        fn exists(&self, _: &Path) -> bool {
            true
        }
        fn is_dir(&self, _: &Path) -> bool {
            true
        }
        fn read_to_string(&self, _: &Path) -> Result<String> {
            Ok(String::new())
        }
        fn len(&self, _: &Path) -> Result<u64> {
            Ok(0)
        }
    }

    struct StubScanner;
    impl FileScanner for StubScanner {
        fn scan(&self, _: &ScanRequest) -> Result<Vec<ScannedFile>> {
            Ok(Vec::new())
        }
        fn find_markers(&self, _: &Path, _: &[&str], _: usize) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }

    /// No parser registered: `run` must then leave every language's notation policy empty rather than
    /// defaulting to some other stack's (see the comment above `lang_policy_default`).
    struct StubParsers;
    impl ParserRegistry for StubParsers {
        fn parser_for(&self, _: &gt_domain::model::Language) -> Option<&dyn LanguageParser> {
            None
        }
        fn supported_languages(&self) -> Vec<gt_domain::model::Language> {
            Vec::new()
        }
    }

    struct StubMarkers;
    impl MarkerProvider for StubMarkers {
        fn markers(&self) -> Vec<Marker> {
            Vec::new()
        }
    }

    struct StubKb;
    impl KnowledgeProvider for StubKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            Vec::new()
        }
        fn by_id(&self, _: &str) -> Option<&FrameworkKnowledge> {
            None
        }
    }

    struct StubProjects;
    impl ProjectWriter for StubProjects {
        fn create_project(&self, _: gt_domain::model::NewProject) -> Result<Project> {
            Err(DomainError::NotFound("not used by run".into()))
        }
        fn update_project(
            &self,
            _: ProjectId,
            _: gt_domain::model::ProjectPatch,
        ) -> Result<Project> {
            Err(DomainError::NotFound("not used by run".into()))
        }
        fn delete_project(&self, _: ProjectId) -> Result<()> {
            Ok(())
        }
        fn set_project_status(&self, _: ProjectId, _: ProjectStatus) -> Result<()> {
            Ok(())
        }
        fn replace_sub_projects(
            &self,
            _: ProjectId,
            _: Vec<NewSubProject>,
        ) -> Result<Vec<SubProject>> {
            Ok(Vec::new())
        }
        fn update_sub_project_facts(&self, _: SubProjectId, _: Value) -> Result<()> {
            Ok(())
        }
        fn set_sub_project_frameworks(&self, _: SubProjectId, _: Vec<String>) -> Result<()> {
            Ok(())
        }
        fn replace_files(&self, _: ProjectId, _: Vec<NewSourceFile>) -> Result<Vec<SourceFile>> {
            Ok(Vec::new())
        }
    }

    struct StubInfra {
        graph: FakeSink,
        techstack: DefaultTechStackRegistry,
        resources: DefaultResourceAdapterRegistry,
        projects: Box<dyn ProjectWriter>,
    }

    impl StubInfra {
        fn new(graph: FakeSink) -> Self {
            Self {
                graph,
                techstack: DefaultTechStackRegistry::new(),
                resources: DefaultResourceAdapterRegistry::new(),
                projects: Box::new(StubProjects),
            }
        }
        /// Builds the infra with a custom `ProjectWriter` — used to pin that `run` propagates an early / late
        /// infrastructure-write failure instead of swallowing it.
        fn with_projects(graph: FakeSink, projects: Box<dyn ProjectWriter>) -> Self {
            Self {
                projects,
                ..Self::new(graph)
            }
        }
    }

    impl PipelineInfrastructure for StubInfra {
        fn fs(&self) -> &dyn FileSystem {
            &StubFs
        }
        fn scanner(&self) -> &dyn FileScanner {
            &StubScanner
        }
        fn parsers(&self) -> &dyn ParserRegistry {
            &StubParsers
        }
        fn techstack(&self) -> &dyn TechStackRegistry {
            &self.techstack
        }
        fn markers(&self) -> &dyn MarkerProvider {
            &StubMarkers
        }
        fn resources(&self) -> &dyn ResourceAdapterRegistry {
            &self.resources
        }
        fn kb(&self) -> &dyn KnowledgeProvider {
            &StubKb
        }
        fn projects(&self) -> &dyn ProjectWriter {
            &*self.projects
        }
        fn graph(&self) -> &dyn GraphSink {
            &self.graph
        }
    }

    /// A `ProjectWriter` whose very first write (`replace_sub_projects`) fails — to verify `run` aborts on an
    /// early infrastructure error rather than swallowing it with `let _ =` and continuing on an empty graph.
    struct FailingReplaceSubProjects;
    impl ProjectWriter for FailingReplaceSubProjects {
        fn replace_sub_projects(
            &self,
            _: ProjectId,
            _: Vec<NewSubProject>,
        ) -> Result<Vec<SubProject>> {
            Err(DomainError::NotFound("replace_sub_projects failed".into()))
        }
        fn replace_files(&self, _: ProjectId, _: Vec<NewSourceFile>) -> Result<Vec<SourceFile>> {
            Ok(Vec::new())
        }
        fn set_sub_project_frameworks(&self, _: SubProjectId, _: Vec<String>) -> Result<()> {
            Ok(())
        }
        fn update_sub_project_facts(&self, _: SubProjectId, _: Value) -> Result<()> {
            Ok(())
        }
        fn create_project(&self, _: NewProject) -> Result<Project> {
            Err(DomainError::NotFound("unused".into()))
        }
        fn update_project(&self, _: ProjectId, _: ProjectPatch) -> Result<Project> {
            Err(DomainError::NotFound("unused".into()))
        }
        fn delete_project(&self, _: ProjectId) -> Result<()> {
            Ok(())
        }
        fn set_project_status(&self, _: ProjectId, _: ProjectStatus) -> Result<()> {
            Ok(())
        }
    }

    /// A `ProjectWriter` that accepts the P0 writes but fails the P3 `update_sub_project_facts` write — to verify
    /// a *late* infrastructure error (after Ingest) still aborts `run`, not just the flush `?`.
    struct FailingUpdateSubProjectFacts;
    impl ProjectWriter for FailingUpdateSubProjectFacts {
        fn replace_sub_projects(
            &self,
            _: ProjectId,
            _: Vec<NewSubProject>,
        ) -> Result<Vec<SubProject>> {
            Ok(vec![SubProject {
                id: SubProjectId::new(1),
                project_id: ProjectId(1),
                name: "app".into(),
                root_path: std::env::temp_dir(),
                language: Language::new("java"),
                role: "backend".into(),
                detected_by: "test".into(),
                frameworks: Vec::new(),
                facts: Value::Null,
            }])
        }
        fn replace_files(&self, _: ProjectId, _: Vec<NewSourceFile>) -> Result<Vec<SourceFile>> {
            Ok(Vec::new())
        }
        fn set_sub_project_frameworks(&self, _: SubProjectId, _: Vec<String>) -> Result<()> {
            Ok(())
        }
        fn update_sub_project_facts(&self, _: SubProjectId, _: Value) -> Result<()> {
            Err(DomainError::NotFound("update_sub_project_facts failed".into()))
        }
        fn create_project(&self, _: NewProject) -> Result<Project> {
            Err(DomainError::NotFound("unused".into()))
        }
        fn update_project(&self, _: ProjectId, _: ProjectPatch) -> Result<Project> {
            Err(DomainError::NotFound("unused".into()))
        }
        fn delete_project(&self, _: ProjectId) -> Result<()> {
            Ok(())
        }
        fn set_project_status(&self, _: ProjectId, _: ProjectStatus) -> Result<()> {
            Ok(())
        }
    }

    fn phases(outcome: &PipelineOutcome) -> Vec<String> {
        outcome.reports.iter().map(|r| r.phase.clone()).collect()
    }

    /// `full_pipeline: false` means "Ingest + CfAst only" — but Prepare still runs, because its framework
    /// detection is what the partial run is for.
    #[test]
    fn run_stops_after_prepare_for_a_partial_pipeline() {
        let infra = StubInfra::new(FakeSink::default());
        let obs = FakeObserver::default();

        let outcome = run(&project(false), &infra, &obs).unwrap();

        assert_eq!(
            phases(&outcome),
            vec![Phase::INGEST.to_string(), Phase::CF_AST.to_string(), Phase::PREPARE.to_string()],
            "a non-full pipeline must stop at P3"
        );
        assert_eq!(obs.starts(), phases(&outcome));
        assert_eq!(obs.ends(), phases(&outcome));
    }

    /// The full orchestration order, including `GuardCapability` riding immediately after Synthesize.
    #[test]
    fn run_walks_every_phase_in_order() {
        let infra = StubInfra::new(FakeSink::default());
        let obs = FakeObserver::default();

        let outcome = run(&project(true), &infra, &obs).unwrap();

        assert_eq!(
            phases(&outcome),
            vec![
                Phase::INGEST.to_string(),
                Phase::CF_AST.to_string(),
                Phase::PREPARE.to_string(),
                Phase::ANNOTATE_PRE.to_string(),
                Phase::SYNTHESIZE.to_string(),
                // Capabilities are guarded as soon as the semantic nodes exist.
                "GuardCapability".to_string(),
                Phase::ANNOTATE_POST.to_string(),
                Phase::RESOLVE.to_string(),
                Phase::PROPAGATE.to_string(),
                "Taint".to_string(),
                "Sign".to_string(),
                "External".to_string(),
                "Tx".to_string(),
                "Guard".to_string(),
            ]
        );
        // The observer is notified of every phase, start and end, in the same order.
        assert_eq!(obs.starts(), phases(&outcome));
        assert_eq!(obs.ends(), phases(&outcome));
    }

    /// A sink that rejects a phase aborts the whole run instead of continuing with an unpersisted graph.
    #[test]
    fn run_aborts_when_a_phase_cannot_be_persisted() {
        let infra = StubInfra::new(FakeSink {
            fail: true,
            ..Default::default()
        });

        let res = run(&project(true), &infra, &FakeObserver::default());

        assert!(res.is_err(), "a persistence failure must abort the pipeline");
    }

    /// A new build must start from a wiped graph, so `run` issues a one-off `reset_project` delta right after
    /// Ingest and *before* the first phase flush. It is a **separate** `apply` (never folded into the Ingest
    /// flush), otherwise the reset would wipe the diagnostics Ingest just wrote.
    #[test]
    fn run_issues_a_reset_project_delta_before_the_first_phase() {
        let infra = StubInfra::new(FakeSink::default());
        run(&project(true), &infra, &FakeObserver::default()).unwrap();

        let applied = infra.graph.applied();
        assert!(applied.len() >= 2, "there must be at least two applies: the Ingest flush and the reset");
        // Exactly one delta carries the reset flag.
        let resets: Vec<usize> = applied
            .iter()
            .enumerate()
            .filter(|(_, d)| d.5)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(resets.len(), 1, "reset_project must be emitted exactly once");
        // …and it sits right after the Ingest flush, ahead of every phase flush.
        assert_eq!(resets[0], 1, "the reset follows the Ingest flush immediately, before any phase flush");
        // The Ingest flush itself (and all later phase flushes) must not carry the flag.
        assert!(!applied[0].5, "the Ingest flush must not carry a reset");
        for (i, d) in applied.iter().enumerate() {
            if i != resets[0] {
                assert!(!d.5, "nothing but the reset delta may carry a reset");
            }
        }
    }

    /// `run` must abort when an infrastructure write before the first phase fails — the `?` on
    /// `replace_sub_projects` (line 86) is load-bearing. Swallowing it (e.g. `let _ = ...`) would let the run
    /// continue on an empty graph, and the existing phase-order / reset tests would still pass, so this pins it.
    #[test]
    fn run_aborts_on_an_early_project_write_failure() {
        let infra =
            StubInfra::with_projects(FakeSink::default(), Box::new(FailingReplaceSubProjects));
        let res = run(&project(true), &infra, &FakeObserver::default());
        assert!(
            res.is_err(),
            "an early infrastructure-write failure must abort the pipeline"
        );
    }

    /// The same abort contract holds for a *late* write that only runs after Ingest
    /// (`update_sub_project_facts`, line 166) — reachable in both partial and full pipelines. Its `?` is a
    /// different site from the flush `?` already covered and from the early write above, so a silent `let _ =`
    /// there would otherwise slip through every existing test.
    #[test]
    fn run_aborts_on_a_late_project_write_failure() {
        let infra =
            StubInfra::with_projects(FakeSink::default(), Box::new(FailingUpdateSubProjectFacts));
        let res = run(&project(true), &infra, &FakeObserver::default());
        assert!(
            res.is_err(),
            "a late infrastructure-write failure must abort the pipeline"
        );
    }
}
