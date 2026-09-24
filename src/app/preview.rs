//! Two bounded workers own preview capture sessions: one for the selected
//! source and one for window thumbnails, so a slow window cannot hold up the
//! preview. No preview is written to disk or served over HTTP, and a result is
//! only displayed for its exact key.
use gpui_kit::{Image, ImageFormat};
use omabeam_capture::{
    AlphaMode, CaptureOptions, CaptureSession, CaptureTarget, CapturedFrame, PixelMode,
    StillCapturer,
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

const PREVIEW_STOPPED: &str = "Preview stopped unexpectedly and will restart.";
/// Thumbnails are requested one at a time and at most this often, the pace
/// they had while they rode along with preview frames.
const THUMBNAIL_PACE: Duration = Duration::from_millis(750);
/// A window's thumbnail is requested at most this often.
const THUMBNAIL_RETRY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreviewKey {
    pub target: CaptureTarget,
    pub cursor: bool,
    pub quality: u8,
    pub max_width: Option<u32>,
    pub pixel_mode: PixelMode,
    pub screenshot: bool,
}

pub(super) struct PreviewFrame {
    pub image: Arc<Image>,
    pub width: u32,
    pub height: u32,
}

impl PreviewFrame {
    fn encode(frame: CapturedFrame, key: &PreviewKey) -> anyhow::Result<Self> {
        let (format, bytes, width, height) = if key.screenshot {
            (
                ImageFormat::Png,
                frame.png()?,
                frame.image.width(),
                frame.image.height(),
            )
        } else {
            let (bytes, width, height) =
                frame.jpeg_with_mode(key.quality, key.max_width, key.pixel_mode)?;
            (ImageFormat::Jpeg, bytes, width, height)
        };
        Ok(Self {
            image: Arc::new(Image::from_bytes(format, bytes)),
            width,
            height,
        })
    }
}

/// Channels to a worker thread started by [`spawn`].
pub(super) struct Channels<J, R> {
    jobs: SyncSender<J>,
    results: Receiver<R>,
}

/// Start a named thread that answers jobs in order. Both channels hold one
/// message, so jobs cannot pile up behind a slow answer.
pub(super) fn spawn<J: Send + 'static, R: Send + 'static>(
    name: &str,
    mut answer: impl FnMut(J) -> R + Send + 'static,
) -> std::io::Result<Channels<J, R>> {
    let (jobs, receiver) = mpsc::sync_channel::<J>(1);
    let (sender, results) = mpsc::sync_channel(1);
    thread::Builder::new().name(name.into()).spawn(move || {
        while let Ok(job) = receiver.recv() {
            if sender.send(answer(job)).is_err() {
                break;
            }
        }
    })?;
    Ok(Channels { jobs, results })
}

/// Results a worker has finished, and whether its thread has gone away.
#[derive(Debug, PartialEq)]
pub(super) struct Drained<R> {
    pub results: Vec<R>,
    pub disconnected: bool,
}

/// Take every finished result without blocking. Unlike `try_iter`, this also
/// reports a worker that stopped, for example by panicking mid-job.
pub(super) fn drain<R>(results: &Receiver<R>) -> Drained<R> {
    let mut drained = Drained {
        results: Vec::new(),
        disconnected: false,
    };
    loop {
        match results.try_recv() {
            Ok(result) => drained.results.push(result),
            Err(TryRecvError::Empty) => return drained,
            Err(TryRecvError::Disconnected) => {
                drained.disconnected = true;
                return drained;
            }
        }
    }
}

/// What happened to a job offered to a [`Worker`].
#[derive(Debug, PartialEq)]
pub(super) enum Offer {
    /// The worker took the job.
    Taken,
    /// A job is already waiting for the worker.
    Busy,
    /// The worker had stopped; the next offer starts a new one.
    Stopped,
    /// A new worker thread could not be started.
    Failed(String),
}

/// A worker thread, started on demand and again after it stops, and the jobs
/// it has taken but not answered. Free of GPUI so it can be tested.
pub(super) struct Worker<J, R> {
    start: Box<dyn Fn() -> std::io::Result<Channels<J, R>>>,
    channels: Option<Channels<J, R>>,
    outstanding: usize,
    /// The last job the running thread took.
    last: Option<J>,
}

impl<J, R> Worker<J, R> {
    pub fn new(start: impl Fn() -> std::io::Result<Channels<J, R>> + 'static) -> Self {
        Self {
            start: Box::new(start),
            channels: None,
            outstanding: 0,
            last: None,
        }
    }

    /// Start the thread unless it is running.
    pub fn start(&mut self) -> std::io::Result<&Channels<J, R>> {
        Ok(match self.channels {
            Some(ref channels) => channels,
            None => self.channels.insert((self.start)()?),
        })
    }

    /// Whether a job the worker took is still unanswered.
    pub fn pending(&self) -> bool {
        self.outstanding > 0
    }

    /// The last job the worker took; none once it stops.
    pub fn last_job(&self) -> Option<&J> {
        self.last.as_ref()
    }

    /// Offer a job without blocking, starting the thread first if needed.
    pub fn offer(&mut self, job: J) -> Offer
    where
        J: Clone,
    {
        let sent = match self.start() {
            Ok(channels) => channels.jobs.try_send(job.clone()),
            Err(error) => return Offer::Failed(error.to_string()),
        };
        match sent {
            Ok(()) => {
                self.outstanding += 1;
                self.last = Some(job);
                Offer::Taken
            }
            Err(TrySendError::Full(_)) => Offer::Busy,
            Err(TrySendError::Disconnected(_)) => {
                self.stop();
                Offer::Stopped
            }
        }
    }

    /// Take finished results. A stopped thread is dropped along with its
    /// unanswered jobs, so the next offer starts a new one.
    pub fn drain(&mut self) -> Drained<R> {
        let Some(channels) = &self.channels else {
            return Drained {
                results: Vec::new(),
                disconnected: false,
            };
        };
        let drained = drain(&channels.results);
        self.outstanding = self.outstanding.saturating_sub(drained.results.len());
        if drained.disconnected {
            self.stop();
        }
        drained
    }

    fn stop(&mut self) {
        self.channels = None;
        self.outstanding = 0;
        self.last = None;
    }
}

pub(super) struct PreviewResult {
    pub key: Option<PreviewKey>,
    pub frame: Result<Option<PreviewFrame>, String>,
}

pub(super) struct ThumbnailResult {
    pub id: String,
    pub frame: Result<PreviewFrame, String>,
}

/// Captures the selected source; a `None` job releases its session.
pub(super) type PreviewWorker = Worker<Option<PreviewKey>, PreviewResult>;
/// Captures window thumbnails by stable ID.
pub(super) type ThumbnailWorker = Worker<String, ThumbnailResult>;

pub(super) fn preview_worker(demo: bool) -> PreviewWorker {
    Worker::new(move || spawn("omabeam-preview", preview_answer(demo)))
}

pub(super) fn thumbnail_worker(demo: bool) -> ThumbnailWorker {
    Worker::new(move || {
        spawn(
            "omabeam-thumbnails",
            thumbnail_answer(capture_thumbnail(demo)),
        )
    })
}

/// Answers preview jobs, keeping the capture session while the key is unchanged.
fn preview_answer(demo: bool) -> impl FnMut(Option<PreviewKey>) -> PreviewResult {
    let mut current: Option<(PreviewKey, CaptureSession)> = None;
    move |key| {
        let frame = (|| -> anyhow::Result<Option<PreviewFrame>> {
            let Some(key) = &key else {
                current = None;
                return Ok(None);
            };
            if demo {
                return PreviewFrame::encode(omabeam_capture::demo_frame(0), key).map(Some);
            }
            if current.as_ref().is_none_or(|(old, _)| old != key) {
                current = None;
                let mut session =
                    CaptureSession::with_options(key.target.clone(), capture_options(key))?;
                let frame = session.capture()?;
                current = Some((key.clone(), session));
                return PreviewFrame::encode(frame, key).map(Some);
            }
            current
                .as_mut()
                .unwrap()
                .1
                .next_frame(Duration::from_millis(120))?
                .map(|frame| PreviewFrame::encode(frame, key))
                .transpose()
        })()
        .map_err(|err| format!("{err:#}"));
        if frame.is_err() {
            current = None;
        }
        PreviewResult { key, frame }
    }
}

/// A thumbnail is a small JPEG stream preview of a window, without the cursor.
fn thumbnail_key(id: String) -> PreviewKey {
    PreviewKey {
        target: CaptureTarget::Toplevel(id),
        cursor: false,
        quality: 55,
        max_width: Some(256),
        pixel_mode: PixelMode::Logical,
        screenshot: false,
    }
}

/// `capture_options` of every `thumbnail_key`, for the capturer that serves them all.
const THUMBNAIL_OPTIONS: CaptureOptions = CaptureOptions {
    cursor: false,
    alpha: AlphaMode::Opaque,
};

/// Answers thumbnail jobs with frames from `capture`.
fn thumbnail_answer(
    mut capture: impl FnMut(&str) -> anyhow::Result<CapturedFrame>,
) -> impl FnMut(String) -> ThumbnailResult {
    move |id| {
        let key = thumbnail_key(id.clone());
        let frame = capture(&id)
            .and_then(|frame| PreviewFrame::encode(frame, &key))
            .map_err(|err| err.to_string());
        ThumbnailResult { id, frame }
    }
}

/// Screenshot previews are PNG, which keeps transparency. Stream previews are
/// JPEG, which composites over black anyway, so they take opaque frames.
fn capture_options(key: &PreviewKey) -> CaptureOptions {
    CaptureOptions {
        cursor: key.cursor,
        alpha: if key.screenshot {
            AlphaMode::Straight
        } else {
            AlphaMode::Opaque
        },
    }
}

fn capture_thumbnail(demo: bool) -> impl FnMut(&str) -> anyhow::Result<CapturedFrame> {
    // One connection serves every thumbnail.
    let mut stills = StillCapturer::new(THUMBNAIL_OPTIONS);
    move |id: &str| {
        if demo {
            return Ok(omabeam_capture::demo_frame(id.bytes().map(u32::from).sum()));
        }
        stills.capture(CaptureTarget::Toplevel(id.into()))
    }
}

/// The next window to capture a thumbnail of, if one is due. Windows take
/// turns, requests are `THUMBNAIL_PACE` apart, and a window waits
/// `THUMBNAIL_RETRY` after its last attempt. Most ticks fall within the pace,
/// so `candidates` lists the windows only after that check.
fn pick_thumbnail(
    candidates: impl FnOnce() -> Vec<String>,
    turn: &mut usize,
    attempts: &HashMap<String, Instant>,
    now: Instant,
) -> Option<String> {
    let since = |attempt: &Instant| now.saturating_duration_since(*attempt);
    if attempts.values().any(|t| since(t) < THUMBNAIL_PACE) {
        return None;
    }
    let candidates = candidates();
    if candidates.is_empty() {
        return None;
    }
    let id = &candidates[*turn % candidates.len()];
    *turn = turn.wrapping_add(1);
    attempts
        .get(id)
        .is_none_or(|t| since(t) > THUMBNAIL_RETRY)
        .then(|| id.clone())
}

/// Whether to offer the preview worker `key` now: one job at a time, every
/// `interval`. `None` releases the worker's session, so it is sent once
/// rather than every interval, where each answer would redraw the picker.
fn preview_due(
    worker: &PreviewWorker,
    key: &Option<PreviewKey>,
    waited: Duration,
    interval: Duration,
) -> bool {
    let released = key.is_none() && worker.last_job() == Some(&None);
    !released && !worker.pending() && waited >= interval
}

impl super::OmaBeam {
    fn desired_preview_key(&self) -> Option<PreviewKey> {
        let target = self.capture_request()?.ok()?.target().ok()?;
        Some(PreviewKey {
            target,
            cursor: !self.picker && !self.screenshot_mode && self.live_config.cursor,
            quality: self.live_config.quality,
            max_width: self.live_config.max_width,
            pixel_mode: self.live_config.pixel_mode,
            screenshot: self.picker || self.screenshot_mode,
        })
    }

    pub(super) fn can_confirm(&self) -> bool {
        if self.page == super::Page::Extend {
            return !self.picker
                && !self.screenshot_mode
                && self.snapshot().is_some_and(|snapshot| {
                    self.desktop_config.placement(&snapshot.monitors).is_ok()
                });
        }
        if self.picker {
            return self.portal_selection().is_some();
        }
        self.preview_key.is_some()
            && self.preview_key == self.desired_preview_key()
            && self.preview_frame.is_some()
            && self.preview_error.is_none()
    }

    pub(super) fn update_previews(&mut self, cx: &mut gpui_kit::Context<Self>) {
        if self.busy {
            return;
        }
        let key = self.desired_preview_key();
        let mut changed = false;
        if key != self.preview_key {
            if let Some(frame) = self.preview_frame.take() {
                frame.image.remove_asset(cx);
            }
            self.preview_key = key.clone();
            self.preview_error = None;
            self.preview_updated = std::time::Instant::now() - Duration::from_secs(10);
            changed = true;
        }
        if key.is_none() {
            let error = self
                .capture_request()
                .and_then(Result::err)
                .map(|e| e.to_string());
            if self.preview_error != error {
                self.preview_error = error;
                changed = true;
            }
        }
        let previews = self.preview_worker.drain();
        for result in previews.results {
            if result.key == key {
                match result.frame {
                    Ok(Some(frame)) => {
                        if let Some(old) = self.preview_frame.take() {
                            if old.image.id() != frame.image.id() {
                                old.image.remove_asset(cx);
                            }
                        }
                        self.preview_frame = Some(frame);
                        self.preview_error = None;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.show_preview_error(format!("Preview unavailable. {error}"), cx);
                    }
                }
                changed = true;
            }
        }
        // drain() dropped a stopped worker; the next job starts a new one.
        if previews.disconnected {
            self.show_preview_error(PREVIEW_STOPPED.into(), cx);
            changed = true;
        }
        // A stopped thumbnail worker is replaced the same way, without an error.
        for ThumbnailResult { id, frame } in self.thumbnail_worker.drain().results {
            if let Some(old) = self.thumbnails.remove(&id) {
                old.remove_asset(cx);
            }
            if self
                .snapshot()
                .is_some_and(|s| s.visible_clients().any(|c| c.stable_id == id))
                && let Ok(frame) = frame
            {
                if self.thumbnails.len() >= 48 {
                    let oldest = self
                        .thumbnails
                        .keys()
                        .min_by_key(|id| self.thumbnail_attempts.get(*id))
                        .cloned();
                    if let Some(oldest) = oldest
                        && let Some(image) = self.thumbnails.remove(&oldest)
                    {
                        image.remove_asset(cx);
                    }
                }
                self.thumbnails.insert(id, frame.image);
            }
            changed = true;
        }
        let alive: Vec<String> = self
            .snapshot()
            .map(|s| s.visible_clients().map(|c| c.stable_id.clone()).collect())
            .unwrap_or_default();
        let expired: Vec<String> = self
            .thumbnails
            .keys()
            .filter(|id| !alive.contains(id))
            .cloned()
            .collect();
        for id in expired {
            if let Some(image) = self.thumbnails.remove(&id) {
                image.remove_asset(cx);
            }
            self.thumbnail_attempts.remove(&id);
            changed = true;
        }
        self.thumbnail_attempts.retain(|id, _| alive.contains(id));
        let interval = if self.preview_error.is_some() {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(750)
        };
        let waited = self.preview_updated.elapsed();
        if preview_due(&self.preview_worker, &key, waited, interval) {
            // A stopped or unstartable worker is retried at the error interval.
            match self.preview_worker.offer(key) {
                Offer::Taken => self.preview_updated = Instant::now(),
                Offer::Busy => {}
                Offer::Stopped => {
                    self.show_preview_error(PREVIEW_STOPPED.into(), cx);
                    self.preview_updated = Instant::now();
                    changed = true;
                }
                Offer::Failed(error) => {
                    self.show_preview_error(format!("Could not start preview: {error}"), cx);
                    self.preview_updated = Instant::now();
                    changed = true;
                }
            }
        }
        // Thumbnails keep their own pace and never wait for the preview.
        if !self.thumbnail_worker.pending()
            && let Some(id) = self.next_thumbnail()
        {
            match self.thumbnail_worker.offer(id.clone()) {
                // A window whose thumbnail could not start waits for its next turn.
                Offer::Taken | Offer::Failed(_) => {
                    self.thumbnail_attempts.insert(id, Instant::now());
                }
                Offer::Busy | Offer::Stopped => {}
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Replace the preview frame with `error`; the view shows the error and
    /// "Retry preview" only when no frame is shown.
    fn show_preview_error(&mut self, error: String, cx: &mut gpui_kit::Context<Self>) {
        if let Some(old) = self.preview_frame.take() {
            old.image.remove_asset(cx);
        }
        self.preview_error = Some(error);
    }

    /// The next listed window whose thumbnail is due; see `pick_thumbnail`.
    fn next_thumbnail(&mut self) -> Option<String> {
        let mut turn = self.thumbnail_index;
        let list = || self.thumbnail_candidates();
        let next = pick_thumbnail(list, &mut turn, &self.thumbnail_attempts, Instant::now());
        self.thumbnail_index = turn;
        next
    }

    /// Up to 48 listed windows that can have thumbnails.
    fn thumbnail_candidates(&self) -> Vec<String> {
        let clients = match self.page {
            super::Page::Tiles => self.current_tiles(),
            super::Page::Windows => self.current_windows(),
            _ => Vec::new(),
        };
        clients
            .into_iter()
            .filter(|c| !c.stable_id.is_empty())
            .take(48)
            .map(|c| c.stable_id.clone())
            .collect()
    }

    /// "Retry preview": start a stopped worker, then ask for a frame now.
    pub(super) fn retry_preview(&mut self) {
        if let Err(error) = self.preview_worker.start() {
            self.preview_error = Some(format!("Could not start preview: {error}"));
        }
        self.preview_updated = Instant::now() - Duration::from_secs(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const WAIT: Duration = Duration::from_secs(3);

    /// Drain `worker` until `take` accepts what arrived; fail after `WAIT`.
    fn wait_for<J, R, T>(
        worker: &mut Worker<J, R>,
        mut take: impl FnMut(Drained<R>) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(value) = take(worker.drain()) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "the worker did not answer in time"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn next_result<J, R>(worker: &mut Worker<J, R>) -> R {
        wait_for(worker, |drained| drained.results.into_iter().next())
    }

    #[test]
    fn draining_tells_a_stopped_worker_from_an_idle_one() {
        let (sender, results) = mpsc::sync_channel(1);
        let idle = Drained {
            results: vec![],
            disconnected: false,
        };
        assert_eq!(drain(&results), idle);
        let worker = thread::spawn(move || {
            sender.send(7).unwrap();
            panic!("preview worker died");
        });
        assert!(worker.join().is_err());
        // What the worker sent before it stopped still arrives.
        let stopped = Drained {
            results: vec![7],
            disconnected: true,
        };
        assert_eq!(drain(&results), stopped);
    }

    #[test]
    fn a_worker_that_panics_mid_job_is_no_longer_pending_and_restarts() {
        let mut worker = Worker::new(|| {
            spawn("omabeam-test-worker", |job: u32| {
                if job == 1 {
                    panic!("preview worker died");
                }
                job
            })
        });
        assert_eq!(worker.offer(1), Offer::Taken);
        assert!(worker.pending());
        let drained = wait_for(&mut worker, |drained| {
            drained.disconnected.then_some(drained)
        });
        assert!(drained.results.is_empty());
        assert!(!worker.pending());
        // The next job starts a new thread.
        assert_eq!(worker.offer(2), Offer::Taken);
        assert_eq!(next_result(&mut worker), 2);
    }

    fn window_key(id: &str, max_width: u32) -> PreviewKey {
        PreviewKey {
            target: CaptureTarget::Toplevel(id.into()),
            cursor: false,
            quality: 55,
            max_width: Some(max_width),
            pixel_mode: PixelMode::Logical,
            screenshot: false,
        }
    }

    #[test]
    fn workers_tag_frames_with_the_requested_source_and_settings() {
        let mut previews = preview_worker(true);
        let mut thumbnails = thumbnail_worker(true);
        let first = window_key("first", 320);
        assert_eq!(previews.offer(Some(first.clone())), Offer::Taken);
        assert_eq!(thumbnails.offer("second".into()), Offer::Taken);
        let result = next_result(&mut previews);
        assert_eq!(result.key.as_ref(), Some(&first));
        let frame = result.frame.unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (320, 180));
        let thumbnail = next_result(&mut thumbnails);
        assert_eq!(thumbnail.id, "second");
        assert_eq!(thumbnail.frame.unwrap().width, 256);

        let second = window_key("second", 160);
        assert_eq!(previews.offer(Some(second.clone())), Offer::Taken);
        let result = next_result(&mut previews);
        assert_eq!(result.key, Some(second));
        assert_eq!(result.frame.unwrap().unwrap().width, 160);
        assert_eq!(previews.offer(None), Offer::Taken);
        assert!(next_result(&mut previews).frame.unwrap().is_none());
    }

    #[test]
    fn a_stalled_thumbnail_does_not_delay_the_preview() {
        let (entered, stalled) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new((entered, released)));
        let mut thumbnails = Worker::new(move || {
            let gate = gate.clone();
            spawn(
                "omabeam-test-thumbnails",
                thumbnail_answer(move |_: &str| {
                    let (entered, released) = &*gate.lock().unwrap();
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    Ok(omabeam_capture::demo_frame(0))
                }),
            )
        });
        let mut previews = preview_worker(true);
        assert_eq!(thumbnails.offer("unresponsive".into()), Offer::Taken);
        stalled.recv_timeout(WAIT).unwrap();
        // The preview keeps refreshing while the thumbnail capture is stuck.
        for width in [320, 160, 320] {
            assert!(!previews.pending());
            let key = window_key("first", width);
            assert_eq!(previews.offer(Some(key.clone())), Offer::Taken);
            let result = next_result(&mut previews);
            assert_eq!(result.key, Some(key));
            assert_eq!(result.frame.unwrap().unwrap().width, width);
        }
        assert!(thumbnails.drain().results.is_empty());
        assert!(thumbnails.pending());
        release.send(()).unwrap();
        let thumbnail = next_result(&mut thumbnails);
        assert_eq!(thumbnail.id, "unresponsive");
        assert_eq!(thumbnail.frame.unwrap().width, 256);
        assert!(!thumbnails.pending());
    }

    #[test]
    fn thumbnails_take_turns_at_their_old_pace_without_the_preview() {
        let windows = ["a", "b", "c"].map(String::from);
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut turn = 0;
        let mut attempts = HashMap::new();
        let mut pick = |attempts: &HashMap<String, Instant>, now| {
            pick_thumbnail(|| windows.to_vec(), &mut turn, attempts, now)
        };
        assert_eq!(pick(&attempts, at(0)).as_deref(), Some("a"));
        attempts.insert("a".to_string(), at(0));
        // One request per 750 ms, as when thumbnails rode along with the preview.
        assert_eq!(pick(&attempts, at(700)), None);
        assert_eq!(pick(&attempts, at(750)).as_deref(), Some("b"));
        attempts.insert("b".to_string(), at(750));
        assert_eq!(pick(&attempts, at(1500)).as_deref(), Some("c"));
        attempts.insert("c".to_string(), at(1500));
        // Each window waits 10 seconds after its last attempt; only "a" has.
        assert_eq!(pick(&attempts, at(9000)), None);
        let due: Vec<_> = (0..3).filter_map(|_| pick(&attempts, at(10_001))).collect();
        assert_eq!(due, ["a"]);
        assert_eq!(
            pick_thumbnail(Vec::new, &mut 0, &HashMap::new(), start),
            None
        );
    }

    #[test]
    fn thumbnail_ticks_within_the_pace_list_no_windows() {
        let start = Instant::now();
        let attempts = HashMap::from([("a".to_string(), start)]);
        let listed = std::cell::Cell::new(0);
        let list = || {
            listed.set(listed.get() + 1);
            vec!["a".to_string(), "b".to_string()]
        };
        let mut turn = 1;
        let soon = start + Duration::from_millis(100);
        assert_eq!(pick_thumbnail(list, &mut turn, &attempts, soon), None);
        assert_eq!(listed.get(), 0, "the pace is checked first");
        let later = start + THUMBNAIL_PACE;
        let next = pick_thumbnail(list, &mut turn, &attempts, later);
        assert_eq!((next.as_deref(), listed.get()), (Some("b"), 1));
    }

    #[test]
    fn a_released_preview_is_sent_none_only_once() {
        let mut previews = preview_worker(true);
        let interval = Duration::from_millis(750);
        let due = |previews: &PreviewWorker, key: &Option<PreviewKey>, waited| {
            preview_due(previews, key, Duration::from_millis(waited), interval)
        };
        // With nothing selected, a new worker is told to release once...
        assert!(due(&previews, &None, 1000));
        assert_eq!(previews.offer(None), Offer::Taken);
        assert!(!due(&previews, &None, 1000), "one job at a time");
        assert!(next_result(&mut previews).frame.unwrap().is_none());
        // ...and not every interval after that, which would redraw the picker.
        assert!(!due(&previews, &None, 1000));
        // A source is captured every interval.
        let key = Some(window_key("first", 320));
        assert!(!due(&previews, &key, 700));
        assert!(due(&previews, &key, 750));
        assert_eq!(previews.offer(key.clone()), Offer::Taken);
        assert!(next_result(&mut previews).frame.unwrap().is_some());
        assert!(due(&previews, &key, 750));
        // Deselecting it releases the session again.
        assert!(due(&previews, &None, 1000));
    }

    #[test]
    fn stream_previews_take_opaque_frames_and_screenshots_keep_alpha() {
        let mut key = window_key("first", 320);
        key.cursor = true;
        let stream = capture_options(&key);
        assert_eq!((stream.cursor, stream.alpha), (true, AlphaMode::Opaque));
        // Screenshot previews are PNG, whose transparency needs straight alpha.
        key.screenshot = true;
        assert_eq!(capture_options(&key).alpha, AlphaMode::Straight);
        // Thumbnails are JPEG previews too.
        let thumbnail = capture_options(&thumbnail_key("window".into()));
        assert_eq!(THUMBNAIL_OPTIONS, thumbnail);
        assert_eq!(THUMBNAIL_OPTIONS.alpha, AlphaMode::Opaque);
    }

    #[test]
    fn preview_uses_stream_encoding_and_screenshot_uses_capture_pixels() {
        let mut key = PreviewKey {
            target: CaptureTarget::Toplevel("stable-window".into()),
            cursor: true,
            quality: 72,
            max_width: Some(320),
            pixel_mode: PixelMode::Logical,
            screenshot: false,
        };
        let stream = PreviewFrame::encode(omabeam_capture::demo_frame(0), &key).unwrap();
        assert_eq!((stream.width, stream.height), (320, 180));
        key.screenshot = true;
        let shot = PreviewFrame::encode(omabeam_capture::demo_frame(0), &key).unwrap();
        assert_eq!((shot.width, shot.height), (640, 360));

        let hidpi = || {
            let mut frame = omabeam_capture::demo_frame(0);
            frame.logical_width = 320;
            frame.logical_height = 180;
            frame
        };
        key.screenshot = false;
        key.max_width = None;
        let logical = PreviewFrame::encode(hidpi(), &key).unwrap();
        assert_eq!((logical.width, logical.height), (320, 180));
        key.pixel_mode = PixelMode::Native;
        let native = PreviewFrame::encode(hidpi(), &key).unwrap();
        assert_eq!((native.width, native.height), (640, 360));
    }

    #[test]
    fn stale_source_and_setting_results_have_different_keys() {
        let original = PreviewKey {
            target: CaptureTarget::Toplevel("first".into()),
            cursor: false,
            quality: 55,
            max_width: None,
            pixel_mode: PixelMode::Logical,
            screenshot: false,
        };
        let mut next = original.clone();
        next.target = CaptureTarget::Toplevel("second".into());
        assert_ne!(original, next);
        next = original.clone();
        next.cursor = true;
        assert_ne!(original, next);
        next = original.clone();
        next.quality = 90;
        assert_ne!(original, next);
        next = original.clone();
        next.max_width = Some(1280);
        assert_ne!(original, next);
        next = original.clone();
        next.pixel_mode = PixelMode::Native;
        assert_ne!(original, next);
    }
}
