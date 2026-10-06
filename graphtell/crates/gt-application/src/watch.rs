//! Source-change watching (polling + debounce): after changes settle, reuse the existing `PipelineService` for a whole-DB safe rebuild,
//! and clear the recall-vector cache so graph, recall, and compliance all reflect the latest code.
//!
//! # Why "whole-DB rebuild" rather than "true single-file increment"
//!
//! `GraphDelta` currently only has `reset_project` (whole-DB clear + rebuild) + incremental insertion, **no per-file field deletion**;
//! even if we added deletion by `file_id`, the downstream `resolve` / `propagate` / `taint` / `cors` / `sign` / `tx` /
//! `guard` are still **global in-memory** computation — to correctly recompute cross-file call edges you must load the whole graph. So a true
//! single-file increment is "small fix + large-stage-scope change", high risk, limited gain.
//!
//! This module takes the **low-risk v1**: poll the source root, and after changes settle (debounce window) call the existing `PipelineService::run`
//! (which already `reset_project` clears + rebuilds + auto-runs the compliance check). Because the watch thread runs **in the same process** as the resident service,
//! reusing the same `PipelineService` and store, there's no cross-process DB contention; `PipelineService`'s own `running` lock already guarantees only one build per project at a time.

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

/// Base polling interval (adapts to project size).
///
/// Measured: serve spins up a watch thread for **every** project; 30 projects each do a full source walk comparing
/// mtime every 2s, and resident load pins multiple cores (cumulative CPU 2h26m / 21min runtime ≈ 7 cores), pushing the user's first
/// query to the very end. Large projects are expensive to walk, so here we slow down by size.
const POLL_INTERVAL_SMALL: Duration = Duration::from_secs(2);
const POLL_INTERVAL_MID: Duration = Duration::from_secs(5);
const POLL_INTERVAL_LARGE: Duration = Duration::from_secs(10);
/// Idle backoff cap: when no change occurs continuously, double the interval up to this ceiling — almost no CPU when code is still.
const POLL_INTERVAL_MAX: Duration = Duration::from_secs(30);
const SMALL_PROJECT_FILES: usize = 500;
const LARGE_PROJECT_FILES: usize = 3000;

/// Debounce window: trigger rebuild only after this much quiet since the last change, merging edit bursts.
const DEBOUNCE: Duration = Duration::from_secs(3);

/// Global rebuild cooldown: at least this long between two rebuilds (cross-project serialized).
///
/// Watch threads are started **per project** and don't know what each other are doing; once several projects change at once they'd all rebuild
/// the whole DB simultaneously (a whole-DB rebuild is heavy work). Here a global gate serializes rebuilds. Small projects poll more sensitively,
/// so they naturally grab the slot first — that's "small-project-first".
const REBUILD_COOLDOWN: Duration = Duration::from_secs(30);

/// Start watching a project's source root (detached thread, ends when the process exits).
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
    // Stagger startup: serve starts watch threads for all projects at once; doing the first full walk simultaneously would spike CPU instantly.
    // Spread by project id over 0~3s to flatten the startup spike.
    thread::sleep(Duration::from_millis((project_id.get() as u64 % 30) * 100));
    let mut mtimes = snapshot(&root);
    // Base interval depends on project size (large projects are expensive to walk); then back off by idle state.
    let base_interval = poll_interval(mtimes.len());
    let mut interval = base_interval;
    let mut dirty = false;
    let mut dirty_since: Option<Instant> = None;
    loop {
        thread::sleep(interval);
        if scan(&root, &mut mtimes) {
            // Change detected: return to the sensitive tier immediately, so later changes are also seen promptly.
            interval = base_interval;
            dirty = true;
            if dirty_since.is_none() {
                dirty_since = Some(Instant::now());
            }
        } else if !dirty {
            // Idle backoff: when no change occurs continuously, double the interval up to [`POLL_INTERVAL_MAX`].
            // Almost no CPU when code is still — this was the main cause of the earlier "resident 7 cores saturated".
            interval = (interval * 2).min(POLL_INTERVAL_MAX);
        }
        if dirty {
            if let Some(since) = dirty_since {
                if since.elapsed() >= DEBOUNCE {
                    // Cross-project serialization: wait for the previous project to yield the rebuild slot (see [`REBUILD_COOLDOWN`]),
                    // to avoid multiple projects rebuilding the whole DB at once and pinning the CPU.
                    acquire_rebuild_slot();
                    match pipeline.spawn(project_id, Arc::new(ProgressObserver::new(project_id))) {
                        Ok(()) => {
                            info!("project #{} source changed, rebuild triggered", project_id.get());
                            // Graph rebuilt, old node vectors are stale: clear the recall cache, next recall recomputes / re-warms on the new graph.
                            recall.clear_node_cache();
                            dirty = false;
                            dirty_since = None;
                        }
                        Err(e) => {
                            // Building / failed: keep dirty, reset the debounce timer and retry later.
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

/// Base polling interval by project file count: large projects are expensive to walk, slow down to cut resident overhead.
fn poll_interval(files: usize) -> Duration {
    if files <= SMALL_PROJECT_FILES {
        POLL_INTERVAL_SMALL
    } else if files <= LARGE_PROJECT_FILES {
        POLL_INTERVAL_MID
    } else {
        POLL_INTERVAL_LARGE
    }
}

/// Global rebuild gate: records the last rebuild start time, used to serialize cross-project rebuilds.
fn rebuild_gate() -> &'static (Mutex<Option<Instant>>, Condvar) {
    static GATE: OnceLock<(Mutex<Option<Instant>>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| (Mutex::new(None), Condvar::new()))
}

/// Take a rebuild slot: block-wait when less than [`REBUILD_COOLDOWN`] since the last rebuild.
///
/// We use "cooldown" rather than "concurrency count" because `pipeline.spawn` is async and we can't get a "rebuild done"
/// callback here; but a cooldown is enough to spread rebuilds out and avoid starting them all at once.
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

/// Walk the source root with `ignore` (respects .gitignore, skips hidden files), recording each file's mtime.
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

/// Re-walk; if any file's mtime changed / added / removed return `true`, updating the map in place.
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
    // A file being deleted (count mismatch) also counts as a change.
    if seen.len() != mtimes.len() {
        changed = true;
    }
    *mtimes = seen;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn poll_interval_scales_with_project_size() {
        // Large projects are expensive to walk, must slow down: main cause of the "resident 7 cores saturated", the tier boundary must be locked.
        assert_eq!(poll_interval(100), POLL_INTERVAL_SMALL);
        assert_eq!(poll_interval(SMALL_PROJECT_FILES), POLL_INTERVAL_SMALL);
        assert_eq!(poll_interval(SMALL_PROJECT_FILES + 1), POLL_INTERVAL_MID);
        assert_eq!(poll_interval(LARGE_PROJECT_FILES), POLL_INTERVAL_MID);
        assert_eq!(poll_interval(LARGE_PROJECT_FILES + 1), POLL_INTERVAL_LARGE);
    }

    #[test]
    fn idle_backoff_is_capped() {
        // Idle backoff must be capped, otherwise the interval grows unbounded and changes are detected far too late.
        let mut interval = POLL_INTERVAL_SMALL;
        for _ in 0..20 {
            interval = (interval * 2).min(POLL_INTERVAL_MAX);
        }
        assert_eq!(interval, POLL_INTERVAL_MAX);
    }

    // ---- change detection (the watch heart) ----

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gt_watch_it_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn snapshot_records_all_files_and_unchanged_is_false() {
        let dir = temp_root("unchanged");
        std::fs::write(dir.join("a.rs"), b"a").unwrap();
        std::fs::write(dir.join("b.rs"), b"b").unwrap();
        let mut mtimes = snapshot(&dir);
        assert_eq!(mtimes.len(), 2, "the snapshot must record every file");
        assert!(!scan(&dir, &mut mtimes), "with no change scan must return false");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_detects_added_file() {
        let dir = temp_root("added");
        std::fs::write(dir.join("a.rs"), b"a").unwrap();
        let mut mtimes = snapshot(&dir);
        std::fs::write(dir.join("b.rs"), b"b").unwrap();
        assert!(scan(&dir, &mut mtimes), "a newly added file must be detected");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_detects_deleted_file() {
        let dir = temp_root("deleted");
        std::fs::write(dir.join("a.rs"), b"a").unwrap();
        std::fs::write(dir.join("b.rs"), b"b").unwrap();
        let mut mtimes = snapshot(&dir);
        std::fs::remove_file(dir.join("b.rs")).unwrap();
        assert!(scan(&dir, &mut mtimes), "a deleted file must be detected");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_detects_modified_mtime() {
        let dir = temp_root("modified");
        let p = dir.join("a.rs");
        std::fs::write(&p, b"a").unwrap();
        let mut mtimes = snapshot(&dir);
        // Filesystem mtime resolution is coarse, so force a distinct later mtime explicitly.
        OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        assert!(scan(&dir, &mut mtimes), "an mtime change must be detected");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- cross-project rebuild gate (`rebuild_gate` / `acquire_rebuild_slot`) ----
    //
    // The watch loop is orchestration glue (needs a live `PipelineService` + `RecallService` to actually rebuild), so
    // it stays out of unit scope; but the gate that serializes rebuilds across projects is pure logic and was
    // completely untested. A regression here either deadlocks every rebuild or lets projects rebuild the whole DB
    // simultaneously — both were exactly the "resident 7 cores saturated" failure mode.

    #[test]
    fn rebuild_gate_returns_one_stable_static() {
        // The gate must be a single global instance shared by every project's watch thread.
        let a: *const _ = rebuild_gate();
        let b: *const _ = rebuild_gate();
        assert_eq!(a, b, "rebuild_gate must return the same global static, otherwise cross-project serialisation breaks");
    }

    #[test]
    fn acquire_rebuild_slot_returns_immediately_after_cooldown() {
        // Force the last rebuild far in the past so the cooldown is satisfied → must not block.
        let (lock, _cv) = rebuild_gate();
        *lock.lock().unwrap() = Some(Instant::now() - REBUILD_COOLDOWN - Duration::from_secs(1));
        let start = Instant::now();
        acquire_rebuild_slot();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "an expired cooldown must grant the rebuild slot immediately instead of blocking for 30s"
        );
        // After acquiring, the gate is advanced to "now" so the next caller waits the full cooldown.
        assert!(lock.lock().unwrap().is_some());
    }
}
