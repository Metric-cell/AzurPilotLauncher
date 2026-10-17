//! Retain startup progress until the splash JavaScript acknowledges readiness.
use std::sync::Mutex;

#[derive(Default)]
struct Pending {
    ready: bool,
    latest: Option<String>,
}

#[derive(Default)]
pub struct SplashState(Mutex<Pending>);

impl SplashState {
    pub fn reset(&self) {
        self.0.lock().unwrap().ready = false;
    }

    // Dispatch while holding the lock so the readiness replay cannot overtake a
    // newer update from the startup thread.
    pub fn update(&self, payload: String, dispatch: impl FnOnce(&str)) {
        let mut state = self.0.lock().unwrap();
        state.latest = Some(payload);
        if state.ready {
            dispatch(state.latest.as_deref().unwrap());
        }
    }

    pub fn acknowledge(&self, dispatch: impl FnOnce(&str)) {
        let mut state = self.0.lock().unwrap();
        if let Some(payload) = state.latest.as_deref() {
            dispatch(payload);
        }
        state.ready = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_page_replays_latest_error_instead_of_initial_progress() {
        let state = SplashState::default();
        state.update("4%".into(), |_| panic!("page is not ready"));
        state.update("error 100%".into(), |_| panic!("page is not ready"));
        state.acknowledge(|payload| assert_eq!(payload, "error 100%"));
        state.update("retry".into(), |payload| assert_eq!(payload, "retry"));
    }

    #[test]
    fn ready_before_progress_and_reload_both_retain_updates() {
        let state = SplashState::default();
        state.acknowledge(|_| panic!("no progress yet"));
        state.update("loading".into(), |payload| assert_eq!(payload, "loading"));
        state.reset();
        state.update("failed".into(), |_| panic!("reload is not ready"));
        state.acknowledge(|payload| assert_eq!(payload, "failed"));
    }
}
