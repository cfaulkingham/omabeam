use crate::{
    CaptureTarget, CapturedFrame, Rect,
    connection::*,
    pixels::{self, AlphaMode, BufferSpec, PixelRect},
};
use anyhow::{Context, Result, ensure};
use image::RgbaImage;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::ops::Range;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

/// Set to 1 to copy, read and decode whole frames every time, for
/// compositors that under-report damage.
const FULL_DAMAGE_ENV: &str = "OMABEAM_CAPTURE_FULL_DAMAGE";

/// What a decoded image was made from. Updating rows in place needs an exact
/// match: a flip, a format change or another alpha mode keeps the size but
/// invalidates every pixel. Skipped damage leaves pixels outside the shown
/// area stale, so a different shown area (an output moved under a desktop
/// rectangle, say) needs a full decode as well.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DecodeKey {
    spec: BufferSpec,
    inverted: bool,
    transform: wl_output::Transform,
    alpha: AlphaMode,
    shown: Option<PixelRect>,
}

/// The output configuration a wlr frame was requested for. wlroots copies the
/// area it computed then and still completes a waiting frame after a
/// rotation, a larger mode or a scale change, so such a frame no longer
/// matches its output.
#[derive(Clone, Copy, PartialEq, Eq)]
struct WlrGeometry {
    transform: wl_output::Transform,
    mode: (u32, u32),
    scale: i32,
    logical: (u32, u32),
    /// The region, clipped to the output, whose buffer is the frame.
    region: Option<Rect>,
}

impl WlrGeometry {
    fn of(output: &Output, target: &CaptureTarget) -> Result<Self> {
        let rect = output.rect();
        let region = match target {
            CaptureTarget::Region(region) => {
                let bounds = Rect { x: 0, y: 0, ..rect };
                let clipped = region.rect.intersect(bounds);
                Some(clipped.context("selected region is outside its output")?)
            }
            _ => None,
        };
        Ok(Self {
            transform: output.transform,
            mode: output.mode,
            scale: output.scale,
            logical: (rect.width, rect.height),
            region,
        })
    }
}

pub(crate) struct Slot {
    pub output: Option<u32>,
    session: Option<session::ExtImageCopyCaptureSessionV1>,
    frame: Option<frame::ExtImageCopyCaptureFrameV1>,
    wlr_frame: Option<wlr_frame::ZwlrScreencopyFrameV1>,
    buffer: Option<Buffer>,
    spec: Option<BufferSpec>,
    constraint_size: (u32, u32),
    constraint_formats: Vec<wl_shm::Format>,
    ready: bool,
    transform: wl_output::Transform,
    inverted: bool,
    last: Option<RgbaImage>,
    retries: u8,
    constraints_epoch: u64,
    requested_epoch: u64,
    await_constraints: bool,
    /// When a rejected buffer started waiting for new constraints.
    await_since: Option<Instant>,
    /// The ext frame's damage events, in buffer pixels as sent.
    damage: Vec<[i32; 4]>,
    /// The frame in flight copies the whole buffer: ext damages all of it,
    /// wlr uses a plain copy that does not wait for damage.
    refresh: bool,
    /// When a whole-buffer copy last arrived: every wlr frame, but an ext
    /// frame only on a refresh.
    refreshed: Option<Instant>,
    /// What `last` was decoded from.
    key: Option<DecodeKey>,
    /// The next frame will likely update `last` in place, so rendering copies
    /// it rather than handing it out. False while the compositor reports
    /// whole-frame damage, which would make every copy wasted.
    keep: bool,
    /// The output configuration the wlr frame was requested for.
    wlr_geometry: Option<WlrGeometry>,
    /// The wlr frame in flight was sent copy_with_damage.
    waiting_for_damage: bool,
    /// A replaced wlr frame was destroyed; its buffer stays untouched until
    /// a sync confirms the compositor processed that.
    retiring: bool,
}
impl Slot {
    fn new(output: Option<u32>) -> Self {
        Self {
            output,
            session: None,
            frame: None,
            wlr_frame: None,
            buffer: None,
            spec: None,
            constraint_size: (0, 0),
            constraint_formats: Vec::new(),
            ready: false,
            transform: wl_output::Transform::Normal,
            inverted: false,
            last: None,
            retries: 0,
            constraints_epoch: 0,
            requested_epoch: 0,
            await_constraints: false,
            await_since: None,
            damage: Vec::new(),
            refresh: false,
            refreshed: None,
            key: None,
            keep: false,
            wlr_geometry: None,
            waiting_for_damage: false,
            retiring: false,
        }
    }
    fn pending(&self) -> bool {
        self.frame.is_some() || self.wlr_frame.is_some() || self.retiring
    }
    /// The decoded frame: a copy while later frames can update it in place.
    fn image(&mut self) -> RgbaImage {
        if self.keep {
            self.last.clone()
        } else {
            self.last.take()
        }
        .unwrap()
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        if let Some(frame) = self.wlr_frame.take() {
            frame.destroy();
        }
        if let Some(session) = self.session.take() {
            session.destroy();
        }
    }
}

/// How a session captures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    /// Paint the cursor into frames.
    pub cursor: bool,
    /// `Straight` keeps transparency, as PNG needs. Encoders that composite
    /// over black anyway (JPEG, H.264) can take `Opaque` frames.
    pub alpha: AlphaMode,
}

/// Persistent connection, sources, sessions and reusable shared-memory buffers.
/// `next_frame` returns None when there is no damage before its timeout, keeping
/// the pending capture alive for the next call.
pub struct CaptureSession {
    runtime: Runtime,
    target: CaptureTarget,
    cursor: bool,
    /// How long a rejected buffer waits for new constraints before retrying
    /// with a fresh one. The retry limit then fails a source that keeps
    /// rejecting, instead of freezing on the last picture.
    pub(crate) constraints_grace: Duration,
    /// How often the whole buffer is copied and read again, in case damage
    /// is under-reported: an ext capture then asks for whole-buffer damage,
    /// and a wlr frame still waiting for damage gives way to a plain copy.
    pub(crate) refresh_interval: Duration,
    /// Copy, read and decode whole frames every time (`FULL_DAMAGE_ENV`).
    pub(crate) full_damage: bool,
    alpha: AlphaMode,
    /// Captures are stills: no frame follows to update an image in place.
    pub(crate) still: bool,
}
impl CaptureSession {
    pub fn new(target: CaptureTarget) -> Result<Self> {
        Self::with_options(target, CaptureOptions::default())
    }
    pub fn new_with_cursor(target: CaptureTarget, cursor: bool) -> Result<Self> {
        let options = CaptureOptions {
            cursor,
            ..CaptureOptions::default()
        };
        Self::with_options(target, options)
    }
    pub fn with_options(target: CaptureTarget, options: CaptureOptions) -> Result<Self> {
        let mut session = Self::with_runtime_options(Runtime::connect(false)?, target, options)?;
        session.full_damage = std::env::var_os(FULL_DAMAGE_ENV).is_some_and(|v| v == "1");
        Ok(session)
    }
    #[cfg(test)]
    pub(crate) fn with_runtime(runtime: Runtime, target: CaptureTarget) -> Result<Self> {
        Self::with_runtime_options(runtime, target, CaptureOptions::default())
    }
    pub(crate) fn with_runtime_options(
        mut runtime: Runtime,
        target: CaptureTarget,
        options: CaptureOptions,
    ) -> Result<Self> {
        Self::add_slots(&mut runtime, &target, options.cursor)?;
        Ok(Self {
            runtime,
            target,
            cursor: options.cursor,
            constraints_grace: Duration::from_secs(2),
            refresh_interval: Duration::from_secs(1),
            full_damage: false,
            alpha: options.alpha,
            still: false,
        })
    }

    /// Capture `target` next over the same connection. The old target's
    /// sessions, frames and buffers go, and new ones are made: a new
    /// session's first frame always arrives, where a kept one could wait for
    /// damage. After an error the session must not be used again.
    pub(crate) fn retarget(&mut self, target: CaptureTarget) -> Result<()> {
        // Every event read so far has been dispatched (each call returns only
        // then), and the connection drops later events for destroyed
        // objects: nothing of the old target, such as a window that closed
        // after its capture, can reach the new slots.
        self.release();
        // A window mapped since the last capture has been announced by now.
        self.runtime.sync()?;
        self.target = target;
        Self::add_slots(&mut self.runtime, &self.target, self.cursor)
    }

    /// Drops the target's sessions, frames, buffers and images, keeping the
    /// connection with its outputs and window list.
    fn release(&mut self) {
        self.runtime.state.slots.clear();
        // Send the destroys now rather than with the next capture; a failure
        // shows then.
        let _ = self.runtime.conn.flush();
    }

    /// Whether the compositor has closed the connection. Nothing is read, so
    /// events it sent meanwhile wait for the next dispatch.
    fn closed(&self) -> bool {
        let fd = self.runtime.conn.as_fd();
        // macOS reports a hang-up only to a poll for input.
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let hung_up = PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL;
        poll(&mut fds, Some(&Timespec::default())).is_ok_and(|ready| ready > 0)
            && fds[0].revents().intersects(hung_up)
    }

    /// Validates `target` and adds its slots, with an ext session each where
    /// the compositor has them. The sync reports a failure at once.
    fn add_slots(runtime: &mut Runtime, target: &CaptureTarget, cursor: bool) -> Result<()> {
        let options = if cursor {
            copy::Options::PaintCursors
        } else {
            copy::Options::empty()
        };
        match target {
            CaptureTarget::Region(region) => ensure!(
                region.rect.width > 0 && region.rect.height > 0,
                "empty capture region"
            ),
            CaptureTarget::Toplevel(id) => ensure!(
                !id.is_empty(),
                "window capture requires a stable identifier"
            ),
            _ => {}
        }
        let qh = runtime.queue.handle();
        let state = &mut runtime.state;
        let output_ids: Vec<u32> = match target {
            CaptureTarget::Toplevel(identifier) => {
                let copy = state
                    .copy
                    .as_ref()
                    .context("window capture requires ext-image-copy-capture-v1")?;
                let manager = state
                    .top_source
                    .as_ref()
                    .context("compositor does not support individual window capture")?;
                let (handle, _) = state
                    .toplevels
                    .values()
                    .find(|(_, id)| id == identifier)
                    .context("selected window is no longer available for capture")?;
                let source = manager.create_source(handle, &qh, ());
                let mut slot = Slot::new(None);
                slot.session = Some(copy.create_session(&source, options, &qh, 0usize));
                source.destroy();
                state.slots.push(slot);
                Vec::new()
            }
            CaptureTarget::Output(name)
            | CaptureTarget::Region(crate::Region { output: name, .. }) => {
                vec![
                    *state
                        .outputs
                        .iter()
                        .find(|(_, o)| o.name == *name)
                        .with_context(|| format!("selected output {name} is unavailable"))?
                        .0,
                ]
            }
            CaptureTarget::DesktopRect(rect) => {
                ensure!(rect.width > 0 && rect.height > 0, "empty capture region");
                let mut ids: Vec<_> = state
                    .outputs
                    .iter()
                    .filter(|(_, output)| output.rect().intersect(*rect).is_some())
                    .map(|(id, _)| *id)
                    .collect();
                ids.sort_unstable();
                ensure!(
                    !ids.is_empty(),
                    "selected window rectangle is outside all outputs"
                );
                ids
            }
        };
        for id in output_ids {
            let output = &state.outputs[&id];
            ensure!(
                output.rect().width > 0 && output.rect().height > 0,
                "output geometry is unavailable"
            );
            let mut slot = Slot::new(Some(id));
            if let (Some(copy), Some(manager)) = (&state.copy, &state.output_source) {
                let source = manager.create_source(&output.proxy, &qh, ());
                slot.session = Some(copy.create_session(&source, options, &qh, state.slots.len()));
                source.destroy();
            } else {
                state.wlr_copy.as_ref().context(
                    "screen capture requires ext-image-copy-capture-v1 or wlr-screencopy",
                )?;
            }
            state.slots.push(slot);
        }
        runtime.sync()
    }

    #[cfg(test)]
    pub(crate) fn bytes_read(&self) -> Vec<usize> {
        let slots = self.runtime.state.slots.iter();
        slots
            .map(|s| s.buffer.as_ref().map_or(0, |b| b.bytes_read))
            .collect()
    }

    /// Whether each slot kept its decoded image for in-place updates.
    #[cfg(test)]
    pub(crate) fn kept_images(&self) -> Vec<bool> {
        let slots = self.runtime.state.slots.iter();
        slots.map(|s| s.last.is_some()).collect()
    }

    /// Obtain a still image, failing promptly if the source cannot produce it.
    pub fn capture(&mut self) -> Result<CapturedFrame> {
        self.next_frame(SETUP_TIMEOUT)?
            .context("screen capture timed out waiting for the first frame")
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        let deadline = Instant::now() + timeout;
        loop {
            self.runtime
                .queue
                .dispatch_pending(&mut self.runtime.state)?;
            self.runtime.check_error()?;
            let changed = self.consume_ready()?;
            if changed && self.runtime.state.slots.iter().all(|s| s.last.is_some()) {
                return self.render().map(Some);
            }
            self.request_frames()?;
            // Flush even when the timeout is zero. No new connection or frame
            // is created when an existing request is still waiting for damage.
            if Instant::now() >= deadline {
                self.runtime.conn.flush()?;
                return Ok(None);
            }
            // A slot waiting out the constraints grace, or a wlr frame due
            // for its refresh, needs no event to go on.
            let (grace, interval) = (self.constraints_grace, self.refresh_interval);
            let slots = self.runtime.state.slots.iter();
            let wake = slots
                .filter_map(|s| {
                    if s.await_constraints {
                        s.await_since?.checked_add(grace)
                    } else if s.waiting_for_damage {
                        s.refreshed?.checked_add(interval)
                    } else {
                        None
                    }
                })
                .fold(deadline, Instant::min);
            self.runtime
                .pump(wake.saturating_duration_since(Instant::now()))?;
        }
    }

    fn request_frames(&mut self) -> Result<()> {
        let now = Instant::now();
        let interval = self.refresh_interval;
        let due = |refreshed: Option<Instant>| {
            refreshed.is_none_or(|t| now.duration_since(t) >= interval)
        };
        let state = &mut self.runtime.state;
        let qh = self.runtime.queue.handle();
        let display = self.runtime.conn.display();
        for (i, slot) in state.slots.iter_mut().enumerate() {
            if slot.waiting_for_damage && due(slot.refreshed) {
                // Refresh a static or under-reported screen with a plain
                // copy. copy_with_damage cannot be re-requested, so the
                // frame is destroyed. The compositor handles requests in
                // order, so the sync's done event means it has processed the
                // destroy: that frame can no longer write the buffer or send
                // events, and any it sent arrive first (and are dropped for a
                // destroyed proxy). Only then is the buffer used again.
                if let Some(frame) = slot.wlr_frame.take() {
                    frame.destroy();
                }
                slot.waiting_for_damage = false;
                slot.retiring = true;
                display.sync(&qh, Retired(i));
            }
            if slot.await_constraints
                && slot
                    .await_since
                    .is_some_and(|t| now.duration_since(t) >= self.constraints_grace)
            {
                // No new constraints followed the rejection: retry with a
                // fresh buffer. The retry limit fails a source that keeps
                // rejecting.
                slot.await_constraints = false;
            }
            if slot.pending() || slot.await_constraints {
                continue;
            }
            if let Some(session) = &slot.session {
                let Some(spec) = slot.spec else {
                    continue;
                };
                if slot.buffer.as_ref().is_none_or(|b| b.spec != spec) {
                    slot.buffer = Some(Buffer::new(state.shm.as_ref().unwrap(), spec, &qh)?);
                }
                let buffer = slot.buffer.as_ref().unwrap();
                slot.requested_epoch = slot.constraints_epoch;
                // The client never writes to its buffer, so the protocol asks
                // for whole-buffer damage only on first use. Asking again now
                // and then makes a compositor that under-reports damage copy
                // everything for the safety refresh.
                slot.refresh = self.full_damage || !buffer.captured || due(slot.refreshed);
                slot.damage.clear();
                let frame = session.create_frame(&qh, i);
                frame.attach_buffer(&buffer.proxy);
                if slot.refresh {
                    frame.damage_buffer(0, 0, spec.width as i32, spec.height as i32);
                }
                frame.capture();
                slot.frame = Some(frame);
            } else {
                let output = state
                    .outputs
                    .get(&slot.output.unwrap())
                    .context("selected output disconnected")?;
                let geometry = WlrGeometry::of(output, &self.target)?;
                slot.wlr_geometry = Some(geometry);
                slot.transform = geometry.transform;
                slot.inverted = false;
                slot.spec = None;
                // A retry after a failure, and the refresh, must not wait for
                // damage.
                slot.refresh = self.full_damage || slot.retries > 0 || due(slot.refreshed);
                let manager = state.wlr_copy.as_ref().unwrap();
                let cursor = i32::from(self.cursor);
                slot.wlr_frame = Some(match geometry.region {
                    // Read back only the region; its buffer is the frame.
                    Some(rect) => manager.capture_output_region(
                        cursor,
                        &output.proxy,
                        rect.x,
                        rect.y,
                        rect.width as i32,
                        rect.height as i32,
                        &qh,
                        i,
                    ),
                    None => manager.capture_output(cursor, &output.proxy, &qh, i),
                });
            }
        }
        Ok(())
    }

    /// Reads and decodes ready buffers. On untransformed ext frames only the
    /// damaged rows the target shows are read and converted, and damage the
    /// target does not show is skipped without publishing.
    fn consume_ready(&mut self) -> Result<bool> {
        let now = Instant::now();
        let state = &mut self.runtime.state;
        let mut changed = false;
        for slot in &mut state.slots {
            if !slot.ready {
                continue;
            }
            slot.ready = false;
            slot.retries = 0;
            let buffer = slot
                .buffer
                .as_mut()
                .context("capture completed without a buffer")?;
            let spec = buffer.spec;
            let output = slot.output.and_then(|id| state.outputs.get(&id));
            if slot.session.is_none() {
                let now_geometry = output.and_then(|o| WlrGeometry::of(o, &self.target).ok());
                if now_geometry != slot.wlr_geometry {
                    // Copied for the configuration it was requested with:
                    // drop it, and make the next request a plain copy.
                    // Not counted as a retry: a multi-step reconfiguration must not end the share.
                    slot.refreshed = None;
                    slot.last = None;
                    continue;
                }
            }
            let (inverted, transform, alpha) = (slot.inverted, slot.transform, self.alpha);
            let whole = PixelRect {
                x: 0,
                y: 0,
                width: spec.width,
                height: spec.height,
            };
            // What the target shows, where frames can be updated in place.
            let shown = if slot.session.is_some() && transform == wl_output::Transform::Normal {
                shown_area(&self.target, output, whole)?
            } else {
                None
            };
            let key = DecodeKey {
                spec,
                inverted,
                transform,
                alpha,
                shown,
            };
            // Every wlr frame copies the whole buffer; ext only on a refresh.
            if slot.refresh || slot.session.is_none() {
                slot.refreshed = Some(now);
            }
            // Damage limits the work only on untransformed ext frames; None
            // reads and decodes everything. wlr damage is just a "something
            // changed" signal, and a frame without damage events may have
            // changed anywhere.
            let rows = if slot.session.is_some()
                && !self.full_damage
                && transform == wl_output::Transform::Normal
                && !slot.damage.is_empty()
            {
                // Keep the image for in-place updates unless an ordinary
                // frame's damage covers every row, which suggests the
                // compositor always reports whole frames. A refresh, such as
                // a buffer's first capture, says nothing about that. Merged
                // bands cover every row only as the single band 0..height. A
                // still has no next frame.
                let all_rows = 0..spec.height;
                slot.keep = !self.still
                    && (slot.refresh
                        || damage_rows(&slot.damage, whole).first() != Some(&all_rows));
                Some(shown.map_or_else(Vec::new, |area| damage_rows(&slot.damage, area)))
            } else {
                slot.keep = false;
                None
            };
            let reusable = slot.last.is_some() && slot.key == Some(key) && !slot.refresh;
            match rows {
                Some(rows) if reusable => {
                    if rows.is_empty() {
                        // Nothing the target shows changed: keep waiting.
                        continue;
                    }
                    let image = slot.last.as_mut().unwrap();
                    let bytes = buffer.read_rows(&rows)?;
                    for band in rows {
                        pixels::decode_rows_into(
                            bytes, spec, inverted, transform, alpha, band, image,
                        )?;
                    }
                }
                _ => {
                    let mut image = slot.last.take().unwrap_or_else(|| RgbaImage::new(0, 0));
                    let bytes = buffer.read()?;
                    pixels::decode_into(bytes, spec, inverted, transform, alpha, &mut image)?;
                    slot.last = Some(image);
                    slot.key = Some(key);
                }
            }
            changed = true;
        }
        Ok(changed)
    }

    fn render(&mut self) -> Result<CapturedFrame> {
        let state = &mut self.runtime.state;
        let slot = &mut state.slots[0];
        match &self.target {
            CaptureTarget::Toplevel(_) => {
                let image = slot.image();
                Ok(CapturedFrame {
                    logical_width: image.width(),
                    logical_height: image.height(),
                    image,
                })
            }
            CaptureTarget::Output(_) => {
                let rect = state.outputs[&slot.output.unwrap()].rect();
                Ok(CapturedFrame {
                    image: slot.image(),
                    logical_width: rect.width,
                    logical_height: rect.height,
                })
            }
            CaptureTarget::Region(region) => match slot.wlr_geometry.and_then(|g| g.region) {
                // wlr captured just the region, clipped to the output.
                Some(rect) => Ok(CapturedFrame {
                    image: slot.image(),
                    logical_width: rect.width,
                    logical_height: rect.height,
                }),
                None => pixels::crop(
                    slot.last.as_ref().unwrap(),
                    state.outputs[&slot.output.unwrap()].rect(),
                    region.rect,
                ),
            },
            CaptureTarget::DesktopRect(region) => {
                let tiles: Vec<_> = state
                    .slots
                    .iter()
                    .map(|s| {
                        (
                            s.last.as_ref().unwrap(),
                            state.outputs[&s.output.unwrap()].rect(),
                        )
                    })
                    .collect();
                pixels::compose(&tiles, *region)
            }
        }
    }
}

/// Still images of one target after another, such as window thumbnails, over
/// one Wayland connection. Only the connection, with its outputs and window
/// list, outlasts a capture: each one makes sessions and buffers for its
/// target and releases them with the image. The first capture connects, and
/// so does the next one after an error or once the compositor has closed the
/// connection.
pub struct StillCapturer {
    open: Box<dyn FnMut(CaptureTarget) -> Result<CaptureSession> + Send>,
    session: Option<CaptureSession>,
}

impl StillCapturer {
    pub fn new(options: CaptureOptions) -> Self {
        Self::with_open(move |target| CaptureSession::with_options(target, options))
    }

    /// Opens sessions with `open`; tests connect to their compositor.
    pub(crate) fn with_open(
        open: impl FnMut(CaptureTarget) -> Result<CaptureSession> + Send + 'static,
    ) -> Self {
        Self {
            open: Box::new(open),
            session: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn kept_slots(&self) -> usize {
        self.session
            .as_ref()
            .map_or(0, |s| s.runtime.state.slots.len())
    }

    pub fn capture(&mut self, target: CaptureTarget) -> Result<CapturedFrame> {
        // A connection the compositor closed while idle is replaced rather
        // than failing this capture. Anything else, such as a window that is
        // gone, fails it.
        if self.session.as_ref().is_some_and(CaptureSession::closed) {
            self.session = None;
        }
        // Taken until the capture succeeds, so an error drops the connection.
        let mut session = match self.session.take() {
            Some(mut session) => {
                session.retarget(target)?;
                session
            }
            None => (self.open)(target)?,
        };
        // No frame follows, so the image is handed out rather than copied,
        // and nothing of the target is kept for the next capture.
        session.still = true;
        let frame = session.capture();
        session.release();
        self.session = frame.is_ok().then_some(session);
        frame
    }
}

impl Dispatch<session::ExtImageCopyCaptureSessionV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &session::ExtImageCopyCaptureSessionV1,
        event: session::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let slot = &mut state.slots[*index];
        match event {
            session::Event::BufferSize { width, height } => slot.constraint_size = (width, height),
            session::Event::ShmFormat {
                format: WEnum::Value(format),
            } if pixels::bytes_per_pixel(format).is_some() => {
                slot.constraint_formats.push(format);
            }
            session::Event::Done => {
                slot.constraints_epoch += 1;
                slot.await_constraints = false;
                // Each complete constraint batch replaces the previous one.
                let formats = std::mem::take(&mut slot.constraint_formats);
                let format = [
                    wl_shm::Format::Xrgb8888,
                    wl_shm::Format::Argb8888,
                    wl_shm::Format::Xbgr8888,
                    wl_shm::Format::Abgr8888,
                ]
                .into_iter()
                .find(|f| formats.contains(f))
                .or_else(|| formats.first().copied());
                let result = format
                    .context("capture source provides no supported shared-memory pixel format")
                    .and_then(|f| {
                        BufferSpec::packed(slot.constraint_size.0, slot.constraint_size.1, f)
                    });
                match result {
                    Ok(spec) => slot.spec = Some(spec),
                    Err(e) => state.error = Some(e),
                }
            }
            session::Event::Stopped => {
                state.error = Some(anyhow::anyhow!(
                    "selected capture source is no longer available"
                ))
            }
            _ => {}
        }
    }
}

impl Dispatch<frame::ExtImageCopyCaptureFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        proxy: &frame::ExtImageCopyCaptureFrameV1,
        event: frame::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let slot = &mut state.slots[*index];
        match event {
            frame::Event::Transform {
                transform: WEnum::Value(transform),
            } => slot.transform = transform,
            frame::Event::Transform { .. } => {
                state.error = Some(anyhow::anyhow!("unknown capture transform"))
            }
            frame::Event::Damage {
                x,
                y,
                width,
                height,
            } => slot.damage.push([x, y, width, height]),
            frame::Event::Ready => {
                slot.ready = true;
                slot.frame = None;
                if let Some(buffer) = &mut slot.buffer {
                    buffer.captured = true;
                }
                proxy.destroy();
            }
            frame::Event::Failed { reason } => {
                slot.frame = None;
                proxy.destroy();
                slot.retries += 1;
                if matches!(
                    reason,
                    WEnum::Value(frame::FailureReason::BufferConstraints)
                ) && slot.retries <= 3
                {
                    slot.buffer = None;
                    // If the new constraints have not arrived yet, do not
                    // repeatedly submit the stale buffer while waiting.
                    slot.await_constraints = slot.requested_epoch == slot.constraints_epoch;
                    slot.await_since = Some(Instant::now());
                } else {
                    state.error = Some(anyhow::anyhow!("screen capture failed: {reason:?}"));
                }
            }
            _ => {}
        }
    }
}

fn copy_wlr_buffer(
    state: &mut State,
    proxy: &wlr_frame::ZwlrScreencopyFrameV1,
    index: usize,
    qh: &QueueHandle<State>,
) -> Result<()> {
    let slot = &mut state.slots[index];
    let spec = slot
        .spec
        .context("capture source provides no supported shared-memory pixel format")?;
    if slot.buffer.as_ref().is_none_or(|b| b.spec != spec) {
        slot.buffer = Some(Buffer::new(state.shm.as_ref().unwrap(), spec, qh)?);
    }
    let buffer = slot.buffer.as_ref().unwrap();
    slot.waiting_for_damage = buffer.captured && !slot.refresh && proxy.version() >= 2;
    if slot.waiting_for_damage {
        // The compositor holds the copy until the output changes.
        proxy.copy_with_damage(&buffer.proxy);
    } else {
        // A plain copy completes on a static screen too, so a buffer's first
        // capture never waits for damage.
        proxy.copy(&buffer.proxy);
    }
    Ok(())
}
impl Dispatch<wlr_frame::ZwlrScreencopyFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        proxy: &wlr_frame::ZwlrScreencopyFrameV1,
        event: wlr_frame::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // A replaced frame's events may still be queued; they must not
        // complete or fail its successor.
        if state.slots[*index].wlr_frame.as_ref() != Some(proxy) {
            return;
        }
        match event {
            wlr_frame::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                let spec = BufferSpec {
                    width,
                    height,
                    stride,
                    format,
                };
                match spec.byte_len() {
                    Ok(_) => state.slots[*index].spec = Some(spec),
                    Err(e) => {
                        state.error = Some(e);
                        return;
                    }
                }
                if proxy.version() < 3
                    && let Err(e) = copy_wlr_buffer(state, proxy, *index, qh)
                {
                    state.error = Some(e);
                }
            }
            wlr_frame::Event::BufferDone => {
                if let Err(e) = copy_wlr_buffer(state, proxy, *index, qh) {
                    state.error = Some(e);
                }
            }
            wlr_frame::Event::Flags {
                flags: WEnum::Value(flags),
            } => state.slots[*index].inverted = flags.contains(wlr_frame::Flags::YInvert),
            wlr_frame::Event::Ready { .. } => {
                let slot = &mut state.slots[*index];
                slot.ready = true;
                slot.wlr_frame = None;
                slot.waiting_for_damage = false;
                if let Some(buffer) = &mut slot.buffer {
                    buffer.captured = true;
                }
                proxy.destroy();
            }
            wlr_frame::Event::Failed => {
                let slot = &mut state.slots[*index];
                slot.wlr_frame = None;
                slot.waiting_for_damage = false;
                proxy.destroy();
                // A frame waiting for damage can outlive an output change,
                // such as a smaller mode, which fails it. Retry with a plain
                // copy; only repeated failure stops the capture.
                slot.retries += 1;
                if slot.retries > 3 {
                    state.error = Some(anyhow::anyhow!(
                        "screen capture failed: compositor rejected the frame"
                    ));
                }
            }
            // Damage only says something changed, which ready implies. How its
            // boxes relate to regions and y_invert is unspecified, so the
            // whole (region) buffer is decoded.
            _ => {}
        }
    }
}

/// The wl_display.sync after a replaced wlr frame's destroy, for a slot.
struct Retired(usize);

impl Dispatch<wl_callback::WlCallback, Retired> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        slot: &Retired,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The compositor has processed the destroy: the buffer is idle. A
        // retarget clears the slots and then syncs, so the reply can come
        // while there are none.
        if let Some(slot) = state.slots.get_mut(slot.0) {
            slot.retiring = false;
        }
    }
}

/// Pixels of an untransformed buffer, `whole`, that the target shows from
/// this output, as `crop` and `compose` pick them; None when the output
/// shows none of a desktop rectangle.
fn shown_area(
    target: &CaptureTarget,
    output: Option<&Output>,
    whole: PixelRect,
) -> Result<Option<PixelRect>> {
    let output = || output.context("selected output disconnected");
    let local = match target {
        CaptureTarget::Output(_) | CaptureTarget::Toplevel(_) => return Ok(Some(whole)),
        CaptureTarget::Region(region) => region.rect,
        CaptureTarget::DesktopRect(rect) => {
            let logical = output()?.rect();
            let Some(overlap) = rect.intersect(logical) else {
                return Ok(None);
            };
            Rect {
                x: overlap.x - logical.x,
                y: overlap.y - logical.y,
                ..overlap
            }
        }
    };
    let (area, _) = pixels::pixel_rect((whole.width, whole.height), output()?.rect(), local)?;
    Ok(Some(area))
}

/// Buffer rows of the damage boxes that touch `area`, clipped to it, sorted
/// and merged. Damage arrives as signed ints and may over-report, so boxes
/// are clipped rather than trusted.
fn damage_rows(damage: &[[i32; 4]], area: PixelRect) -> Vec<Range<u32>> {
    let (left, top) = (i64::from(area.x), i64::from(area.y));
    let right = left + i64::from(area.width);
    let bottom = top + i64::from(area.height);
    let mut rows: Vec<Range<u32>> = damage
        .iter()
        .filter_map(|&[x, y, width, height]| {
            let (x, y) = (i64::from(x), i64::from(y));
            let columns = x.max(left) < (x + i64::from(width)).min(right);
            let (start, end) = (y.max(top), (y + i64::from(height)).min(bottom));
            // Both lie within `area`, so they fit in u32.
            (columns && start < end).then_some(start as u32..end as u32)
        })
        .collect();
    rows.sort_unstable_by_key(|rows| rows.start);
    let mut merged: Vec<Range<u32>> = Vec::with_capacity(rows.len());
    for band in rows {
        match merged.last_mut() {
            Some(last) if band.start <= last.end => last.end = last.end.max(band.end),
            _ => merged.push(band),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_rows_clip_to_the_area_and_merge() {
        let buffer = PixelRect {
            x: 0,
            y: 0,
            width: 100,
            height: 80,
        };
        let bands = |damage: &[[i32; 4]], area| {
            let rows = damage_rows(damage, area).into_iter();
            rows.map(|r| (r.start, r.end)).collect::<Vec<_>>()
        };
        let rows = |damage: &[[i32; 4]]| bands(damage, buffer);
        assert_eq!(rows(&[[0, 2, 5, 3]]), [(2, 5)]);
        // Overlapping and adjacent boxes merge; the order does not matter.
        assert_eq!(rows(&[[3, 5, 2, 2], [0, 2, 5, 3], [9, 6, 1, 4]]), [(2, 10)]);
        assert_eq!(rows(&[[0, 10, 1, 1], [0, 0, 1, 1]]), [(0, 1), (10, 11)]);
        assert_eq!(rows(&[[0, 0, 1, 10], [0, 2, 1, 3]]), [(0, 10)]);
        // Over-reported boxes are clipped instead of failing the frame.
        assert_eq!(rows(&[[-10, 70, 30, 50]]), [(70, 80)]);
        assert_eq!(rows(&[[-5, -5, i32::MAX, i32::MAX]]), [(0, 80)]);
        // Boxes outside the buffer or without area change nothing.
        assert!(rows(&[[100, 0, 5, 5], [0, 80, 5, 5], [0, 0, 0, 5], [0, 0, 5, -1]]).is_empty());
        assert!(rows(&[[i32::MAX, i32::MAX, i32::MAX, i32::MAX]]).is_empty());
        assert!(rows(&[[i32::MIN, i32::MIN, 10, 10]]).is_empty());
        // A region: only boxes that reach its columns count, and only its rows.
        let region = PixelRect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        assert!(bands(&[[0, 0, 10, 100], [40, 0, 5, 100]], region).is_empty());
        assert_eq!(bands(&[[15, 0, 5, 100]], region), [(20, 60)]);
        assert_eq!(bands(&[[39, 59, 5, 5]], region), [(59, 60)]);
    }
}
