use crate::{
    CaptureTarget, CapturedFrame,
    connection::*,
    pixels::{self, BufferSpec},
};
use anyhow::{Context, Result, ensure};
use image::RgbaImage;
use std::time::{Duration, Instant};

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
        }
    }
    fn pending(&self) -> bool {
        self.frame.is_some() || self.wlr_frame.is_some()
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

/// Persistent connection, sources, sessions and reusable shared-memory buffers.
/// `next_frame` returns None when there is no damage before its timeout, keeping
/// the pending capture alive for the next call.
pub struct CaptureSession {
    runtime: Runtime,
    target: CaptureTarget,
    cursor: bool,
}
impl CaptureSession {
    pub fn new(target: CaptureTarget) -> Result<Self> {
        Self::new_with_cursor(target, false)
    }
    pub fn new_with_cursor(target: CaptureTarget, cursor: bool) -> Result<Self> {
        Self::with_runtime_options(Runtime::connect(false)?, target, cursor)
    }
    #[cfg(test)]
    pub(crate) fn with_runtime(runtime: Runtime, target: CaptureTarget) -> Result<Self> {
        Self::with_runtime_options(runtime, target, false)
    }
    pub(crate) fn with_runtime_options(
        mut runtime: Runtime,
        target: CaptureTarget,
        cursor: bool,
    ) -> Result<Self> {
        let options = if cursor {
            copy::Options::PaintCursors
        } else {
            copy::Options::empty()
        };
        match &target {
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
        let output_ids: Vec<u32> = match &target {
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
        runtime.sync()?;
        Ok(Self {
            runtime,
            target,
            cursor,
        })
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
            self.runtime
                .pump(deadline.saturating_duration_since(Instant::now()))?;
        }
    }

    fn request_frames(&mut self) -> Result<()> {
        let state = &mut self.runtime.state;
        let qh = self.runtime.queue.handle();
        for (i, slot) in state.slots.iter_mut().enumerate() {
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
                slot.requested_epoch = slot.constraints_epoch;
                let frame = session.create_frame(&qh, i);
                frame.attach_buffer(&slot.buffer.as_ref().unwrap().proxy);
                frame.damage_buffer(0, 0, spec.width as i32, spec.height as i32);
                frame.capture();
                slot.frame = Some(frame);
            } else {
                let output = state
                    .outputs
                    .get(&slot.output.unwrap())
                    .context("selected output disconnected")?;
                slot.transform = output.transform;
                slot.inverted = false;
                slot.spec = None;
                slot.wlr_frame = Some(state.wlr_copy.as_ref().unwrap().capture_output(
                    i32::from(self.cursor),
                    &output.proxy,
                    &qh,
                    i,
                ));
            }
        }
        Ok(())
    }

    fn consume_ready(&mut self) -> Result<bool> {
        let mut changed = false;
        for slot in &mut self.runtime.state.slots {
            if !slot.ready {
                continue;
            }
            slot.ready = false;
            slot.retries = 0;
            let buffer = slot
                .buffer
                .as_ref()
                .context("capture completed without a buffer")?;
            slot.last = Some(pixels::decode(
                &buffer.read()?,
                buffer.spec,
                slot.inverted,
                slot.transform,
            )?);
            changed = true;
        }
        Ok(changed)
    }

    fn render(&self) -> Result<CapturedFrame> {
        let state = &self.runtime.state;
        let slot = &state.slots[0];
        let image = slot.last.as_ref().unwrap();
        match &self.target {
            CaptureTarget::Toplevel(_) => Ok(CapturedFrame {
                logical_width: image.width(),
                logical_height: image.height(),
                image: image.clone(),
            }),
            CaptureTarget::Output(_) => {
                let rect = state.outputs[&slot.output.unwrap()].rect();
                Ok(CapturedFrame {
                    image: image.clone(),
                    logical_width: rect.width,
                    logical_height: rect.height,
                })
            }
            CaptureTarget::Region(region) => pixels::crop(
                image,
                state.outputs[&slot.output.unwrap()].rect(),
                region.rect,
            ),
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
            frame::Event::Ready => {
                slot.ready = true;
                slot.frame = None;
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
    // Plain copy has bounded compositor work even when output content is static.
    proxy.copy(&slot.buffer.as_ref().unwrap().proxy);
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
                state.slots[*index].ready = true;
                state.slots[*index].wlr_frame = None;
                proxy.destroy();
            }
            wlr_frame::Event::Failed => {
                state.slots[*index].wlr_frame = None;
                proxy.destroy();
                state.error = Some(anyhow::anyhow!(
                    "screen capture failed: compositor rejected the frame"
                ));
            }
            _ => {}
        }
    }
}
