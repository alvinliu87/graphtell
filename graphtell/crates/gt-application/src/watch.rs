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
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tracing::{info, warn};

use gt_domain::model::ProjectId;

use crate::pipeline_runner::PipelineService;
use crate::project_service::ProgressObserver;
use crate::RecallService;

/// 基础轮询间隔（按工程规模自适应）。
///
/// 实测：serve 会给**每个**工程起一个监听线程，30 个工程各自每 2s 全量遍历源码比对
/// mtime，常驻就把多个核吃满（累计 CPU 2h26m / 运行 21min ≈ 7 核），把用户的首次
/// 查询挤到最后。大工程遍历成本高，这里按规模放慢。
const POLL_INTERVAL_SMALL: Duration = Duration::from_secs(2);
const POLL_INTERVAL_MID: Duration = Duration::from_secs(5);
const POLL_INTERVAL_LARGE: Duration = Duration::from_secs(10);
/// 空闲退避上限：连续无变更时逐步翻倍放慢到这个间隔 —— 代码不动时几乎不吃 CPU。
const POLL_INTERVAL_MAX: Duration = Duration::from_secs(30);
const SMALL_PROJECT_FILES: usize = 500;
const LARGE_PROJECT_FILES: usize = 3000;

/// 防抖窗口：最后一次改动后安静这么久才触发重建，合并编辑爆发。
const DEBOUNCE: Duration = Duration::from_secs(3);

/// 全局重建冷却：两次重建之间至少间隔这么久（跨工程串行）。
///
/// 监听线程是**按工程**起的，彼此不知道对方在干什么；一旦多个工程同时改动就会同时
/// 整库重建（整库重建是重活）。这里用一个全局闸门把重建串行化。小工程的轮询更灵敏，
/// 因此自然先抢到名额 —— 这就是"小工程优先"。
const REBUILD_COOLDOWN: Duration = Duration::from_secs(30);

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
    // 启动错峰：serve 会一次性给所有工程起监听线程，若同时做首次全量遍历会瞬间打满
    // CPU。按工程 id 摊开 0~3s，把启动尖峰抹平。
    thread::sleep(Duration::from_millis((project_id.get() as u64 % 30) * 100));
    let mut mtimes = snapshot(&root);
    // 基础间隔按工程规模决定（大工程遍历贵）；随后按空闲情况退避。
    let base_interval = poll_interval(mtimes.len());
    let mut interval = base_interval;
    let mut dirty = false;
    let mut dirty_since: Option<Instant> = None;
    loop {
        thread::sleep(interval);
        if scan(&root, &mut mtimes) {
            // 有变更：立刻回到灵敏档，保证后续改动也能被及时看到。
            interval = base_interval;
            dirty = true;
            if dirty_since.is_none() {
                dirty_since = Some(Instant::now());
            }
        } else if !dirty {
            // 空闲退避：连续无变更时逐步翻倍放慢，封顶 [`POLL_INTERVAL_MAX`]。
            // 代码不动时几乎不吃 CPU —— 这正是之前"常驻 7 核满载"的主因。
            interval = (interval * 2).min(POLL_INTERVAL_MAX);
        }
        if dirty {
            if let Some(since) = dirty_since {
                if since.elapsed() >= DEBOUNCE {
                    // 跨工程串行：等上一个工程让出重建名额（见 [`REBUILD_COOLDOWN`]），
                    // 避免多个工程同时整库重建把 CPU 打满。
                    acquire_rebuild_slot();
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

/// 按工程文件数决定基础轮询间隔：大工程遍历成本高，放慢以压低常驻开销。
fn poll_interval(files: usize) -> Duration {
    if files <= SMALL_PROJECT_FILES {
        POLL_INTERVAL_SMALL
    } else if files <= LARGE_PROJECT_FILES {
        POLL_INTERVAL_MID
    } else {
        POLL_INTERVAL_LARGE
    }
}

/// 全局重建闸门：记录上次重建开始时刻，据此把跨工程重建串行化。
fn rebuild_gate() -> &'static (Mutex<Option<Instant>>, Condvar) {
    static GATE: OnceLock<(Mutex<Option<Instant>>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| (Mutex::new(None), Condvar::new()))
}

/// 占用重建名额：距上次重建不足 [`REBUILD_COOLDOWN`] 时阻塞等待。
///
/// 用"冷却"而非"并发计数"是因为 `pipeline.spawn` 是异步的，这里拿不到"重建已完成"
/// 的回调；而冷却足以把重建摊开，避免同时开工。
fn acquire_rebuild_slot() {
    let (lock, cv) = rebuild_gate();
    let mut last = lock.lock().unwrap();
    loop {
        match *last {
            Some(t) => {
                let elapsed = t.elapsed();
                if elapsed >= REBUILD_COOLDOWN {
                    break;
                }
                let wait = REBUILD_COOLDOWN - elapsed;
                last = cv.wait_timeout(last, wait).unwrap().0;
            }
            None => break,
        }
    }
    *last = Some(Instant::now());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_scales_with_project_size() {
        // 大工程遍历成本高，必须放慢：这是"常驻 7 核满载"的主因，分档边界要锁住。
        assert_eq!(poll_interval(100), POLL_INTERVAL_SMALL);
        assert_eq!(poll_interval(SMALL_PROJECT_FILES), POLL_INTERVAL_SMALL);
        assert_eq!(poll_interval(SMALL_PROJECT_FILES + 1), POLL_INTERVAL_MID);
        assert_eq!(poll_interval(LARGE_PROJECT_FILES), POLL_INTERVAL_MID);
        assert_eq!(poll_interval(LARGE_PROJECT_FILES + 1), POLL_INTERVAL_LARGE);
    }

    #[test]
    fn idle_backoff_is_capped() {
        // 空闲退避必须封顶，否则间隔会无限增长，改动后迟迟发现不了。
        let mut interval = POLL_INTERVAL_SMALL;
        for _ in 0..20 {
            interval = (interval * 2).min(POLL_INTERVAL_MAX);
        }
        assert_eq!(interval, POLL_INTERVAL_MAX);
    }
}
