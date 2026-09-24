use super::diagnostics::{
    DiagnosticsState, FrameMeasurement, LogThrottle, SendMeasurement, StreamDiagnostics,
    ViewerDiagnostics,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamStats {
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub uptime: u64,
    pub viewers: usize,
    pub source: String,
    pub state: String,
    pub error: Option<String>,
    #[serde(default)]
    pub diagnostics: StreamDiagnostics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webrtc: Option<super::WebRtcStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<super::desktop::DesktopStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cast: Option<super::cast::CastStats>,
}

pub(super) struct FrameData {
    pub jpeg: Arc<[u8]>,
    pub raw: Option<Arc<super::h264::RawFrame>>,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub encode_started_at: Instant,
    /// The generation whose lazy JPEG encode failed.
    jpeg_failed: Option<u64>,
    diagnostics: DiagnosticsState,
    times: VecDeque<Instant>,
    started: Instant,
    pub ended: Option<Instant>,
    pub error: Option<String>,
}

pub(super) struct FrameState {
    pub inner: Mutex<FrameData>,
    pub tick: Condvar,
    pub viewers: AtomicUsize,
    pub cast_viewers: AtomicUsize,
    pub rtc: Mutex<Option<Arc<super::webrtc::Service>>>,
    pub desktop: Option<Arc<super::desktop::DesktopControl>>,
    /// Serializes lazy JPEG encodes and rate-limits their failure log.
    jpeg_encode: Mutex<LogThrottle>,
    source: String,
}

/// The latest frame could not be JPEG-encoded. Streams skip to the next
/// generation; snapshots answer 500.
#[derive(Debug)]
pub(super) struct EncodeFailed {
    pub generation: u64,
}

impl FrameState {
    /// Synchronize external predicate changes with waiters so a notification
    /// cannot land between checking a predicate and entering the wait.
    pub fn wake(&self) {
        let _data = self.inner.lock().unwrap();
        self.tick.notify_all();
    }

    pub fn new(source: String) -> Self {
        Self {
            inner: Mutex::new(FrameData {
                jpeg: Arc::from([]),
                raw: None,
                generation: 0,
                width: 0,
                height: 0,
                encode_started_at: Instant::now(),
                jpeg_failed: None,
                diagnostics: DiagnosticsState::new(Instant::now()),
                times: VecDeque::new(),
                started: Instant::now(),
                ended: None,
                error: None,
            }),
            tick: Condvar::new(),
            viewers: AtomicUsize::new(0),
            cast_viewers: AtomicUsize::new(0),
            rtc: Mutex::new(None),
            desktop: None,
            jpeg_encode: Mutex::new(LogThrottle::default()),
            source,
        }
    }
    #[cfg(test)]
    pub fn publish(&self, jpeg: Vec<u8>, width: u32, height: u32, measurement: FrameMeasurement) {
        self.publish_raw(jpeg, width, height, measurement, None);
    }
    pub fn publish_raw(
        &self,
        jpeg: Vec<u8>,
        width: u32,
        height: u32,
        measurement: FrameMeasurement,
        raw: Option<Arc<super::h264::RawFrame>>,
    ) {
        let mut data = self.inner.lock().unwrap();
        if data.ended.is_some() {
            return;
        }
        let now = Instant::now();
        data.encode_started_at = measurement.encode_started_at;
        data.diagnostics.publish(now, jpeg.len(), measurement);
        data.jpeg = jpeg.into();
        data.raw = raw;
        data.generation += 1;
        data.width = width;
        data.height = height;
        data.times.push_back(now);
        while data.times.len() > 2 && now.duration_since(data.times[0]) > Duration::from_secs(2) {
            data.times.pop_front();
        }
        drop(data);
        self.tick.notify_all();
    }
    pub fn fail(&self, error: String) {
        let mut data = self.inner.lock().unwrap();
        data.jpeg = Arc::from([]);
        data.raw = None;
        data.ended = Some(Instant::now());
        data.error = Some(error);
        drop(data);
        self.tick.notify_all();
    }
    /// Capture skipped a frame whose eager JPEG failed. It counts like a failed
    /// lazy encode; the frame was never published, so it counts only once.
    pub fn jpeg_skipped(&self) {
        self.inner.lock().unwrap().diagnostics.jpeg_failed();
    }
    pub fn viewer_count(&self) -> usize {
        self.viewers.load(Ordering::SeqCst)
            + self.cast_viewers.load(Ordering::SeqCst)
            + self
                .rtc
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, |rtc| rtc.connected())
    }
    pub fn authorized(&self, connection: Option<&str>) -> bool {
        self.desktop
            .as_ref()
            .is_none_or(|d| d.authorized(connection))
    }
    pub fn stats(&self) -> StreamStats {
        let data = self.inner.lock().unwrap();
        let now = Instant::now();
        let fps = measured_fps(&data.times, now);
        StreamStats {
            fps: if data.ended.is_some() { 0.0 } else { fps },
            width: data.width,
            height: data.height,
            frames: data.generation,
            uptime: data.started.elapsed().as_secs(),
            // A transport handoff can briefly have both sockets open for the
            // same extended-display client; it is still one viewer.
            viewers: if self.desktop.is_some() {
                self.viewer_count().min(1)
            } else {
                self.viewer_count()
            },
            source: self.desktop.as_ref().map_or_else(
                || self.source.clone(),
                |desktop| {
                    let config = desktop.stats().config;
                    format!(
                        "Extended desktop {}×{} · {}",
                        config.width,
                        config.height,
                        config.position.label()
                    )
                },
            ),
            state: if data.ended.is_some() {
                "ended"
            } else {
                "live"
            }
            .into(),
            error: data.error.clone(),
            diagnostics: data.diagnostics.stats(now),
            webrtc: self.rtc.lock().unwrap().as_ref().map(|rtc| rtc.stats()),
            desktop: self.desktop.as_ref().map(|d| d.stats()),
            cast: None,
        }
    }

    /// Cache at most the latest JPEG. RTC-only viewers do not run the JPEG
    /// encoder; snapshots and fallback connections request it on demand.
    pub fn jpeg_frame(&self) -> Result<(Arc<[u8]>, u64, Instant), EncodeFailed> {
        let mut failure_log = self.jpeg_encode.lock().unwrap();
        let (raw, generation, at) = {
            let data = self.inner.lock().unwrap();
            if !data.jpeg.is_empty() || data.ended.is_some() {
                return Ok((data.jpeg.clone(), data.generation, data.encode_started_at));
            }
            // Each viewer would otherwise repeat the failing encode.
            if data.jpeg_failed == Some(data.generation) {
                return Err(EncodeFailed {
                    generation: data.generation,
                });
            }
            (data.raw.clone(), data.generation, data.encode_started_at)
        };
        let Some(raw) = raw else {
            return Ok((Arc::from([]), generation, at));
        };
        let started = Instant::now();
        let encoded = raw
            .frame
            .jpeg_with_mode(
                raw.config.quality,
                raw.config.max_width,
                raw.config.pixel_mode,
            )
            .map(|(jpeg, _, _)| Arc::<[u8]>::from(jpeg));
        let mut data = self.inner.lock().unwrap();
        if data.ended.is_some() {
            return Ok((Arc::from([]), generation, at));
        }
        let jpeg = match encoded {
            Ok(jpeg) => jpeg,
            Err(error) => {
                data.diagnostics.jpeg_failed();
                if data.generation == generation {
                    data.jpeg_failed = Some(generation);
                }
                drop(data);
                if failure_log.allow(Instant::now()) {
                    eprintln!("live share: could not encode a JPEG frame: {error:#}");
                }
                return Err(EncodeFailed { generation });
            }
        };
        if data.generation == generation {
            data.jpeg = jpeg.clone();
            data.encode_started_at = started;
            // Capture publication records capture timings separately. Lazy JPEG
            // timings are recorded here without counting another capture.
            data.diagnostics
                .jpeg_encoded(Instant::now(), jpeg.len(), started.elapsed());
        }
        Ok((jpeg, generation, started))
    }

    pub fn viewer_diagnostics(&self) -> Vec<ViewerDiagnostics> {
        self.inner
            .lock()
            .unwrap()
            .diagnostics
            .viewers(Instant::now())
    }
}

pub(super) fn measured_fps(times: &VecDeque<Instant>, now: Instant) -> f64 {
    match (times.front(), times.back()) {
        (Some(first), Some(last))
            if times.len() > 1 && now.duration_since(*last) < Duration::from_secs(3) =>
        {
            (times.len() - 1) as f64 / now.duration_since(*first).as_secs_f64().max(0.001)
        }
        _ => 0.0,
    }
}

pub(super) struct Viewer<'a> {
    frames: &'a FrameState,
    id: u64,
}
impl<'a> Viewer<'a> {
    pub fn new(frames: &'a FrameState) -> Self {
        let id = frames
            .inner
            .lock()
            .unwrap()
            .diagnostics
            .add_viewer(Instant::now());
        frames.viewers.fetch_add(1, Ordering::SeqCst);
        frames.tick.notify_all();
        Self { frames, id }
    }

    pub fn record_send(&self, measurement: SendMeasurement) {
        self.frames.inner.lock().unwrap().diagnostics.record_send(
            self.id,
            Instant::now(),
            measurement,
        );
    }
}
impl Drop for Viewer<'_> {
    fn drop(&mut self) {
        self.frames
            .inner
            .lock()
            .unwrap()
            .diagnostics
            .remove_viewer(self.id);
        self.frames.viewers.fetch_sub(1, Ordering::SeqCst);
        self.frames.tick.notify_all();
    }
}
