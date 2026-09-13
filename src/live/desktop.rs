//! One browser owns an extended display across media transports. The lease is
//! memory-only; a closed/lost page reserves its place briefly for reconnection.
use crate::hypr::desktop::DesktopConfig;
use serde::{Deserialize, Serialize};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

pub(super) const RECONNECT_GRACE: Duration = Duration::from_secs(15);
pub(super) const IN_USE: &str = "This display is already connected to another device.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopStats {
    pub config: DesktopConfig,
    pub matched: bool,
    pub occupied: bool,
    pub updating: bool,
    pub error: Option<String>,
    pub reconnect_seconds: u64,
}

struct Lease {
    client: String,
    connection: String,
    until: Instant,
    departed: bool,
}

#[derive(Clone)]
pub(super) struct Resize {
    pub config: DesktopConfig,
    pub matched: bool,
    connection: String,
}

struct State {
    original: DesktopConfig,
    current: DesktopConfig,
    matched: bool,
    lease: Option<Lease>,
    pending: Option<Resize>,
    applying: bool,
    last_resize: Option<Instant>,
    error: Option<String>,
}

pub(super) struct DesktopControl(Mutex<State>);

pub(super) type ApiResult<T> = Result<T, (&'static str, String)>;
fn conflict() -> (&'static str, String) {
    ("409 Conflict", IN_USE.into())
}
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl State {
    fn authorized(&self, connection: &str, now: Instant) -> bool {
        self.lease.as_ref().is_some_and(|lease| {
            lease.connection == connection && !lease.departed && now < lease.until
        })
    }
}

impl DesktopControl {
    pub fn new(config: DesktopConfig) -> Self {
        Self(Mutex::new(State {
            original: config.clone(),
            current: config,
            matched: false,
            lease: None,
            pending: None,
            applying: false,
            last_resize: None,
            error: None,
        }))
    }

    pub fn stats(&self) -> DesktopStats {
        let state = self.0.lock().unwrap();
        DesktopStats {
            config: state.current.clone(),
            matched: state.matched,
            occupied: state
                .lease
                .as_ref()
                .is_some_and(|l| Instant::now() < l.until),
            updating: state.pending.is_some() || state.applying,
            error: state.error.clone(),
            reconnect_seconds: RECONNECT_GRACE.as_secs(),
        }
    }

    pub fn claim(&self, client: &str, connection: &str) -> ApiResult<()> {
        self.claim_at(client, connection, Instant::now())
    }

    fn claim_at(&self, client: &str, connection: &str, now: Instant) -> ApiResult<()> {
        if !valid_id(client) || !valid_id(connection) {
            return Err(("400 Bad Request", "Invalid viewer identity".into()));
        }
        let mut state = self.0.lock().unwrap();
        if let Some(lease) = &state.lease
            && now < lease.until
            && (lease.client != client
                || (!lease.departed && lease.connection != connection)
                || (lease.departed && lease.connection == connection))
        {
            return Err(conflict());
        }
        if !state.authorized(connection, now) {
            state.pending = None;
        }
        state.lease = Some(Lease {
            client: client.into(),
            connection: connection.into(),
            until: now + RECONNECT_GRACE,
            departed: false,
        });
        Ok(())
    }

    pub fn authorized(&self, connection: Option<&str>) -> bool {
        connection.is_some_and(|id| self.0.lock().unwrap().authorized(id, Instant::now()))
    }

    pub fn heartbeat(&self, connection: &str) -> ApiResult<()> {
        let now = Instant::now();
        let mut state = self.0.lock().unwrap();
        if !state.authorized(connection, now) {
            return Err(conflict());
        }
        state.lease.as_mut().unwrap().until = now + RECONNECT_GRACE;
        Ok(())
    }

    pub fn release(&self, connection: &str) {
        let mut state = self.0.lock().unwrap();
        if state.authorized(connection, Instant::now()) {
            let lease = state.lease.as_mut().unwrap();
            lease.departed = true;
            lease.until = Instant::now() + RECONNECT_GRACE;
            state.pending = None;
        }
    }

    pub fn request_size(&self, connection: &str, size: Option<(u32, u32, u32)>) -> ApiResult<()> {
        let mut state = self.0.lock().unwrap();
        if !state.authorized(connection, Instant::now()) {
            return Err(conflict());
        }
        let matched = size.is_some();
        let mut config = state.original.clone();
        if let Some((width, height, scale)) = size {
            config.width = width;
            config.height = height;
            config.scale = scale;
        }
        config
            .validate()
            .map_err(|e| ("400 Bad Request", e.to_string()))?;
        // Both transports must support every accepted client size.
        if matched
            && (config.width.max(config.height) > 3840 || config.width.min(config.height) > 2160)
        {
            return Err((
                "400 Bad Request",
                "Display must fit within 3840×2160 or portrait".into(),
            ));
        }
        if state.applying || state.pending.is_some() {
            return Err((
                "429 Too Many Requests",
                "Display resize is already in progress".into(),
            ));
        }
        if config == state.current && matched == state.matched {
            return Ok(());
        }
        if state
            .last_resize
            .is_some_and(|at| at.elapsed() < Duration::from_millis(750))
        {
            return Err((
                "429 Too Many Requests",
                "Please wait before resizing again".into(),
            ));
        }
        state.last_resize = Some(Instant::now());
        state.error = None;
        state.pending = Some(Resize {
            config,
            matched,
            connection: connection.into(),
        });
        Ok(())
    }

    pub fn take_resize(&self) -> Option<Resize> {
        let mut state = self.0.lock().unwrap();
        let resize = state.pending.take()?;
        if !state.authorized(&resize.connection, Instant::now()) {
            return None;
        }
        state.applying = true;
        Some(resize)
    }

    pub fn resized(&self, resize: Resize, error: Option<String>) {
        let mut state = self.0.lock().unwrap();
        if error.is_none() {
            state.current = resize.config;
            state.matched = resize.matched;
        }
        // Keep the serialized session record bounded even with verbose IPC errors.
        state.error = error.map(|message| message.chars().take(600).collect());
        state.applying = false;
    }

    pub fn apply_resize<T>(
        &self,
        resize: Resize,
        mut apply: impl FnMut(&DesktopConfig) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        use anyhow::Context;
        let previous = self.stats().config;
        match apply(&resize.config) {
            Ok(value) => {
                self.resized(resize, None);
                Ok(value)
            }
            Err(error) => {
                let restored = apply(&previous)
                    .context("could not restore the extended display after resize failure");
                self.resized(resize, Some(format!("Could not resize display: {error:#}")));
                restored
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CLIENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PAGE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const OTHER: &str = "cccccccccccccccccccccccccccccccc";

    #[test]
    fn one_page_owns_all_transports_and_refresh_can_resume_after_departure() {
        let control = DesktopControl::new(DesktopConfig::default());
        control.claim(CLIENT, PAGE).unwrap();
        assert!(control.authorized(Some(PAGE)));
        assert!(!control.authorized(None));
        assert!(control.claim(OTHER, OTHER).is_err());
        // A duplicated tab with the same sessionStorage identity still cannot
        // displace a page that is actively displaying the desktop.
        assert!(control.claim(CLIENT, OTHER).is_err());
        control.heartbeat(PAGE).unwrap();
        control.release(PAGE);
        assert!(!control.authorized(Some(PAGE)));
        assert!(control.heartbeat(PAGE).is_err());
        assert!(control.claim(OTHER, OTHER).is_err());
        control.claim(CLIENT, OTHER).unwrap();
        control.release(PAGE); // A late old-page request cannot release the new page.
        assert!(control.authorized(Some(OTHER)));
    }

    #[test]
    fn lost_clients_expire_and_a_different_device_can_claim() {
        let control = DesktopControl::new(DesktopConfig::default());
        let now = Instant::now();
        control.claim_at(CLIENT, PAGE, now).unwrap();
        assert!(
            control
                .claim_at(
                    OTHER,
                    OTHER,
                    now + RECONNECT_GRACE - Duration::from_millis(1)
                )
                .is_err()
        );
        control
            .claim_at(OTHER, OTHER, now + RECONNECT_GRACE)
            .unwrap();
        assert!(!control.authorized(Some(PAGE)));
        assert!(control.authorized(Some(OTHER)));
    }

    #[test]
    fn claims_are_atomic() {
        let control = std::sync::Arc::new(DesktopControl::new(DesktopConfig::default()));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|n| {
                let control = control.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    control
                        .claim(&format!("{n:032x}"), &format!("{n:032x}"))
                        .is_ok()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|w| usize::from(w.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn resize_requires_ownership_is_bounded_and_rolls_back_on_failure() {
        let original = DesktopConfig::default();
        let control = DesktopControl::new(original.clone());
        assert!(control.request_size(PAGE, Some((1280, 800, 1))).is_err());
        control.claim(CLIENT, PAGE).unwrap();
        for size in [
            (0, 800, 1),
            (1280, 800, 3),
            (3840, 3840, 1),
            (2880, 2880, 1),
            (1281, 800, 2),
        ] {
            assert!(control.request_size(PAGE, Some(size)).is_err());
        }
        control.request_size(PAGE, Some((1280, 800, 1))).unwrap();
        let resize = control.take_resize().unwrap();
        let mut applied = Vec::new();
        let restored = control
            .apply_resize(resize, |config| {
                applied.push(config.clone());
                if config.width == 1280 {
                    anyhow::bail!("compositor rejected size");
                }
                Ok(config.width)
            })
            .unwrap();
        assert_eq!(restored, original.width);
        assert_eq!(applied.len(), 2);
        assert_eq!(control.stats().config, original);
        assert!(!control.stats().matched);
        assert!(
            control
                .stats()
                .error
                .unwrap()
                .contains("compositor rejected size")
        );
        assert!(!control.stats().updating);
    }

    #[test]
    fn pending_resize_is_cancelled_on_departure_and_host_size_can_be_restored() {
        let original = DesktopConfig::default();
        let control = DesktopControl::new(original.clone());
        control.claim(CLIENT, PAGE).unwrap();
        control.request_size(PAGE, Some((2560, 1600, 2))).unwrap();
        control.release(PAGE);
        assert!(control.take_resize().is_none());
        control.claim(CLIENT, OTHER).unwrap();
        control.0.lock().unwrap().last_resize = None;
        control.request_size(OTHER, Some((2560, 1600, 2))).unwrap();
        let request = control.take_resize().unwrap();
        control.apply_resize(request, |_| Ok(())).unwrap();
        assert!(control.stats().matched);
        control.0.lock().unwrap().last_resize = None;
        control.request_size(OTHER, None).unwrap();
        let request = control.take_resize().unwrap();
        control.apply_resize(request, |_| Ok(())).unwrap();
        assert_eq!(control.stats().config, original);
        assert!(!control.stats().matched);
    }
}
