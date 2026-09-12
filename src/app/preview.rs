//! A single bounded worker owns preview capture sessions. No preview is written
//! to disk or served over HTTP, and a result is only displayed for its exact key.
use gpui_kit::{Image, ImageFormat};
use omabeam_capture::{CaptureSession, CaptureTarget, CapturedFrame, PixelMode};
use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

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

pub(super) struct PreviewJob {
    pub key: Option<PreviewKey>,
    pub thumbnail: Option<String>,
}

pub(super) struct PreviewResult {
    pub key: Option<PreviewKey>,
    pub frame: Result<Option<PreviewFrame>, String>,
    pub thumbnail: Option<(String, Result<PreviewFrame, String>)>,
}

pub(super) struct PreviewWorker {
    pub jobs: SyncSender<PreviewJob>,
    pub results: Receiver<PreviewResult>,
}

impl PreviewWorker {
    pub fn new(demo: bool) -> anyhow::Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel::<PreviewJob>(1);
        let (sender, results) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("omabeam-preview".into())
            .spawn(move || {
                let mut current: Option<(PreviewKey, CaptureSession)> = None;
                while let Ok(job) = receiver.recv() {
                    let frame = (|| -> anyhow::Result<Option<PreviewFrame>> {
                        let Some(key) = &job.key else {
                            current = None;
                            return Ok(None);
                        };
                        if demo {
                            return PreviewFrame::encode(omabeam_capture::demo_frame(0), key)
                                .map(Some);
                        }
                        if current.as_ref().is_none_or(|(old, _)| old != key) {
                            current = None;
                            let mut session =
                                CaptureSession::new_with_cursor(key.target.clone(), key.cursor)?;
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
                    let thumbnail = job.thumbnail.map(|id| {
                        let result = (|| -> anyhow::Result<PreviewFrame> {
                            let key = PreviewKey {
                                target: CaptureTarget::Toplevel(id.clone()),
                                cursor: false,
                                quality: 55,
                                max_width: Some(256),
                                pixel_mode: PixelMode::Logical,
                                screenshot: false,
                            };
                            let frame = if demo {
                                omabeam_capture::demo_frame(id.bytes().map(u32::from).sum())
                            } else {
                                CaptureSession::new(key.target.clone())?.capture()?
                            };
                            PreviewFrame::encode(frame, &key)
                        })()
                        .map_err(|err| err.to_string());
                        (id, result)
                    });
                    if sender
                        .send(PreviewResult {
                            key: job.key,
                            frame,
                            thumbnail,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self { jobs, results })
    }
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
        let results = self
            .preview_worker
            .as_ref()
            .map(|w| w.results.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for result in results {
            self.preview_pending = false;
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
                        if let Some(old) = self.preview_frame.take() {
                            old.image.remove_asset(cx);
                        }
                        self.preview_error = Some(format!("Preview unavailable. {error}"));
                    }
                }
                changed = true;
            }
            if let Some((id, thumbnail)) = result.thumbnail {
                if let Some(old) = self.thumbnails.remove(&id) {
                    old.remove_asset(cx);
                }
                if self
                    .snapshot()
                    .is_some_and(|s| s.visible_clients().any(|c| c.stable_id == id))
                {
                    if let Ok(frame) = thumbnail {
                        if self.thumbnails.len() >= 48 {
                            let oldest = self
                                .thumbnails
                                .keys()
                                .min_by_key(|id| self.thumbnail_attempts.get(*id))
                                .cloned();
                            if let Some(oldest) = oldest {
                                if let Some(image) = self.thumbnails.remove(&oldest) {
                                    image.remove_asset(cx);
                                }
                            }
                        }
                        self.thumbnails.insert(id, frame.image);
                    }
                }
                changed = true;
            }
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
        if !self.preview_pending && self.preview_updated.elapsed() >= interval {
            let clients = match self.page {
                super::Page::Tiles => self.current_tiles(),
                super::Page::Windows => self.current_windows(),
                _ => Vec::new(),
            };
            let candidates: Vec<_> = clients
                .into_iter()
                .filter(|c| !c.stable_id.is_empty())
                .take(48)
                .map(|c| c.stable_id.clone())
                .collect();
            let thumbnail = if candidates.is_empty() {
                None
            } else {
                let id = &candidates[self.thumbnail_index % candidates.len()];
                self.thumbnail_index = self.thumbnail_index.wrapping_add(1);
                self.thumbnail_attempts
                    .get(id)
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(10))
                    .then(|| id.clone())
            };
            if let Some(worker) = &self.preview_worker {
                let job = PreviewJob {
                    key,
                    thumbnail: thumbnail.clone(),
                };
                match worker.jobs.try_send(job) {
                    Ok(()) => {
                        self.preview_pending = true;
                        self.preview_updated = std::time::Instant::now();
                        if let Some(id) = thumbnail {
                            self.thumbnail_attempts.insert(id, self.preview_updated);
                        }
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        self.preview_error =
                            Some("Preview worker stopped. Reopen OmaBeam to retry.".into());
                        self.preview_worker = None;
                        changed = true;
                    }
                    Err(mpsc::TrySendError::Full(_)) => {}
                }
            }
        }
        if changed {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_tags_frames_with_the_requested_source_and_settings() {
        let worker = PreviewWorker::new(true).unwrap();
        let mut key = PreviewKey {
            target: CaptureTarget::Toplevel("first".into()),
            cursor: false,
            quality: 55,
            max_width: Some(320),
            pixel_mode: PixelMode::Logical,
            screenshot: false,
        };
        worker
            .jobs
            .send(PreviewJob {
                key: Some(key.clone()),
                thumbnail: Some("second".into()),
            })
            .unwrap();
        let result = worker.results.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(result.key.as_ref(), Some(&key));
        let frame = result.frame.unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (320, 180));
        let (id, thumbnail) = result.thumbnail.unwrap();
        assert_eq!(id, "second");
        assert_eq!(thumbnail.unwrap().width, 256);

        key.target = CaptureTarget::Toplevel("second".into());
        key.max_width = Some(160);
        worker
            .jobs
            .send(PreviewJob {
                key: Some(key.clone()),
                thumbnail: None,
            })
            .unwrap();
        let result = worker.results.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(result.key, Some(key));
        assert_eq!(result.frame.unwrap().unwrap().width, 160);
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
