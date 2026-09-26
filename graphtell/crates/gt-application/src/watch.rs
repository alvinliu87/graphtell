//! 源码变更监听（轮询 + 防抖）：改动停滞后复用现有 `PipelineService` 整库安全重建，
//! 并清空召回向量缓存，使图、召回、合规都反映最新代码。
//!
//! # 为什么是「整库重建」而不是「真·单文件增量」
//!
//! `GraphDelta` 目前只有 `reset_project`（整库清空重建）+ 增量插入，**没有按文件删除字段**；
//! 即便补上按 `file_id` 删除，下游 `resolve` / `propagate` / `taint` / `cors` / `sign` / `tx` /
//! `guard` 仍是**全局内存**计算——要正确重算跨文件调用边，必须加载整张图。所以真正的
//! 单文件增量是「小修 + 大改阶段作用域」，风险高、收益有限。
//!
//! 本模块走**低风险 v1**：轮询源码根，改动停滞后（防抖窗口）调用既有 `PipelineService::run`
//! （它本来就会 `reset_project` 清空 + 重建 + 自动跑合规检查）。因为监听线程跑在**常驻服务
//! 同一进程**内、复用同一 `PipelineService` 与 store，不存在跨进程 DB 争用；`PipelineService`
//! 自身的 `running` 锁已保证单工程同时只有一个建图任务。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tracing::{info, warn};

use gt_domain::model::ProjectId;

use crate::pipeline_runner::PipelineService;
use crate::project_service::ProgressObserver;
use crate::RecallService;

/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 防抖窗口：最后一次改动后安静这么久才触发重建，合并编辑爆发。
const DEBOUNCE: Duration = Duration::from_secs(3);

/// 启动对某工程源码根的监听（detached 线程，进程退出即止）。
pub fn watch_project(
    project_id: ProjectId,
    root: PathBuf,
    pipeline: Arc<PipelineService>,
    recall: Arc<RecallService>,
) {
    thread::spawn(move || run(project_id, root, pipeline, recall));
}

fn run(
    project_id: ProjectId,
    root: PathBuf,
    pipeline: Arc<PipelineService>,
    recall: Arc<RecallService>,
) {
    info!(
        "监听工程 #{} 源码变更：{}",
        project_id.get(),
        root.display()
    );
    let mut mtimes = snapshot(&root);
    let mut dirty = false;
    let mut dirty_since: Option<Instant> = None;
    loop {
        thread::sleep(POLL_INTERVAL);
        if scan(&root, &mut mtimes) {
            dirty = true;
            if dirty_since.is_none() {
                dirty_since = Some(Instant::now());
            }
        }
        if dirty {
            if let Some(since) = dirty_since {
                if since.elapsed() >= DEBOUNCE {
                    match pipeline.spawn(project_id, Arc::new(ProgressObserver::new(project_id))) {
                        Ok(()) => {
                            info!("工程 #{} 源码变更，已触发重建", project_id.get());
                            // 图已重建，旧节点向量失效：清空召回缓存，下次召回按新图重算/重预热。
                            recall.clear_node_cache();
                            dirty = false;
                            dirty_since = None;
                        }
                        Err(e) => {
                            // 正在建图 / 失败：保留 dirty，重置防抖计时稍后重试。
                            warn!(
                                "工程 #{} 重建未启动（{}），稍后重试",
                                project_id.get(),
                                e
                            );
                            dirty_since = Some(Instant::now());
                        }
                    }
                }
            }
        }
    }
}

/// 用 `ignore` 遍历源码根（遵循 .gitignore、跳过隐藏文件），记录每个文件 mtime。
fn snapshot(root: &Path) -> HashMap<PathBuf, SystemTime> {
    let mut map = HashMap::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let Ok(e) = entry else { continue };
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let Ok(t) = meta.modified() else { continue };
        map.insert(e.path().to_path_buf(), t);
    }
    map
}

/// 重新遍历；任一文件 mtime 变化 / 新增 / 删除即返回 `true`，并就地更新 map。
fn scan(root: &Path, mtimes: &mut HashMap<PathBuf, SystemTime>) -> bool {
    let mut changed = false;
    let mut seen: HashMap<PathBuf, SystemTime> = HashMap::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let Ok(e) = entry else { continue };
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = e.path().to_path_buf();
        let Some(t) = e.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        if mtimes.get(&path).map(|old| *old != t).unwrap_or(true) {
            changed = true;
        }
        seen.insert(path, t);
    }
    // 文件被删除（数量不一致）也算变更。
    if seen.len() != mtimes.len() {
        changed = true;
    }
    *mtimes = seen;
    changed
}
