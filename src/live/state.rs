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
}

pub(super) struct FrameData {
    pub jpeg: Arc<[u8]>,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    times: VecDeque<Instant>,
    started: Instant,
    pub ended: Option<Instant>,
    pub error: Option<String>,
}

pub(super) struct FrameState {
    pub inner: Mutex<FrameData>,
    pub tick: Condvar,
    pub viewers: AtomicUsize,
    source: String,
}

impl FrameState {
    pub fn new(source: String) -> Self {
        Self {
            inner: Mutex::new(FrameData {
                jpeg: Arc::from([]),
                generation: 0,
                width: 0,
                height: 0,
                times: VecDeque::new(),
                started: Instant::now(),
                ended: None,
                error: None,
            }),
            tick: Condvar::new(),
            viewers: AtomicUsize::new(0),
            source,
        }
    }
    pub fn publish(&self, jpeg: Vec<u8>, width: u32, height: u32) {
        let mut data = self.inner.lock().unwrap();
        if data.ended.is_some() {
            return;
        }
        data.jpeg = jpeg.into();
        data.generation += 1;
        data.width = width;
        data.height = height;
        let now = Instant::now();
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
        data.ended = Some(Instant::now());
        data.error = Some(error);
        drop(data);
        self.tick.notify_all();
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
            viewers: self.viewers.load(Ordering::SeqCst),
            source: self.source.clone(),
            state: if data.ended.is_some() {
                "ended"
            } else {
                "live"
            }
            .into(),
            error: data.error.clone(),
        }
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

pub(super) struct Viewer<'a>(pub &'a FrameState);
impl<'a> Viewer<'a> {
    pub fn new(frames: &'a FrameState) -> Self {
        frames.viewers.fetch_add(1, Ordering::SeqCst);
        frames.tick.notify_all();
        Self(frames)
    }
}
impl Drop for Viewer<'_> {
    fn drop(&mut self) {
        self.0.viewers.fetch_sub(1, Ordering::SeqCst);
        self.0.tick.notify_all();
    }
}
