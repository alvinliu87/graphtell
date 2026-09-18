//! 可观测性端口（流水线进度上报）。

use crate::model::{Phase, PhaseReport, ProjectId};

/// 流水线观察者。
///
/// 默认全空实现 —— 调用方可选择不关心进度，无需传 `Option`。
pub trait PipelineObserver: Send + Sync {
    fn on_phase_start(&self, _project_id: ProjectId, _phase: &Phase) {}
    fn on_phase_end(&self, _project_id: ProjectId, _report: &PhaseReport) {}
    fn on_message(&self, _project_id: ProjectId, _message: &str) {}
}

/// 什么都不做的观察者（单元测试与 CLI 静默模式使用）。
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

/// 时间端口，便于测试与可重现的审计时间。
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> i64;
}

/// 系统时钟。
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}
