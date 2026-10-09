//! Routes and handlers.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, Query, State},
    response::Html,
    routing::{delete, get, post, put},
    Json, Router,
};
use tower_http::services::{ServeDir, ServeFile};
use serde::{Deserialize, Serialize};

use gt_application::{
    pipeline_runner::{PipelineDeps, PipelineService},
    project_service::ProgressObserver,
    GraphQueryService, ProjectService, RecallQuery, RecallService, RuleService, ViewService,
};
use gt_domain::error::DomainError;
use gt_domain::model::{
    CheckRule, NodeId, ProjectId, ProjectRuleConfig, RuleConfigPatch, SubProjectId, Violation,
};
use gt_domain::port::{
    EdgeDirection, ParserRegistry, Persistence, RuleProvider, ViewRegistryProvider,
};

use crate::dto::{
    ApiResponse, CreateProjectRequest, DirEntryDto, HealthDto, ProjectDto, RunAcceptedDto,
    RunStatusDto, SubProjectDto, UpdateProjectRequest,
};

/// State shared by all handlers (injected by the composition root).
pub struct AppState {
    pub projects: Arc<ProjectService>,
    pub pipeline: Arc<PipelineService>,
    pub graphs: Arc<GraphQueryService>,
    pub views: Arc<ViewService>,
    pub checks: Arc<RuleService>,
    pub recall: Arc<RecallService>,
    pub parsers: Arc<dyn ParserRegistry>,
    pub frameworks: usize,
    /// The most recent graph-build progress (project id -> observer).
    pub progress: Mutex<HashMap<i64, Arc<ProgressObserver>>>,
    /// Shared node-vector cache (shared with the encoder inside `recall`); cleared when the graph is rebuilt.
    pub node_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
    /// Live state of a (possibly running) model download, surfaced to the one-click download UI.
    pub model_download: Arc<Mutex<crate::model_manager::DownloadState>>,
}

pub type Shared = Arc<AppState>;

/// Convenience constructor: assemble HTTP state from the services.
pub fn state(
    store: Arc<dyn Persistence>,
    deps: Arc<PipelineDeps>,
    frameworks: usize,
    view_registry: Arc<dyn ViewRegistryProvider>,
    rules: Arc<dyn RuleProvider>,
) -> Shared {
    let projects = Arc::new(ProjectService::new(
        Arc::clone(&store),
        Arc::new(gt_domain::port::SystemClock),
    ));
    // A compliance check runs automatically after the build, so the pipeline also needs the rule set.
    let pipeline = Arc::new(PipelineService::new(
        Arc::clone(&store),
        Arc::clone(&deps),
        Arc::clone(&rules),
    ));
    let graphs = Arc::new(GraphQueryService::new(Arc::clone(&store)));
    let views = Arc::new(ViewService::new(Arc::clone(&store), view_registry));
    let checks = Arc::new(RuleService::new(Arc::clone(&store), rules));
    // Recall: prefer the real bge-m3 (when weights are available), otherwise fall back to the default hash encoder; the node-vector cache is reused across recalls.
    let node_cache = Arc::new(Mutex::new(HashMap::<u64, Vec<f32>>::new()));
    let recall = Arc::new(
        RecallService::with_embedder_and_cache(
            Arc::clone(&store),
            Arc::clone(&deps.fs),
            Arc::clone(&deps.scanner),
            Some(gt_application::resolve_recall_embedder()),
            Arc::clone(&node_cache),
            Some(std::path::PathBuf::from("data/embeddings")),
            Some(std::path::PathBuf::from("data/snapshots")),
        )
        // The production HTTP entry starts a background async warm-up: the first recall returns immediately with the
        // fast encoder (never blocking the UI) while a thread computes the bge vectors and persists them; once done
        // the project switches to the semantic path automatically.
        .with_async_warmup(),
    );
    // Apply a persisted backend-mode choice (local / url / hash) so a mode switch survives restart.
    // `auto` (or no saved config) is already handled by the `resolve_recall_embedder()` above.
    let saved = crate::model_manager::load_model_config();
    if !saved.mode.is_empty() && saved.mode != "auto" {
        let (emb, _info, _dim) =
            gt_application::embedding::resolve_backend(&saved.mode, saved.url.clone());
        recall.set_semantic_embedder(emb);
    }
    let state = Arc::new(AppState {
        projects,
        pipeline,
        graphs,
        views,
        checks,
        recall,
        parsers: Arc::clone(&deps.parsers),
        frameworks,
        progress: Mutex::new(HashMap::new()),
        node_cache,
        model_download: Arc::new(Mutex::new(Default::default())),
    });
    // Start source-change watching (P2 v1): poll + debounce -> safe whole-database rebuild + automatic compliance + clear the recall cache.
    // Started only for existing projects; a newly created project starts it separately in the create_project handler.
    if let Ok(list) = state.projects.list() {
        for p in list {
            let root = std::path::PathBuf::from(&p.root_path);
            if root.is_dir() {
                gt_application::watch::watch_project(
                    p.id,
                    root,
                    Arc::clone(&state.pipeline),
                    Arc::clone(&state.recall),
                );
            }
        }
    }
    state
}

/// Assemble every route.
///
/// When `ui_dir` is `Some` and points at a directory containing `index.html`, the backend also **hosts the built
/// React SPA**: `tower_http::ServeDir` acts as the fallback (a static hit returns the file directly, e.g.
/// `/assets/*`; a miss falls back to `index.html` to support front-end routing), and the root path `/` is given to
/// the SPA. `/compose` then remains the embedded standalone "prompt augmentation" page. With `ui_dir` = `None`
/// (the development default) no front end is hosted and the root falls back to `/compose`, as it always has.
pub fn build_router(state: Shared, ui_dir: Option<std::path::PathBuf>) -> Router {
    let router = Router::new()
        .route("/api/health", get(health))
        // Server status (including the active embedding backend, shown read-only in the UI)
        .route("/api/server/status", get(server_status))
        // Project CRUD
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project).put(update_project).delete(delete_project),
        )
        // Graph building
        .route("/api/projects/{id}/run", post(run_pipeline))
        .route("/api/projects/{id}/run/status", get(run_status))
        // Graph queries
        .route("/api/projects/{id}/sub-projects", get(list_sub_projects))
        .route("/api/projects/{id}/stats", get(stats))
        .route("/api/projects/{id}/nodes", get(query_nodes))
        .route("/api/projects/{id}/diagnostics", get(diagnostics))
        .route("/api/projects/{id}/diagnostics/summary", get(diagnostics_summary))
        // Filesystem browsing (for the directory picker; under WSL it can reach mount paths such as /mnt/c)
        .route("/api/fs/browse", get(browse_fs))
        .route("/api/nodes/{id}", get(get_node))
        .route("/api/nodes/{id}/annotations", get(node_annotations))
        .route("/api/nodes/{id}/neighbors", get(neighbors))
        .route("/api/nodes/{id}/subgraph", get(subgraph))
        .route("/api/symbols/{table}", get(symbols))
        .route("/api/projects/{id}/perspectives", get(list_perspectives))
        .route("/api/projects/{id}/view/{perspective}", get(object_view))
        .route("/api/projects/{id}/view/{perspective}/candidates", get(view_candidates))
        .route("/api/projects/{id}/aggregate/{perspective}", get(aggregate_view))
        .route("/api/nodes/{id}/locations", get(node_locations))
        .route("/api/edges/{id}/evidence", get(edge_evidence))
        // Compliance check (rules -> violations)
        .route("/api/rules", get(list_rules))
        .route("/api/projects/{id}/check", post(run_check))
        .route("/api/projects/{id}/violations", get(list_violations))
        .route("/api/projects/{id}/check/summary", get(check_summary))
        // Project-level rule config (per-project override of enabled state and parameters)
        .route("/api/projects/{id}/rules/config", get(list_rule_configs))
        .route("/api/projects/{id}/rules/config", put(put_rule_config))
        .route("/api/projects/{id}/rules/config/batch", post(batch_rule_config))
        .route(
            "/api/projects/{id}/rules/config/{rule_id}",
            delete(reset_rule_config),
        )
        // Prompt augmentation · recall (prompt -> related code)
        .route("/api/projects/{id}/recall", get(recall_get).post(recall_post))
        // Background warm-up progress (bge semantic vectors): lets the IDE / MCP know whether recall is still on the cold path.
        .route("/api/projects/{id}/warmup", get(project_warmup))
        // Prompt augmentation · compose: recall context + user intent -> a complete prompt to paste into an LLM
        .route("/api/projects/{id}/prompt", post(compose_prompt))
        // Model management: status + one-click download + backend-mode switch (no restart needed)
        .route("/api/models/status", get(models_status))
        .route("/api/models/download", post(models_download))
        .route("/api/server/backend", put(set_backend))
        // The prompt augmentation page (a self-contained static page, embedded in the binary, no extra static hosting)
        .route("/compose", get(compose_page));

    // A production deployment can have the backend host the built React SPA: when `ui_dir` points at a directory
    // containing index.html, ServeDir acts as the fallback (a file hit returns it directly, a miss falls back to
    // index.html for front-end routing) and the root `/` goes to the SPA; otherwise the root still falls back to
    // the embedded compose page. Development passes no ui_dir, so behaviour is unchanged.
    let router = if let Some(ui_dir) = ui_dir {
        if ui_dir.join("index.html").is_file() {
            router.fallback_service(
                ServeDir::new(ui_dir.clone()).fallback(ServeFile::new(ui_dir.join("index.html"))),
            )
        } else {
            // The directory exists but has no build output: the root still falls back to the embedded compose page instead of crashing.
            router.route("/", get(compose_page))
        }
    } else {
        router.route("/", get(compose_page))
    };

    router.with_state(state)
}

// ---------------------------------------------------------------- handlers

/// Detect whether the backend process runs inside WSL.
///
/// WSL kernel release information (`/proc/sys/kernel/osrelease` and `/proc/version`) contains the string
/// "microsoft", which bare Linux / Docker / remote VMs never do. The distro name is hard to derive from kernel
/// information, so it defaults to `Ubuntu` (the name of the overwhelming majority of default distros); the front
/// end can still override it manually.
fn detect_wsl() -> (bool, String) {
    let check = |path: &str| {
        fs::read_to_string(path)
            .map(|s| s.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
    };
    if check("/proc/sys/kernel/osrelease") || check("/proc/version") {
        (true, "Ubuntu".to_string())
    } else {
        (false, String::new())
    }
}

async fn health(State(state): State<Shared>) -> Json<ApiResponse<HealthDto>> {
    let (is_wsl, wsl_distro) = detect_wsl();
    Json(ApiResponse::success(HealthDto {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        languages: state
            .parsers
            .supported_languages()
            .into_iter()
            .map(|l| l.to_string())
            .collect(),
        frameworks: state.frameworks,
        is_wsl,
        wsl_distro,
    }))
}

async fn models_status(
    State(state): State<Shared>,
) -> Json<ApiResponse<crate::model_manager::ModelStatusDto>> {
    Json(crate::model_manager::model_status(
        &state.recall,
        &state.model_download,
    ))
}

async fn models_download(
    State(state): State<Shared>,
) -> Json<ApiResponse<crate::model_manager::ModelStatusDto>> {
    Json(crate::model_manager::start_model_download(
        &state.recall,
        &state.model_download,
    ))
}

async fn set_backend(
    State(state): State<Shared>,
    Json(req): Json<crate::model_manager::SetBackendRequest>,
) -> Json<ApiResponse<crate::model_manager::BackendStatus>> {
    Json(crate::model_manager::apply_backend_mode(&state.recall, &req))
}

async fn list_projects(State(state): State<Shared>) -> Json<ApiResponse<Vec<ProjectDto>>> {
    match state.projects.list() {
        Ok(list) => Json(ApiResponse::success(list.into_iter().map(Into::into).collect())),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn create_project(
    State(state): State<Shared>,
    Json(req): Json<CreateProjectRequest>,
) -> Json<ApiResponse<ProjectDto>> {
    let result = state.projects.create(gt_domain::model::NewProject {
        name: req.name,
        root_path: req.root_path.into(),
        description: req.description,
        config: req.config,
    });
    match result {
        Ok(p) => {
            // Start graph building automatically after creation
            let _ = trigger_run(&state, p.id);
            // A new project also starts change watching
            let root = std::path::PathBuf::from(&p.root_path);
            if root.is_dir() {
                gt_application::watch::watch_project(
                    p.id,
                    root,
                    Arc::clone(&state.pipeline),
                    Arc::clone(&state.recall),
                );
            }
            Json(ApiResponse::success(p.into()))
        }
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn get_project(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<ProjectDto>> {
    match state.projects.get(ProjectId(id)) {
        Ok(p) => Json(ApiResponse::success(p.into())),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn update_project(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateProjectRequest>,
) -> Json<ApiResponse<ProjectDto>> {
    match state.projects.update(ProjectId(id), req.into_patch()) {
        Ok(p) => Json(ApiResponse::success(p.into())),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn delete_project(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<String>> {
    match state.projects.delete(ProjectId(id)) {
        Ok(()) => Json(ApiResponse::success("deleted".to_string())),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn run_pipeline(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<RunAcceptedDto>> {
    match trigger_run(&state, ProjectId(id)) {
        Ok(()) => Json(ApiResponse::success(RunAcceptedDto { project_id: id, accepted: true })),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Trigger a background graph build.
fn trigger_run(state: &Shared, id: ProjectId) -> Result<(), DomainError> {
    let observer = Arc::new(ProgressObserver::new(id));
    state
        .progress
        .lock()
        .unwrap()
        .insert(id.get(), Arc::clone(&observer));
    // The graph is about to be rebuilt and old node vectors may be stale, so clear the recall cache first (the next recall warms up again).
    state.node_cache.lock().unwrap().clear();
    state.pipeline.spawn(id, observer)
}

async fn run_status(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<RunStatusDto>> {
    let project = state.projects.get(ProjectId(id));
    let status = match &project {
        Ok(p) => p.status.to_string(),
        Err(_) => "unknown".to_string(),
    };
    let mut dto = RunStatusDto {
        project_id: id,
        status,
        ..Default::default()
    };
    if let Some(obs) = state.progress.lock().unwrap().get(&id) {
        let snap = obs.snapshot();
        dto.current_phase = snap.current_phase;
        dto.phases = snap
            .reports
            .into_iter()
            .map(|r| crate::dto::PhaseReportDto {
                phase: r.phase,
                nodes_created: r.nodes_created,
                edges_created: r.edges_created,
                annotations_created: r.annotations_created,
                aliases_created: r.aliases_created,
                duration_ms: r.duration_ms,
                diagnostics: r.diagnostics.len(),
            })
            .collect();
    }
    Json(ApiResponse::success(dto))
}

async fn list_sub_projects(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Vec<SubProjectDto>>> {
    match state.projects.sub_projects(ProjectId(id)) {
        Ok(v) => Json(ApiResponse::success(v.into_iter().map(Into::into).collect())),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn stats(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<gt_domain::port::GraphStats>> {
    match state.graphs.stats(ProjectId(id)) {
        Ok(s) => Json(ApiResponse::success(s)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct NodeQuery {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

async fn query_nodes(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<NodeQuery>,
) -> Json<ApiResponse<Vec<gt_domain::model::Node>>> {
    match state.graphs.nodes(
        ProjectId(id),
        q.kind.as_deref(),
        q.name.as_deref(),
        q.limit.unwrap_or(100),
        q.offset.unwrap_or(0),
    ) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn get_node(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Option<gt_domain::model::Node>>> {
    match state.graphs.node(NodeId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn node_annotations(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Vec<gt_domain::model::Annotation>>> {
    match state.graphs.annotations(NodeId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct NeighborQuery {
    pub direction: Option<String>,
}

async fn neighbors(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<NeighborQuery>,
) -> Json<ApiResponse<Vec<gt_domain::model::Edge>>> {
    let dir = match q.direction.as_deref() {
        Some("in") => EdgeDirection::Incoming,
        Some("out") => EdgeDirection::Outgoing,
        _ => EdgeDirection::Both,
    };
    match state.graphs.neighbors(NodeId(id), dir) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct SubgraphQuery {
    pub depth: Option<u32>,
    pub max_nodes: Option<usize>,
}

async fn subgraph(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<SubgraphQuery>,
) -> Json<ApiResponse<gt_application::graph_query::Subgraph>> {
    match state
        .graphs
        .subgraph(NodeId(id), q.depth.unwrap_or(2), q.max_nodes.unwrap_or(300))
    {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct SymbolQuery {
    pub project_id: Option<i64>,
}

async fn symbols(
    State(state): State<Shared>,
    Path(table): Path<String>,
    Query(q): Query<SymbolQuery>,
) -> Json<ApiResponse<Vec<gt_domain::model::SymbolEntry>>> {
    let pid = ProjectId(q.project_id.unwrap_or(1));
    match state.graphs.symbols(pid, &table) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Read cap for the diagnostics list.
///
/// It must not be small: the diagnostics page **groups by problem type** (per-type counts, sample locations,
/// expandable lists), and grouping is only correct when it sees the full set — sample_project alone has 445 build
/// diagnostics for one project, so a smaller cap would list only the most recently written rows
/// (`ORDER BY id DESC`), showing the user "write order" rather than "how many problems", and the per-type
/// counts would be wrong too.
///
/// Same order of magnitude as the compliance page's `STORED_LIMIT` (5000): this is the "full fetch before
/// pagination"; the page paginates itself.
const DIAGNOSTIC_LIMIT: u32 = 5000;

async fn diagnostics(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Vec<gt_domain::model::Diagnostic>>> {
    match state.graphs.diagnostics(ProjectId(id), DIAGNOSTIC_LIMIT) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Severity rollup for non-rule diagnostics (menu badge; excludes the `rule:` prefix so compliance checks are not double-counted).
async fn diagnostics_summary(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<gt_application::DiagnosticSummary>> {
    match state.graphs.diagnostics_summary(ProjectId(id)) {
        Ok(s) => Json(ApiResponse::success(s)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}


async fn list_perspectives(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Vec<serde_json::Value>>> {
    match state.views.perspectives(ProjectId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct ViewQuery {
    pub node: Option<i64>,
    pub depth: Option<u32>,
    pub limit: Option<u32>,
    pub name_contains: Option<String>,
    pub sub_project_id: Option<i64>,
}

async fn view_candidates(
    State(state): State<Shared>,
    Path((id, perspective)): Path<(i64, String)>,
    Query(q): Query<ViewQuery>,
) -> Json<ApiResponse<Vec<gt_domain::model::Candidate>>> {
    match state
        .views
        .candidates(
            ProjectId(id),
            &perspective,
            q.limit.unwrap_or(300),
            q.name_contains.as_deref(),
            q.sub_project_id.map(SubProjectId),
        )
    {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn object_view(
    State(state): State<Shared>,
    Path((id, perspective)): Path<(i64, String)>,
    Query(q): Query<ViewQuery>,
) -> Json<ApiResponse<gt_domain::model::ObjectView>> {
    let Some(node) = q.node else {
        return Json(ApiResponse::failure("missing node parameter"));
    };
    match state
        .views
        .object_view(ProjectId(id), &perspective, NodeId(node), q.depth)
    {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn aggregate_view(
    State(state): State<Shared>,
    Path((id, perspective)): Path<(i64, String)>,
    Query(q): Query<ViewQuery>,
) -> Json<ApiResponse<gt_domain::model::AggregateView>> {
    match state
        .views
        .aggregate_view(ProjectId(id), &perspective, q.limit.unwrap_or(12))
    {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn node_locations(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<gt_domain::model::NodeLocations>> {
    match state.views.node_locations(NodeId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn edge_evidence(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Option<gt_domain::model::EdgeEvidence>>> {
    match state.views.edge_evidence(id) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

// ---------------------------------------------------------------- compliance check

/// List every loaded rule (so the UI can show "what can be checked").
async fn list_rules(State(state): State<Shared>) -> Json<ApiResponse<Vec<CheckRule>>> {
    Json(ApiResponse::success(state.checks.rules()))
}

/// Check request body; an empty `rule_ids` means run every enabled rule.
#[derive(Debug, Deserialize, Default)]
pub struct CheckRequest {
    pub rule_ids: Option<Vec<String>>,
}

async fn run_check(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    body: Option<Json<CheckRequest>>,
) -> Json<ApiResponse<gt_application::CheckReport>> {
    let only = body.and_then(|Json(b)| b.rule_ids);
    match state.checks.check(ProjectId(id), only.as_deref(), true) {
        Ok(r) => Json(ApiResponse::success(r)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct ViolationQuery {
    pub limit: Option<u32>,
    /// Comma-separated sub-project ids; empty means no filtering (return every sub-project).
    pub sub_project_id: Option<String>,
}

/// Parse `?sub_project_id=1,2` into `Option<Vec<SubProjectId>>`; an empty string means no filtering.
fn parse_sub_project_ids(raw: &Option<String>) -> Option<Vec<SubProjectId>> {
    raw.as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.split(',')
                .filter_map(|p| p.trim().parse::<i64>().ok())
                .map(SubProjectId)
                .collect()
        })
}

/// Read the violations persisted by the previous check (without re-running rules).
/// The **default count** for reading persisted violations.
///
/// One measured check produced 996 violations, so the cap must clear that: at 500, "just ran,
/// see 996 / 59 critical" and "refresh, see 500 / 0 critical" disagree, and are read as
/// "nothing was persisted / back to old data".
/// Reading the violation table is a full fetch before pagination (the UI pages 20 at a time itself), so this is
/// only an **upper-bound guard**, not a pagination parameter; hence an obviously generous value. When it really is
/// exceeded, the ordering already guarantees the least severe are the ones evicted.
const DEFAULT_VIOLATION_LIMIT: u32 = 5_000;

async fn list_violations(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<ViolationQuery>,
) -> Json<ApiResponse<Vec<Violation>>> {
    let sub = parse_sub_project_ids(&q.sub_project_id);
    match state
        .checks
        .violations(ProjectId(id), q.limit.unwrap_or(DEFAULT_VIOLATION_LIMIT), sub.as_deref())
    {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Compliance-check severity rollup (menu badge; does not re-run rules).
async fn check_summary(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<ViolationQuery>,
) -> Json<ApiResponse<gt_application::CheckSummary>> {
    let sub = parse_sub_project_ids(&q.sub_project_id);
    match state.checks.summary(ProjectId(id), sub.as_deref()) {
        Ok(s) => Json(ApiResponse::success(s)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Batch-write request body for project-level rule config.
#[derive(Debug, Deserialize)]
pub struct RuleConfigBatch {
    pub items: Vec<RuleConfigPatch>,
}

/// List every rule config override for a project.
async fn list_rule_configs(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<std::collections::HashMap<String, ProjectRuleConfig>>> {
    match state.checks.rule_configs(ProjectId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Write config for a single rule (fields may be omitted = leave that item unchanged).
async fn put_rule_config(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(patch): Json<RuleConfigPatch>,
) -> Json<ApiResponse<bool>> {
    // Returns `bool` rather than `()`: `()` serializes to `null`, and the front end's unified response parsing treats
    // `data: null` as a failure (it cannot distinguish "no data" from "an error happened").
    match state.checks.apply_rule_config(ProjectId(id), patch) {
        Ok(_) => Json(ApiResponse::success(true)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Reset a rule's project override (falling back to the YAML global default).
async fn reset_rule_config(
    State(state): State<Shared>,
    Path((id, rule_id)): Path<(i64, String)>,
) -> Json<ApiResponse<bool>> {
    match state.checks.reset_rule_config(ProjectId(id), &rule_id) {
        Ok(_) => Json(ApiResponse::success(true)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Batch-write rule config (used when enabling / disabling a whole group or category).
async fn batch_rule_config(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(body): Json<RuleConfigBatch>,
) -> Json<ApiResponse<bool>> {
    match state.checks.batch_rule_config(ProjectId(id), body.items) {
        Ok(_) => Json(ApiResponse::success(true)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

// ---------------------------------------------------------- prompt augmentation · recall

/// Recall parameters in GET form (for simple UI calls).
#[derive(Debug, Deserialize)]
pub struct RecallGetQuery {
    pub q: String,
    pub limit: Option<usize>,
    pub hops: Option<u32>,
    /// Comma-separated node kinds, e.g. `Table,HttpContract`.
    pub kinds: Option<String>,
    pub snippets: Option<bool>,
    /// Whether to also append the full source of the hit files (see `RecallQuery::include_body`).
    pub include_body: Option<bool>,
}

impl RecallGetQuery {
    fn into_query(self) -> RecallQuery {
        RecallQuery {
            query: self.q,
            limit: self.limit.unwrap_or(10),
            hops: self.hops.unwrap_or(2),
            kinds: self
                .kinds
                .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
                .unwrap_or_default(),
            with_snippets: self.snippets.unwrap_or(true),
            include_body: self.include_body.unwrap_or(false),
        }
    }
}

async fn recall_get(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<RecallGetQuery>,
) -> Json<ApiResponse<gt_application::RecallResult>> {
    match state.recall.recall(ProjectId(id), &q.into_query()) {
        Ok(r) => Json(ApiResponse::success(r)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn recall_post(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(req): Json<RecallQuery>,
) -> Json<ApiResponse<gt_application::RecallResult>> {
    match state.recall.recall(ProjectId(id), &req) {
        Ok(r) => Json(ApiResponse::success(r)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

// ---------------------------------------------------------- prompt augmentation · compose

/// Prompt-augmentation request: recall parameters + the user's task intent.
#[derive(Debug, Deserialize)]
pub struct ComposePromptRequest {
    /// Search terms used to recall code (natural language mixed with identifiers is fine).
    pub query: String,
    /// The user's task intent / extra context; when omitted the prompt asks the LLM to infer it from the context.
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub hops: Option<u32>,
    #[serde(default)]
    pub with_snippets: Option<bool>,
}

/// Prompt-augmentation result.
#[derive(Debug, Serialize)]
pub struct ComposePromptResult {
    /// The complete prompt, ready to paste into an LLM.
    pub prompt: String,
    /// The raw recall context (markdown), so the user can trim it.
    pub markdown: String,
    pub seed_count: usize,
    pub hit_count: usize,
    /// Rough token estimate for the prompt (different factors for Chinese and English).
    pub approx_tokens: usize,
    /// The **complete recall result** (seeds / hits / parsed query terms / quality tier).
    ///
    /// Why the whole recall result is carried along: the Web "prompt augmentation" page shows both the
    /// "composed prompt" and the "hit list" (to check recall quality and click a feature term to re-run).
    /// Without it the front end would have to call `/recall` again — and recall is the most expensive step here,
    /// so running it twice for one generation is pure waste, and the two results may disagree (the graph can be
    /// rebuilt in between).
    pub recall: gt_application::RecallResult,
}

/// Compose a prompt: graph recall context + user intent -> a prompt to feed straight to an LLM.
///
/// Design point: recall only supplies the "map" (paths / line numbers / snippets), and the prompt explicitly asks
/// the LLM to read the full file by path when needed — saving the discovery tokens without losing implementation
/// completeness.
async fn compose_prompt(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(req): Json<ComposePromptRequest>,
) -> Json<ApiResponse<ComposePromptResult>> {
    let rq = RecallQuery {
        query: req.query.clone(),
        limit: req.limit.unwrap_or(10),
        hops: req.hops.unwrap_or(2),
        kinds: Vec::new(),
        with_snippets: req.with_snippets.unwrap_or(true),
        include_body: false,
    };
    match state.recall.recall(ProjectId(id), &rq) {
        Ok(r) => {
            let prompt = compose_prompt_text(&req.query, req.intent.as_deref(), &r.markdown);
            let approx_tokens = estimate_tokens(&prompt);
            let seed_count = r.seeds.len();
            let hit_count = r.hits.len();
            Json(ApiResponse::success(ComposePromptResult {
                prompt,
                markdown: r.markdown.clone(),
                seed_count,
                hit_count,
                approx_tokens,
                recall: r,
            }))
        }
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// Prompt template: evidence first (the recall context), then the task, quality constraints last.
fn compose_prompt_text(query: &str, intent: Option<&str>, markdown: &str) -> String {
    let intent_text = intent
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "(not provided; please infer the task goal from the code context below)".to_string());

    format!(
        "You are a senior software engineer. Below is the [RELEVANT CODE CONTEXT] retrieved from a code knowledge graph for this task:\
the most relevant files, symbols and call / data relations have been selected by relevance, with file paths, line numbers and key snippets.\n\n\
========== RELEVANT CODE CONTEXT (graph recall) ==========\n\
{markdown}\n\
==============================================\n\n\
[TASK]\n\
{intent}\n\n\
(Recall query used to build this context: {query})\n\n\
[REQUIREMENTS]\n\
1. Reuse the file, class, function and field names from the context; do not invent interfaces or fields that are absent from it.\n\
2. The above is only a \"map\": snippets are for locating. For full implementation details, read the exact file at the given path and line number.\n\
3. Keep the change scoped to task-relevant files and make it minimal and reviewable.\n\
4. If the context is insufficient, say explicitly what is missing instead of filling the gaps by imagination.\n",
        markdown = markdown,
        intent = intent_text,
        query = query
    )
}

/// Rough token count: ASCII (code / English) is about 4 characters per token, Chinese about 1.5 characters per token.
fn estimate_tokens(s: &str) -> usize {
    let mut ascii = 0usize;
    let mut cjk = 0usize;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            cjk += 1;
        }
    }
    (ascii as f64 / 4.0 + cjk as f64 / 1.5).ceil() as usize
}

/// The prompt augmentation page: an embedded static page, so no extra static hosting or front-end build is needed.
async fn compose_page() -> Html<&'static str> {
    Html(include_str!("compose.html"))
}

/// Server status: exposes the active embedding backend and dimension for read-only display in the UI.
#[derive(Serialize)]
pub struct ServerStatusDto {
    pub version: String,
    pub embedding_backend: String,
    pub embedding_dim: usize,
}

async fn server_status() -> Json<ApiResponse<ServerStatusDto>> {
    Json(ApiResponse::success(ServerStatusDto {
        version: env!("CARGO_PKG_VERSION").to_string(),
        embedding_backend: gt_application::embedding::embedding_backend_info(),
        embedding_dim: gt_application::embedding::embedding_dim(),
    }))
}

/// Background warm-up progress: whether the bge semantic vectors are ready / how far warm-up has got.
#[derive(Debug, Clone, Serialize)]
pub struct WarmupStatusDto {
    /// Whether this project's semantic vectors are all ready (recall uses the full semantic path).
    pub warmed: bool,
    /// Whether a background warm-up is running (recall is temporarily on the cold path).
    pub warming: bool,
    /// Number of nodes already encoded (meaningful only while `warming`).
    pub done: usize,
    /// Total number of nodes to encode (meaningful only while `warming`).
    pub total: usize,
}

/// Query the background warm-up progress of a project.
async fn project_warmup(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<WarmupStatusDto>> {
    let st = state.recall.warmup_progress(id);
    Json(ApiResponse::success(WarmupStatusDto {
        warmed: st.map(|s| s.warmed).unwrap_or(false),
        warming: st.map(|s| s.warming).unwrap_or(false),
        done: st.map(|s| s.done).unwrap_or(0),
        total: st.map(|s| s.total).unwrap_or(0),
    }))
}

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    pub path: Option<String>,
}

/// Browse a directory: return the list of sub-directories under that path (for the front-end directory picker).
///
/// The backend process runs on the host system (including WSL), so mount paths such as `/mnt/c` work naturally.
/// An empty path falls back to `/home` (if it exists) or `/`.
async fn browse_fs(State(_state): State<Shared>, Query(q): Query<BrowseQuery>) -> Json<ApiResponse<Vec<DirEntryDto>>> {
    let raw = q.path.filter(|s| !s.trim().is_empty());
    let candidate = raw.unwrap_or_else(|| {
        if std::path::Path::new("/home").is_dir() {
            "/home".to_string()
        } else {
            "/".to_string()
        }
    });
    let path = std::path::Path::new(&candidate);
    match fs::read_dir(path) {
        Ok(rd) => {
            let mut dirs: Vec<DirEntryDto> = Vec::new();
            for entry in rd.flatten() {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !meta.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name == "." || name == ".." {
                    continue;
                }
                let full = entry.path().to_string_lossy().to_string();
                dirs.push(DirEntryDto {
                    name,
                    path: full,
                    is_dir: true,
                });
            }
            dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            Json(ApiResponse::success(dirs))
        }
        Err(e) => Json(ApiResponse::failure(format!(
            "Cannot read directory '{}': {}",
            candidate, e
        ))),
    }
}

/// Reserved: filter files by sub-project.
#[allow(dead_code)]
fn _sub(id: SubProjectId) -> SubProjectId {
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?sub_project_id=1,2` parses to a list; empty / missing means "no filtering" (None).
    #[test]
    fn parse_sub_project_ids_handles_empty_and_csv() {
        assert_eq!(parse_sub_project_ids(&None), None);
        assert_eq!(parse_sub_project_ids(&Some(String::new())), None);
        assert_eq!(
            parse_sub_project_ids(&Some("1,2,3".to_string())),
            Some(vec![SubProjectId(1), SubProjectId(2), SubProjectId(3)])
        );
        // whitespace tolerance; non-numeric tokens are skipped
        assert_eq!(
            parse_sub_project_ids(&Some(" 4 , 5 , x".to_string())),
            Some(vec![SubProjectId(4), SubProjectId(5)])
        );
    }

    /// The GET recall path must agree with the `RecallQuery` serde default (limit 10, hops 2), not an old 20.
    #[test]
    fn recall_get_query_applies_contract_defaults() {
        let q = RecallGetQuery {
            q: "find the order service".to_string(),
            limit: None,
            hops: None,
            kinds: None,
            snippets: None,
            include_body: None,
        };
        let rq = q.into_query();
        assert_eq!(rq.limit, 10, "the GET recall default limit must match the RecallQuery contract (10)");
        assert_eq!(rq.hops, 2);
        assert!(rq.kinds.is_empty());
        assert!(rq.with_snippets);
        assert!(!rq.include_body);
    }

    #[test]
    fn recall_get_query_splits_kinds_and_overrides() {
        let q = RecallGetQuery {
            q: "q".to_string(),
            limit: Some(5),
            hops: Some(3),
            kinds: Some("Table,HttpContract".to_string()),
            snippets: Some(false),
            include_body: Some(true),
        };
        let rq = q.into_query();
        assert_eq!(rq.limit, 5);
        assert_eq!(rq.hops, 3);
        assert_eq!(rq.kinds, vec!["Table".to_string(), "HttpContract".to_string()]);
        assert!(!rq.with_snippets);
        assert!(rq.include_body);
    }

    /// Token estimate: ASCII ~4 chars/token, CJK ~1.5 chars/token, rounded up.
    #[test]
    fn estimate_tokens_counts_ascii_and_cjk() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("中文"), 2);
        assert_eq!(estimate_tokens("ab中文"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }

    /// The composed prompt must carry the context, the (possibly inferred) task, and the requirements block.
    #[test]
    fn compose_prompt_text_contains_context_and_task() {
        let text = compose_prompt_text("my query", Some("do X"), "# the context");
        assert!(text.contains("RELEVANT CODE CONTEXT"));
        assert!(text.contains("# the context"));
        assert!(text.contains("do X"));
        assert!(text.contains("[TASK]"));

        // Omitted intent falls back to the "infer the task" placeholder.
        let inferred = compose_prompt_text("q", None, "md");
        assert!(inferred.contains("not provided"));
    }
}
