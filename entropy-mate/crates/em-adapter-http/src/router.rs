//! 路由与处理器。

use std::fs;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use em_application::{
    pipeline_runner::{PipelineDeps, PipelineService},
    project_service::ProgressObserver,
    GraphQueryService, ProjectService, ViewService,
};
use em_domain::error::DomainError;
use em_domain::model::{NodeId, ProjectId, SubProjectId};
use em_domain::port::{EdgeDirection, ParserRegistry, Persistence, ViewRegistryProvider};

use crate::dto::{
    ApiResponse, CreateProjectRequest, DirEntryDto, HealthDto, ProjectDto, RunAcceptedDto,
    RunStatusDto, SubProjectDto, UpdateProjectRequest,
};

/// 所有处理器共享的状态（组装根注入）。
pub struct AppState {
    pub projects: Arc<ProjectService>,
    pub pipeline: Arc<PipelineService>,
    pub graphs: Arc<GraphQueryService>,
    pub views: Arc<ViewService>,
    pub parsers: Arc<dyn ParserRegistry>,
    pub frameworks: usize,
    /// 最近一次建图进度（工程 id → 观察者）。
    pub progress: Mutex<std::collections::HashMap<i64, Arc<ProgressObserver>>>,
}

pub type Shared = Arc<AppState>;

/// 便捷构造：由各服务组装 HTTP 状态。
pub fn state(
    store: Arc<dyn Persistence>,
    deps: Arc<PipelineDeps>,
    frameworks: usize,
    view_registry: Arc<dyn ViewRegistryProvider>,
) -> Shared {
    let projects = Arc::new(ProjectService::new(
        Arc::clone(&store),
        Arc::new(em_domain::port::SystemClock),
    ));
    let pipeline = Arc::new(PipelineService::new(Arc::clone(&store), Arc::clone(&deps)));
    let graphs = Arc::new(GraphQueryService::new(Arc::clone(&store)));
    let views = Arc::new(ViewService::new(Arc::clone(&store), view_registry));
    Arc::new(AppState {
        projects,
        pipeline,
        graphs,
        views,
        parsers: Arc::clone(&deps.parsers),
        frameworks,
        progress: Mutex::new(std::collections::HashMap::new()),
    })
}

pub fn build_router(state: Shared) -> Router {
    Router::new()
        .route("/api/health", get(health))
        // Project CRUD
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project).put(update_project).delete(delete_project),
        )
        // 建图
        .route("/api/projects/{id}/run", post(run_pipeline))
        .route("/api/projects/{id}/run/status", get(run_status))
        // 图查询
        .route("/api/projects/{id}/sub-projects", get(list_sub_projects))
        .route("/api/projects/{id}/stats", get(stats))
        .route("/api/projects/{id}/nodes", get(query_nodes))
        .route("/api/projects/{id}/diagnostics", get(diagnostics))
        // 文件系统浏览（供目录选择器使用，WSL 下可访问 /mnt/c 等挂载路径）
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
        .with_state(state)
}

// ---------------------------------------------------------------- 处理器

/// 探测后端进程是否运行在 WSL 中。
///
/// WSL 的内核发行信息（`/proc/sys/kernel/osrelease` 与 `/proc/version`）会包含
/// "microsoft" 字样，裸 Linux / Docker / 远程 VM 不会。发行版名难以从内核信息取得，
/// 默认返回 `Ubuntu`（绝大多数默认发行版即此名），前端仍可手动覆盖。
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
    let result = state.projects.create(em_domain::model::NewProject {
        name: req.name,
        root_path: req.root_path.into(),
        description: req.description,
        config: req.config,
    });
    match result {
        Ok(p) => {
            // 创建后自动开始建图
            let _ = trigger_run(&state, p.id);
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

/// 触发后台建图。
fn trigger_run(state: &Shared, id: ProjectId) -> Result<(), DomainError> {
    let observer = Arc::new(ProgressObserver::new(id));
    state
        .progress
        .lock()
        .unwrap()
        .insert(id.get(), Arc::clone(&observer));
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
) -> Json<ApiResponse<em_domain::port::GraphStats>> {
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
) -> Json<ApiResponse<Vec<em_domain::model::Node>>> {
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
) -> Json<ApiResponse<Option<em_domain::model::Node>>> {
    match state.graphs.node(NodeId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn node_annotations(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Vec<em_domain::model::Annotation>>> {
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
) -> Json<ApiResponse<Vec<em_domain::model::Edge>>> {
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
) -> Json<ApiResponse<em_application::graph_query::Subgraph>> {
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
) -> Json<ApiResponse<Vec<em_domain::model::SymbolEntry>>> {
    let pid = ProjectId(q.project_id.unwrap_or(1));
    match state.graphs.symbols(pid, &table) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn diagnostics(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Vec<em_domain::model::Diagnostic>>> {
    match state.graphs.diagnostics(ProjectId(id), 200) {
        Ok(v) => Json(ApiResponse::success(v)),
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
}

async fn view_candidates(
    State(state): State<Shared>,
    Path((id, perspective)): Path<(i64, String)>,
    Query(q): Query<ViewQuery>,
) -> Json<ApiResponse<Vec<em_domain::model::Candidate>>> {
    match state
        .views
        .candidates(
            ProjectId(id),
            &perspective,
            q.limit.unwrap_or(300),
            q.name_contains.as_deref(),
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
) -> Json<ApiResponse<em_domain::model::ObjectView>> {
    let Some(node) = q.node else {
        return Json(ApiResponse::failure("缺少 node 参数"));
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
) -> Json<ApiResponse<em_domain::model::AggregateView>> {
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
) -> Json<ApiResponse<em_domain::model::NodeLocations>> {
    match state.views.node_locations(NodeId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

async fn edge_evidence(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<Option<em_domain::model::EdgeEvidence>>> {
    match state.views.edge_evidence(id) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    pub path: Option<String>,
}

/// 浏览目录：返回该路径下的子目录列表（供前端目录选择器）。
///
/// 后端进程运行在宿主系统（含 WSL），因此天然支持 `/mnt/c` 等挂载路径。
/// 路径为空时回退到 `/home`（若存在）或 `/`。
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
            "无法读取目录「{}」: {}",
            candidate, e
        ))),
    }
}

/// 预留：按子工程过滤文件。
#[allow(dead_code)]
fn _sub(id: SubProjectId) -> SubProjectId {
    id
}
