use anyhow::Result;
use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
    flag,
    low_level::unregister,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Handlers only set a flag. Resource cleanup stays on the session thread.
pub(super) struct SessionSignals {
    stop: Arc<AtomicBool>,
    ids: Vec<SigId>,
}

impl SessionSignals {
    pub fn new() -> Result<Self> {
        let mut signals = Self {
            stop: Arc::new(AtomicBool::new(false)),
            ids: Vec::new(),
        };
        for signal in [SIGINT, SIGTERM] {
            signals
                .ids
                .push(flag::register(signal, signals.stop.clone())?);
        }
        Ok(signals)
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl Drop for SessionSignals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            unregister(id);
        }
    }
}
