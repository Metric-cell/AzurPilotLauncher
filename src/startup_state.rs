//! Startup visibility and worker lifetime shared with native window callbacks.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone, Default)]
pub(crate) struct StartupState {
    pub running: Arc<AtomicBool>,
    pub completed: Arc<AtomicBool>,
}

impl StartupState {
    pub fn start_worker(&self) -> StartupWorker {
        self.running.store(true, Ordering::SeqCst);
        StartupWorker(self.running.clone())
    }

    pub fn can_show_main(&self) -> bool {
        self.completed.load(Ordering::SeqCst)
    }
}

/// Keep this guard alive until the startup worker and its owned resources end.
/// Cleanup must also be released on early return or panic unwinding.
pub(crate) struct StartupWorker(Arc<AtomicBool>);

impl Drop for StartupWorker {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_panic_releases_cleanup_without_exposing_main() {
        let state = StartupState::default();
        let result = std::panic::catch_unwind(|| {
            let _worker = state.start_worker();
            assert!(state.running.load(Ordering::SeqCst));
            assert!(!state.can_show_main());
            panic!("fixture: startup window disappeared");
        });
        assert!(result.is_err());
        assert!(!state.running.load(Ordering::SeqCst));
        assert!(!state.can_show_main());
    }
}
