//! 路由与处理器。

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

/// 所有处理器共享的状态（组装根注入）。
pub struct AppState {
    pub projects: Arc<ProjectService>,
    pub pipeline: Arc<PipelineService>,
    pub graphs: Arc<GraphQueryService>,
    pub views: Arc<ViewService>,
    pub checks: Arc<RuleService>,
    pub recall: Arc<RecallService>,
    pub parsers: Arc<dyn ParserRegistry>,
    pub frameworks: usize,
    /// 最近一次建图进度（工程 id → 观察者）。
    pub progress: Mutex<HashMap<i64, Arc<ProgressObserver>>>,
    /// 共享节点向量缓存（与 `recall` 内的编码器共用）；建图重建时清空。
    pub node_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
}

pub type Shared = Arc<AppState>;

/// 便捷构造：由各服务组装 HTTP 状态。
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
    // 建图完成后会自动跑合规检查，因此流水线也要拿到规则集。
    let pipeline = Arc::new(PipelineService::new(
        Arc::clone(&store),
        Arc::clone(&deps),
        Arc::clone(&rules),
    ));
    let graphs = Arc::new(GraphQueryService::new(Arc::clone(&store)));
    let views = Arc::new(ViewService::new(Arc::clone(&store), view_registry));
    let checks = Arc::new(RuleService::new(Arc::clone(&store), rules));
    // 召回：优先真实 bge-m3（权重可用时），否则退回默认哈希编码器；节点向量缓存跨召回复用。
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
        // 生产 HTTP 入口开启后台异步预热：首个召回立即用快速编码器返回（不阻塞 UI），
        // 同时 spawn 线程把 bge 向量算好落盘；完成后该工程自动切到语义路。
        .with_async_warmup(),
    );
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
    });
    // 启动源码变更监听（P2 v1）：轮询+防抖 → 整库安全重建 + 自动合规 + 清空召回缓存。
    // 仅对已有工程启动；新建工程在 create_project 处理器里单独启动。
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

/// 组装所有路由。
///
/// `ui_dir` 为 `Some` 且指向含 `index.html` 的目录时，后端会**顺带托管构建好的 React SPA**：
/// 用 `tower_http::ServeDir` 做兜底（命中静态文件直接返回，如 `/assets/*`；未命中回退到
/// `index.html` 以支持前端路由），并把根路径 `/` 让给 SPA。此时 `/compose` 仍是内嵌的独立
/// 「提示词增强」页。`ui_dir` 为 `None`（开发态默认）时不托管前端，根路径回退到 `/compose`，
/// 与历史行为一致。
pub fn build_router(state: Shared, ui_dir: Option<std::path::PathBuf>) -> Router {
    let router = Router::new()
        .route("/api/health", get(health))
        // 服务器状态（含当前 embedding 后端，供 UI 只读展示）
        .route("/api/server/status", get(server_status))
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
        .route("/api/projects/{id}/diagnostics/summary", get(diagnostics_summary))
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
        // 合规检查（规则 → 违规）
        .route("/api/rules", get(list_rules))
        .route("/api/projects/{id}/check", post(run_check))
        .route("/api/projects/{id}/violations", get(list_violations))
        .route("/api/projects/{id}/check/summary", get(check_summary))
        // 工程级规则配置（按工程覆盖启用态与参数）
        .route("/api/projects/{id}/rules/config", get(list_rule_configs))
        .route("/api/projects/{id}/rules/config", put(put_rule_config))
        .route("/api/projects/{id}/rules/config/batch", post(batch_rule_config))
        .route(
            "/api/projects/{id}/rules/config/{rule_id}",
            delete(reset_rule_config),
        )
        // 提示词增强 · 召回（提示词 → 相关代码）
        .route("/api/projects/{id}/recall", get(recall_get).post(recall_post))
        // 后台预热进度（语义向量 bge 计算）：让 IDE / MCP 知道召回是否还在走冷路径。
        .route("/api/projects/{id}/warmup", get(project_warmup))
        // 提示词增强 · 合成：召回上下文 + 用户意图 → 可直接粘给 LLM 的完整提示词
        .route("/api/projects/{id}/prompt", post(compose_prompt))
        // 提示词增强页（自包含静态页，内嵌进二进制，无需额外静态托管）
        .route("/compose", get(compose_page));

    // 生产部署可让后端顺带托管构建好的 React SPA：当 `ui_dir` 指向含 index.html 的目录时，
    // 用 ServeDir 兜底（命中文件直接返回、未命中回退 index.html 做前端路由），并把根 `/`
    // 让给 SPA；否则根仍回退到内嵌 compose 页。开发态不传 ui_dir，行为不变。
    let router = if let Some(ui_dir) = ui_dir {
        if ui_dir.join("index.html").is_file() {
            router.fallback_service(
                ServeDir::new(ui_dir.clone()).fallback(ServeFile::new(ui_dir.join("index.html"))),
            )
        } else {
            // 目录存在但没有构建产物：根仍回退到内嵌 compose 页，不崩。
            router.route("/", get(compose_page))
        }
    } else {
        router.route("/", get(compose_page))
    };

    router.with_state(state)
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
    let result = state.projects.create(gt_domain::model::NewProject {
        name: req.name,
        root_path: req.root_path.into(),
        description: req.description,
        config: req.config,
    });
    match result {
        Ok(p) => {
            // 创建后自动开始建图
            let _ = trigger_run(&state, p.id);
            // 新工程也启动变更监听
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

/// 触发后台建图。
fn trigger_run(state: &Shared, id: ProjectId) -> Result<(), DomainError> {
    let observer = Arc::new(ProgressObserver::new(id));
    state
        .progress
        .lock()
        .unwrap()
        .insert(id.get(), Arc::clone(&observer));
    // 图将被重建，旧节点向量可能失效，先清空召回缓存（下次召回重新预热）。
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

/// 诊断列表的读取上限。
///
/// 不能写小：诊断页要**按问题类型分组**（每类的条数、样例位置、展开清单），
/// 而分组必须看到全量才能算对。曾经写 200，而 CRMEB 一个工程就有 445 条建图诊断 ——
/// 于是页面只列最近写入的 200 条（`ORDER BY id DESC`），用户看到的是"写入顺序"，
/// 不是"有多少问题"，分类计数也跟着错。
///
/// 与合规检查页的 `STORED_LIMIT`（5000）同量级：这是"分页前的全量拉取"，页面自己分页。
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

/// 非规则诊断的严重度汇总（菜单角标用，排除 `rule:` 前缀避免与合规检查重复计数）。
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

// ---------------------------------------------------------------- 合规检查

/// 列出全部已装载规则（供 UI 展示"能检查什么"）。
async fn list_rules(State(state): State<Shared>) -> Json<ApiResponse<Vec<CheckRule>>> {
    Json(ApiResponse::success(state.checks.rules()))
}

/// 检查请求体；`rule_ids` 为空表示跑全部启用规则。
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
    /// 逗号分隔的子工程 id；为空表示不过滤（返回全部子工程）。
    pub sub_project_id: Option<String>,
}

/// 把 `?sub_project_id=1,2` 解析成 `Option<Vec<SubProjectId>>`；空串视为不过滤。
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

/// 读取上一次检查落库的违规（不重跑规则）。
/// 读取落库违规的**默认条数**。
///
/// 曾经是 500，实测 likeshop 一次检查 996 条 —— 于是"刚跑完看到 996 / 严重 59"
/// 与"刷新页面看到 500 / 严重 0"对不上，被读成"没持久化 / 回到老数据"。
/// 违规表的读取是分页前的全量拉取（UI 自己按 20 条一页翻），这里只做**上限保护**，
/// 不是分页参数，所以取一个明显够用的值；真超过它时，排序已保证淘汰的是最不严重的。
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

/// 合规检查严重度汇总（菜单角标用，不重跑规则）。
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

/// 工程级规则配置批量写入请求体。
#[derive(Debug, Deserialize)]
pub struct RuleConfigBatch {
    pub items: Vec<RuleConfigPatch>,
}

/// 列出某工程全部规则配置覆盖。
async fn list_rule_configs(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> Json<ApiResponse<std::collections::HashMap<String, ProjectRuleConfig>>> {
    match state.checks.rule_configs(ProjectId(id)) {
        Ok(v) => Json(ApiResponse::success(v)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// 写入单条规则配置（部分字段可省 = 不改该项）。
async fn put_rule_config(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(patch): Json<RuleConfigPatch>,
) -> Json<ApiResponse<bool>> {
    // 返回 `bool` 而不是 `()`：`()` 序列化成 `null`，前端统一响应解析会把
    // `data: null` 当成失败（它无法区分"无数据"和"出错了"）。
    match state.checks.apply_rule_config(ProjectId(id), patch) {
        Ok(_) => Json(ApiResponse::success(true)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// 重置某条规则的工程覆盖（回归 YAML 全局默认）。
async fn reset_rule_config(
    State(state): State<Shared>,
    Path((id, rule_id)): Path<(i64, String)>,
) -> Json<ApiResponse<bool>> {
    match state.checks.reset_rule_config(ProjectId(id), &rule_id) {
        Ok(_) => Json(ApiResponse::success(true)),
        Err(e) => Json(ApiResponse::failure(e.to_string())),
    }
}

/// 批量写入规则配置（启用/停用整组、整分类时用）。
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

// ---------------------------------------------------------- 提示词增强 · 召回

/// GET 形式的召回参数（供 UI 简单调用）。
#[derive(Debug, Deserialize)]
pub struct RecallGetQuery {
    pub q: String,
    pub limit: Option<usize>,
    pub hops: Option<u32>,
    /// 逗号分隔的节点种类，如 `Table,HttpContract`。
    pub kinds: Option<String>,
    pub snippets: Option<bool>,
    /// 是否把命中文件的完整源码一并附上（见 `RecallQuery::include_body`）。
    pub include_body: Option<bool>,
}

impl RecallGetQuery {
    fn into_query(self) -> RecallQuery {
        RecallQuery {
            query: self.q,
            limit: self.limit.unwrap_or(20),
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

// ---------------------------------------------------------- 提示词增强 · 合成

/// 提示词增强请求：召回参数 + 用户任务意图。
#[derive(Debug, Deserialize)]
pub struct ComposePromptRequest {
    /// 用于召回代码的检索词（自然语言 + 标识符混写皆可）。
    pub query: String,
    /// 用户的任务意图 / 补充说明；缺省时提示词会要求 LLM 依据上下文推断。
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub hops: Option<u32>,
    #[serde(default)]
    pub with_snippets: Option<bool>,
}

/// 提示词增强结果。
#[derive(Debug, Serialize)]
pub struct ComposePromptResult {
    /// 可直接粘给 LLM 的完整提示词。
    pub prompt: String,
    /// 原始召回上下文（markdown），便于用户自行裁剪。
    pub markdown: String,
    pub seed_count: usize,
    pub hit_count: usize,
    /// 提示词 token 粗估（中英文按不同系数）。
    pub approx_tokens: usize,
    /// **完整召回结果**（种子 / 命中 / 解析出的查询词 / 质量档位）。
    ///
    /// 为什么要把整份召回结果带上：Web 端"提示词增强"页要同时展示
    /// 「合成的提示词」和「命中列表（可核对召回质量、点特征词重跑）」。
    /// 不带的话前端只能再调一次 `/recall` —— 而召回正是这里最贵的一步，
    /// 一次生成跑两遍纯属浪费，且两次结果可能不一致（图在两次之间被重建）。
    pub recall: gt_application::RecallResult,
}

/// 合成提示词：图谱召回上下文 + 用户意图 → 一段可直接投喂 LLM 的提示词。
///
/// 设计要点：召回只提供"地图"（路径/行号/片段），提示词明确要求 LLM 按需按路径精准
/// 读取文件全文 —— 既省掉 discovery 的 token，又不丢实现完整性。
async fn compose_prompt(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(req): Json<ComposePromptRequest>,
) -> Json<ApiResponse<ComposePromptResult>> {
    let rq = RecallQuery {
        query: req.query.clone(),
        limit: req.limit.unwrap_or(20),
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

/// 粗估 token 数：ASCII（代码/英文）约 4 字符 1 token，中文约 1.5 字符 1 token。
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

/// 提示词增强页：内嵌静态页，避免额外静态资源托管与前端构建。
async fn compose_page() -> Html<&'static str> {
    Html(include_str!("compose.html"))
}

/// 服务器状态：暴露当前生效的 embedding 后端与维度，供 UI 只读展示。
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

/// 后台预热进度：语义向量 bge 是否就绪 / 预热中进度。
#[derive(Debug, Clone, Serialize)]
pub struct WarmupStatusDto {
    /// 该工程语义向量是否已全部就绪（召回走完整语义路）。
    pub warmed: bool,
    /// 是否正在后台预热中（召回暂走冷路径）。
    pub warming: bool,
    /// 已编码节点数（仅 `warming` 时有意义）。
    pub done: usize,
    /// 待编码节点总数（仅 `warming` 时有意义）。
    pub total: usize,
}

/// 查询某工程的后台预热进度。
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
            "Cannot read directory '{}': {}",
            candidate, e
        ))),
    }
}

/// 预留：按子工程过滤文件。
#[allow(dead_code)]
fn _sub(id: SubProjectId) -> SubProjectId {
    id
}
