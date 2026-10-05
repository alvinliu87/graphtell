//! Observability port (pipeline progress reporting).

use crate::model::{Phase, PhaseReport, ProjectId};

/// Pipeline observer.
///
/// All-default no-op implementation — a caller may simply not care about progress, so no `Option` is needed.
pub trait PipelineObserver: Send + Sync {
    fn on_phase_start(&self, _project_id: ProjectId, _phase: &Phase) {}
    fn on_phase_end(&self, _project_id: ProjectId, _report: &PhaseReport) {}
    fn on_message(&self, _project_id: ProjectId, _message: &str) {}
}

/// An observer that does nothing (used by unit tests and the CLI quiet mode).
pub struct NoopObserver;

impl PipelineObserver for NoopObserver {}

impl<T: PipelineObserver + ?Sized> PipelineObserver for &T {
    fn on_phase_start(&self, project_id: ProjectId, phase: &Phase) {
        (**self).on_phase_start(project_id, phase)
    }
    fn on_phase_end(&self, project_id: ProjectId, report: &PhaseReport) {
        (**self).on_phase_end(project_id, report)
    }
    fn on_message(&self, project_id: ProjectId, message: &str) {
        (**self).on_message(project_id, message)
    }
}

/// The time port, for testable and reproducible audit timestamps.
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> i64;
}

/// The system clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the one piece of runtime behaviour in this port: `now_millis` must return epoch **millis**, not
    /// seconds and not `0`. The `unwrap_or(0)` fallback means a broken clock could silently regress to `0`, and a
    /// copy-paste of `as_secs()` would be off by 1000× — both are caught here.
    #[test]
    fn system_clock_returns_recent_epoch_millis() {
        let now = SystemClock.now_millis();
        assert!(now > 0, "时钟不应静默返回 0");
        // 2023-01-01 in millis; below this it is either seconds or a regression.
        assert!(
            now > 1_672_531_200_000,
            "now_millis 应返回毫秒级纪元时间，而非秒或 0: {now}"
        );
    }
}
