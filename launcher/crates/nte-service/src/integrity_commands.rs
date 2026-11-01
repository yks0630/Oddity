//! 启动器安全验证的后台检查与状态保存.
//!
//! 检查在独立线程里完成, 不阻塞窗口和游戏启动; 界面通过 `integrity-state`
//! 事件和 `Bootstrap.integrity` 读取结果. 提示是否已经展示过保存在数据目录,
//! 避免每次启动重复打扰.

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;

use nte_core::integrity::{self, History, Installation, Notice, Status};

use crate::service::ServiceHandle;

/// 当前进程内的校验状态和上次检查记录.
pub struct IntegrityMonitor {
    history_path: PathBuf,
    status: Mutex<Status>,
    history: Mutex<History>,
}

impl IntegrityMonitor {
    pub fn new(data_dir: &std::path::Path) -> Self {
        let history_path = data_dir.join("integrity.json");
        let history = History::load(&history_path);
        // 先展示上一次的检查时间, 新结果完成后由事件替换.
        let status = Status::checking(env!("CARGO_PKG_VERSION"), history.checked_at);
        Self {
            history_path,
            status: Mutex::new(status),
            history: Mutex::new(history),
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn apply(&self, mut result: Status) -> Status {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let notice = history.observe(&result);
        if notice == Some(Notice::Changed) {
            // 本次没有复现或联网失败时, 用上次尚未确认的变更列表恢复提示.
            result.state = integrity::State::Changed;
            result.source = history.pending_source;
            result.changed = history.pending_changed.clone();
            result.changed_total = history.pending_changed_total;
        }
        result.notice = notice;
        // 状态文件写失败不应让本次检查消失, 下次启动重新判断即可.
        let _ = history.save(&self.history_path);
        drop(history);
        *self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = result.clone();
        result
    }

    pub fn acknowledge(&self, notice: Notice) -> Result<(), String> {
        let mut history = self.history.lock().map_err(|error| error.to_string())?;
        if history.acknowledge(notice) {
            history
                .save(&self.history_path)
                .map_err(|error| error.to_string())?;
        }
        drop(history);
        let mut status = self.status.lock().map_err(|error| error.to_string())?;
        if status.notice == Some(notice) {
            status.notice = None;
        }
        Ok(())
    }
}

/// 启动异步检查; 结果通过事件送回界面.
pub fn start(app: ServiceHandle) {
    let _ = thread::Builder::new()
        .name("oddity-integrity".into())
        .spawn(move || {
            let installation = Installation::detect();
            let result = integrity::check(
                &installation,
                env!("CARGO_PKG_VERSION"),
                integrity::FETCH_TIMEOUT,
            );
            let status = app.integrity.apply(result);
            let _ = app.emit("integrity-state", status);
        });
}

/// 界面展示完提示后清除, 避免重复弹出.
pub fn acknowledge(app: &ServiceHandle, notice: &str) -> Result<(), String> {
    let notice = match notice {
        "success" => Notice::Success,
        "changed" => Notice::Changed,
        _ => return Err("未知的安全验证提示".into()),
    };
    app.integrity.acknowledge(notice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TempDir;

    fn status(state: integrity::State, version: &str) -> Status {
        Status {
            state,
            source: Some(integrity::Source::Online),
            checked_at: 1234,
            changed: Vec::new(),
            changed_total: 0,
            notice: None,
            app_version: version.into(),
            release_url: String::new(),
        }
    }

    #[test]
    fn monitor_keeps_notice_until_acknowledged() {
        let directory = TempDir::new("integrity-monitor");
        let monitor = IntegrityMonitor::new(directory.path());
        assert_eq!(monitor.status().state, integrity::State::Checking);
        assert_eq!(monitor.status().checked_at, 0);

        let changed = monitor.apply(status(integrity::State::Changed, "1.0.0"));
        assert_eq!(changed.notice, Some(Notice::Changed));
        // 重新读取时提示仍在, 直到界面确认.
        assert_eq!(monitor.status().notice, Some(Notice::Changed));
        let stored = History::load(&directory.join("integrity.json"));
        assert_eq!(stored.pending_notice, Some(Notice::Changed));
        assert_eq!(stored.state, Some(integrity::State::Changed));

        monitor.acknowledge(Notice::Changed).unwrap();
        assert_eq!(monitor.status().notice, None);
        let stored = History::load(&directory.join("integrity.json"));
        assert_eq!(stored.pending_notice, None);
        assert_eq!(stored.changed_notice_version.as_deref(), Some("1.0.0"));

        // 同一版本再次发现不一致时不再提示.
        let changed = monitor.apply(status(integrity::State::Changed, "1.0.0"));
        assert_eq!(changed.notice, None);
    }

    #[test]
    fn monitor_restores_unacknowledged_changes_after_offline_check() {
        let directory = TempDir::new("integrity-monitor-pending");
        let monitor = IntegrityMonitor::new(directory.path());
        let mut changed = status(integrity::State::Changed, "1.0.0");
        changed.changed = vec!["app.bin".into()];
        changed.changed_total = 1;
        monitor.apply(changed);

        let mut baseline = status(integrity::State::Baseline, "1.0.0");
        baseline.source = Some(integrity::Source::Bundled);
        let restored = monitor.apply(baseline);
        assert_eq!(restored.state, integrity::State::Changed);
        assert_eq!(restored.source, Some(integrity::Source::Online));
        assert_eq!(restored.changed, vec!["app.bin".to_string()]);
        assert_eq!(restored.notice, Some(Notice::Changed));

        monitor.acknowledge(Notice::Changed).unwrap();
        let again = monitor.apply(status(integrity::State::Baseline, "1.0.0"));
        assert_eq!(again.notice, None);
    }

    #[test]
    fn monitor_reports_success_after_a_failed_check() {
        let directory = TempDir::new("integrity-monitor-success");
        let monitor = IntegrityMonitor::new(directory.path());
        monitor.apply(status(integrity::State::Unverified, "1.0.0"));
        let verified = monitor.apply(status(integrity::State::Verified, "1.0.0"));
        assert_eq!(verified.notice, Some(Notice::Success));
        assert_eq!(monitor.status().state, integrity::State::Verified);
    }

    #[test]
    fn start_publishes_the_detected_state() {
        use std::time::Duration;

        let app = crate::service::test_service();
        let (sender, receiver) = std::sync::mpsc::channel();
        app.subscribe("integrity-state", move |payload| {
            let _ = sender.send(payload);
        });
        start(app.clone());
        let payload = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("安全验证事件");
        // 测试进程不在打包目录里, 应归类为不支持校验, 不发起网络请求.
        assert_eq!(payload["state"], "unsupported");
        assert_eq!(app.integrity.status().state, integrity::State::Unsupported);
    }

    #[test]
    fn acknowledge_rejects_unknown_notices() {
        let app = crate::service::test_service();
        assert!(acknowledge(&app, "unknown").is_err());
        assert!(acknowledge(&app, "success").is_ok());
    }
}
