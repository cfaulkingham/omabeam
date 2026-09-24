//! Bounded sender measurements. Socket writes do not prove browser receipt or
//! presentation; capture wait includes waiting for compositor damage.
use omabeam_capture::PixelMode;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

const TIMING_WINDOW: Duration = Duration::from_secs(5);
const MAX_TIMINGS: usize = 256;
const RATE_WINDOW: Duration = Duration::from_secs(2);
const RATE_BUCKET: Duration = Duration::from_millis(250);
const LOG_INTERVAL: Duration = Duration::from_secs(10);

/// One log line per interval, so a persistent fault cannot flood the live log.
#[derive(Default)]
pub(super) struct LogThrottle(Option<Instant>);

impl LogThrottle {
    pub(super) fn allow(&mut self, now: Instant) -> bool {
        let allowed = self
            .0
            .is_none_or(|last| now.duration_since(last) >= LOG_INTERVAL);
        if allowed {
            self.0 = Some(now);
        }
        allowed
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimingStats {
    pub samples: usize,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamDiagnostics {
    pub native_pixels: bool,
    pub capture_width: u32,
    pub capture_height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub jpeg_bytes: usize,
    pub capture_wait_ms: TimingStats,
    pub encode_ms: TimingStats,
    pub send_ms: TimingStats,
    pub outgoing_mbps: f64,
    pub bytes_sent: u64,
    pub frames_sent: u64,
    pub frames_skipped: u64,
    pub write_errors: u64,
    /// Failed JPEG encodes, counted once per frame: lazy ones and frames capture skipped.
    #[serde(default)]
    pub encode_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewerDiagnostics {
    pub id: u64,
    pub sent_fps: f64,
    pub outgoing_mbps: f64,
    pub bytes_sent: u64,
    pub frames_sent: u64,
    pub frames_skipped: u64,
    pub send_ms: TimingStats,
    pub frame_age_ms: Option<f64>,
    pub last_sent_ago_ms: Option<f64>,
}

pub(super) struct FrameMeasurement {
    pub pixel_mode: PixelMode,
    pub capture_width: u32,
    pub capture_height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub capture_wait: Duration,
    pub encode: Duration,
    /// Start of scaling/encoding, after the capturer returned the frame.
    pub encode_started_at: Instant,
}

impl Default for FrameMeasurement {
    fn default() -> Self {
        Self {
            pixel_mode: PixelMode::Logical,
            capture_width: 0,
            capture_height: 0,
            logical_width: 0,
            logical_height: 0,
            capture_wait: Duration::ZERO,
            encode: Duration::ZERO,
            encode_started_at: Instant::now(),
        }
    }
}

#[derive(Default)]
pub(super) struct Timings(VecDeque<(Instant, f64)>);

impl Timings {
    pub(super) fn record(&mut self, now: Instant, elapsed: Duration) {
        self.0.push_back((now, elapsed.as_secs_f64() * 1000.0));
        while self.0.len() > MAX_TIMINGS
            || self
                .0
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) >= TIMING_WINDOW)
        {
            self.0.pop_front();
        }
    }

    pub(super) fn stats(&self, now: Instant) -> TimingStats {
        let mut values: Vec<_> = self
            .0
            .iter()
            .filter(|(at, _)| now.duration_since(*at) < TIMING_WINDOW)
            .map(|(_, value)| *value)
            .collect();
        values.sort_by(f64::total_cmp);
        let percentile =
            |p: usize| (!values.is_empty()).then(|| values[(values.len() * p).div_ceil(100) - 1]);
        TimingStats {
            samples: values.len(),
            p50: percentile(50),
            p95: percentile(95),
        }
    }
}

struct Bucket {
    at: Instant,
    bytes: u64,
    frames: u64,
}

pub(super) struct Rate {
    started: Instant,
    buckets: VecDeque<Bucket>,
}

impl Rate {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            started: now,
            buckets: VecDeque::new(),
        }
    }

    pub(super) fn record(&mut self, now: Instant, bytes: u64, frames: u64) {
        while self
            .buckets
            .front()
            .is_some_and(|b| now.duration_since(b.at) >= RATE_WINDOW)
        {
            self.buckets.pop_front();
        }
        if self
            .buckets
            .back()
            .is_none_or(|b| now.duration_since(b.at) >= RATE_BUCKET)
        {
            self.buckets.push_back(Bucket {
                at: now,
                bytes: 0,
                frames: 0,
            });
        }
        let bucket = self.buckets.back_mut().unwrap();
        bucket.bytes += bytes;
        bucket.frames += frames;
    }

    pub(super) fn values(&self, now: Instant) -> (f64, f64) {
        let (bytes, frames) = self
            .buckets
            .iter()
            .filter(|b| now.duration_since(b.at) < RATE_WINDOW)
            .fold((0, 0), |(bytes, frames), b| {
                (bytes + b.bytes, frames + b.frames)
            });
        let seconds = now
            .duration_since(self.started)
            .clamp(RATE_BUCKET, RATE_WINDOW)
            .as_secs_f64();
        (
            bytes as f64 * 8.0 / seconds / 1_000_000.0,
            frames as f64 / seconds,
        )
    }
}

struct Delivery {
    bytes: u64,
    frames: u64,
    skipped: u64,
    rate: Rate,
    send: Timings,
    frame_age: Option<Duration>,
    last_sent: Option<Instant>,
}

impl Delivery {
    fn new(now: Instant) -> Self {
        Self {
            bytes: 0,
            frames: 0,
            skipped: 0,
            rate: Rate::new(now),
            send: Timings::default(),
            frame_age: None,
            last_sent: None,
        }
    }

    fn record(&mut self, now: Instant, measurement: &SendMeasurement) {
        self.bytes += measurement.bytes;
        self.skipped += measurement.skipped;
        self.rate
            .record(now, measurement.bytes, u64::from(measurement.completed));
        if measurement.completed {
            self.frames += 1;
            self.send.record(now, measurement.elapsed);
            self.frame_age = Some(measurement.frame_age);
            self.last_sent = Some(now);
        }
    }
}

pub(super) struct SendMeasurement {
    pub bytes: u64,
    pub skipped: u64,
    pub elapsed: Duration,
    pub frame_age: Duration,
    pub completed: bool,
}

pub(super) struct DiagnosticsState {
    latest: FrameMeasurement,
    jpeg_bytes: usize,
    capture_wait: Timings,
    encode: Timings,
    delivery: Delivery,
    write_errors: u64,
    encode_errors: u64,
    next_viewer: u64,
    viewers: BTreeMap<u64, Delivery>,
}

impl DiagnosticsState {
    pub fn new(now: Instant) -> Self {
        Self {
            latest: FrameMeasurement::default(),
            jpeg_bytes: 0,
            capture_wait: Timings::default(),
            encode: Timings::default(),
            delivery: Delivery::new(now),
            write_errors: 0,
            encode_errors: 0,
            next_viewer: 1,
            viewers: BTreeMap::new(),
        }
    }

    pub fn publish(&mut self, now: Instant, jpeg_bytes: usize, measurement: FrameMeasurement) {
        self.capture_wait.record(now, measurement.capture_wait);
        if jpeg_bytes > 0 {
            self.encode.record(now, measurement.encode);
        }
        self.jpeg_bytes = jpeg_bytes;
        self.latest = measurement;
    }

    pub fn jpeg_encoded(&mut self, now: Instant, bytes: usize, elapsed: Duration) {
        self.jpeg_bytes = bytes;
        self.encode.record(now, elapsed);
    }

    pub fn jpeg_failed(&mut self) {
        self.encode_errors += 1;
    }

    pub fn add_viewer(&mut self, now: Instant) -> u64 {
        let id = self.next_viewer;
        self.next_viewer += 1;
        self.viewers.insert(id, Delivery::new(now));
        id
    }

    pub fn remove_viewer(&mut self, id: u64) {
        self.viewers.remove(&id);
    }

    pub fn record_send(&mut self, id: u64, now: Instant, measurement: SendMeasurement) {
        self.delivery.record(now, &measurement);
        if !measurement.completed {
            self.write_errors += 1;
        }
        if let Some(viewer) = self.viewers.get_mut(&id) {
            viewer.record(now, &measurement);
        }
    }

    pub fn stats(&self, now: Instant) -> StreamDiagnostics {
        StreamDiagnostics {
            native_pixels: self.latest.pixel_mode == PixelMode::Native,
            capture_width: self.latest.capture_width,
            capture_height: self.latest.capture_height,
            logical_width: self.latest.logical_width,
            logical_height: self.latest.logical_height,
            jpeg_bytes: self.jpeg_bytes,
            capture_wait_ms: self.capture_wait.stats(now),
            encode_ms: self.encode.stats(now),
            send_ms: self.delivery.send.stats(now),
            outgoing_mbps: self.delivery.rate.values(now).0,
            bytes_sent: self.delivery.bytes,
            frames_sent: self.delivery.frames,
            frames_skipped: self.delivery.skipped,
            write_errors: self.write_errors,
            encode_errors: self.encode_errors,
        }
    }

    pub fn viewers(&self, now: Instant) -> Vec<ViewerDiagnostics> {
        self.viewers
            .iter()
            .map(|(&id, v)| {
                let (outgoing_mbps, sent_fps) = v.rate.values(now);
                ViewerDiagnostics {
                    id,
                    sent_fps,
                    outgoing_mbps,
                    bytes_sent: v.bytes,
                    frames_sent: v.frames,
                    frames_skipped: v.skipped,
                    send_ms: v.send.stats(now),
                    frame_age_ms: v.frame_age.map(|d| d.as_secs_f64() * 1000.0),
                    last_sent_ago_ms: v
                        .last_sent
                        .map(|at| now.duration_since(at).as_secs_f64() * 1000.0),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timings_report_percentiles_expire_and_remain_bounded() {
        let now = Instant::now();
        let mut timings = Timings::default();
        assert_eq!(timings.stats(now), TimingStats::default());
        for value in 1..=100 {
            timings.record(now, Duration::from_millis(value));
        }
        let stats = timings.stats(now);
        assert_eq!(stats.samples, 100);
        assert_eq!(stats.p50, Some(50.0));
        assert_eq!(stats.p95, Some(95.0));
        assert_eq!(timings.stats(now + TIMING_WINDOW), TimingStats::default());
        for _ in 0..1000 {
            timings.record(now, Duration::from_millis(1));
        }
        assert_eq!(timings.0.len(), MAX_TIMINGS);
    }

    #[test]
    fn log_throttle_allows_one_line_per_interval() {
        let now = Instant::now();
        let mut log = LogThrottle::default();
        assert!(log.allow(now));
        assert!(!log.allow(now + Duration::from_secs(9)));
        assert!(log.allow(now + LOG_INTERVAL));
        assert!(!log.allow(now + LOG_INTERVAL + Duration::from_secs(1)));
    }

    #[test]
    fn bandwidth_buckets_keep_all_bytes_under_high_fanout_and_decay_when_idle() {
        let now = Instant::now();
        let mut rate = Rate::new(now);
        for ms in 0..2000 {
            for _ in 0..64 {
                rate.record(now + Duration::from_millis(ms), 1000, 1);
            }
        }
        assert!(rate.buckets.len() <= 8);
        let (mbps, fps) = rate.values(now + Duration::from_millis(1999));
        assert!((mbps - (128_000_000.0 * 8.0 / 1.999 / 1_000_000.0)).abs() < 0.001);
        assert!((fps - 128_000.0 / 1.999).abs() < 0.001);
        assert_eq!(rate.values(now + Duration::from_secs(4)), (0.0, 0.0));
        rate.record(now + Duration::from_secs(4), 125_000, 1);
        assert_eq!(rate.buckets.len(), 1);
        assert_eq!(rate.values(now + Duration::from_secs(4)), (0.5, 0.5));
    }

    #[test]
    fn viewer_delivery_is_isolated_and_totals_survive_disconnects_and_failed_writes() {
        let now = Instant::now();
        let mut d = DiagnosticsState::new(now);
        let fast = d.add_viewer(now);
        let slow = d.add_viewer(now);
        let sample = |bytes, skipped, completed| SendMeasurement {
            bytes,
            skipped,
            completed,
            elapsed: Duration::from_millis(20),
            frame_age: Duration::from_millis(30),
        };
        let at = now + Duration::from_secs(1);
        d.record_send(fast, at, sample(1000, 0, true));
        d.record_send(slow, at, sample(500, 8, false));
        let viewers = d.viewers(at);
        assert_eq!(viewers[0].frames_sent, 1);
        assert_eq!(viewers[0].frames_skipped, 0);
        assert_eq!(viewers[0].frame_age_ms, Some(30.0));
        assert_eq!(viewers[1].bytes_sent, 500); // partial write is counted
        assert_eq!(viewers[1].frames_sent, 0);
        assert_eq!(viewers[1].frames_skipped, 8);
        assert_eq!(viewers[1].frame_age_ms, None);
        assert_eq!(viewers[1].send_ms.samples, 0);
        d.remove_viewer(slow);
        d.remove_viewer(fast);
        assert!(d.viewers(at).is_empty());
        let stats = d.stats(at);
        assert_eq!(stats.bytes_sent, 1500);
        assert_eq!(stats.frames_sent, 1);
        assert_eq!(stats.frames_skipped, 8);
        assert_eq!(stats.write_errors, 1);
        assert_eq!(stats.send_ms.p95, Some(20.0));
        assert_eq!(d.stats(now + Duration::from_secs(8)).outgoing_mbps, 0.0);
    }
}
