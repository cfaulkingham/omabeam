use crate::{Rect, Region, connection::*, pixels::BufferSpec};
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

pub(crate) struct Overlay {
    output: u32,
    surface: wl_surface::WlSurface,
    layer: layer_surface::ZwlrLayerSurfaceV1,
    size: Option<(u32, u32)>,
    scale: i32,
    origin: (i32, i32),
    pub buffers: Vec<Buffer>,
    /// Parallel to `buffers` (the release handler only knows `Buffer`): the
    /// frame each one holds, or `None` until it has been drawn in full.
    drawn: Vec<Option<Frame>>,
    /// What the compositor shows, for damage; `None` forces full damage.
    shown: Option<Shown>,
    /// The last commit's `wl_surface.frame` has not fired; redraws wait for it.
    frame_pending: bool,
    scratch: Vec<u8>,
    dirty: bool,
}
impl Drop for Overlay {
    fn drop(&mut self) {
        self.layer.destroy();
        self.surface.destroy();
    }
}

pub(crate) struct Seat {
    proxy: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    touch: Option<wl_touch::WlTouch>,
    cursor: Option<(wl_surface::WlSurface, Buffer)>,
    output: Option<u32>,
    touch_id: Option<i32>,
}
impl Seat {
    pub fn new(proxy: wl_seat::WlSeat) -> Self {
        Self {
            proxy,
            pointer: None,
            keyboard: None,
            touch: None,
            cursor: None,
            output: None,
            touch_id: None,
        }
    }
}
impl Drop for Seat {
    fn drop(&mut self) {
        if let Some(p) = self.pointer.take()
            && p.version() >= 3
        {
            p.release();
        }
        if let Some(k) = self.keyboard.take()
            && k.version() >= 3
        {
            k.release();
        }
        if let Some(t) = self.touch.take()
            && t.version() >= 3
        {
            t.release();
        }
        if let Some((s, _)) = self.cursor.take() {
            s.destroy();
        }
        if self.proxy.version() >= 5 {
            self.proxy.release();
        }
    }
}

#[derive(Default)]
struct Drag {
    anchor: Option<(i32, i32)>,
    position: (i32, i32),
    owner: Option<u32>,
    moving: bool,
    square: bool,
}
impl Drag {
    fn motion(&mut self, seat: u32, position: (i32, i32)) {
        if self.owner.is_some_and(|owner| owner != seat) {
            return;
        }
        if self.moving
            && let Some(anchor) = &mut self.anchor
        {
            anchor.0 = anchor
                .0
                .saturating_add(position.0.saturating_sub(self.position.0));
            anchor.1 = anchor
                .1
                .saturating_add(position.1.saturating_sub(self.position.1));
        }
        self.position = position;
    }
    fn start(&mut self, seat: u32) {
        if self.owner.is_none() {
            self.owner = Some(seat);
            self.anchor = Some(self.position);
        }
    }
    fn rect(&self) -> Option<Rect> {
        let (ax, ay) = self.anchor?;
        let (x, y) = self.position;
        let mut width = ax.abs_diff(x).saturating_add(1);
        let mut height = ay.abs_diff(y).saturating_add(1);
        if self.square {
            width = width.max(height);
            height = width;
        }
        let x = if x >= ax {
            ax
        } else {
            ax.saturating_sub((width - 1).min(i32::MAX as u32) as i32)
        };
        let y = if y >= ay {
            ay
        } else {
            ay.saturating_sub((height - 1).min(i32::MAX as u32) as i32)
        };
        Some(Rect {
            x,
            y,
            width,
            height,
        })
    }
}

pub(crate) struct Selector {
    pub overlays: Vec<Overlay>,
    drag: Drag,
    result: Option<Result<Region>>,
}
impl Selector {
    /// Applies an input change to the drag and marks only the overlays whose
    /// image it can change: those the old or the new rect overlaps.
    fn update(&mut self, outputs: &HashMap<u32, Output>, change: impl FnOnce(&mut Drag)) {
        let before = self.drag.rect();
        change(&mut self.drag);
        let after = self.drag.rect();
        if before == after {
            return;
        }
        for overlay in &mut self.overlays {
            let bounds =
                overlay
                    .size
                    .zip(outputs.get(&overlay.output))
                    .map(|((width, height), output)| Rect {
                        x: output.position.0,
                        y: output.position.1,
                        width,
                        height,
                    });
            // An overlay without a size yet is drawn in full once configured.
            overlay.dirty |= bounds.is_none_or(|bounds| {
                [before, after]
                    .into_iter()
                    .flatten()
                    .any(|rect| rect.intersect(bounds).is_some())
            });
        }
    }
    fn cancel(&mut self) {
        if self.result.is_none() {
            self.result = Some(Err(anyhow::anyhow!("region selection cancelled")));
        }
    }
    fn finish(&mut self, seat: u32, outputs: &HashMap<u32, Output>) {
        // A dispatch can contain both Escape and a queued button release.
        // The first terminal event wins, regardless of later input in the batch.
        if self.result.is_some() || self.drag.owner != Some(seat) {
            return;
        }
        let Some(rect) = self.drag.rect() else {
            return;
        };
        let mut outputs: Vec<_> = outputs.values().collect();
        outputs.sort_by(|a, b| a.name.cmp(&b.name));
        self.result = Some(region_in_output(
            rect,
            outputs.into_iter().map(|o| (o.name.as_str(), o.rect())),
        ));
    }
}

/// Match the portal's single-output selection contract: the output containing
/// the top-left corner owns the region; dimensions are clipped to that output.
fn region_in_output<'a>(
    rect: Rect,
    outputs: impl Iterator<Item = (&'a str, Rect)>,
) -> Result<Region> {
    let (name, output) = outputs
        .into_iter()
        .find(|(_, o)| {
            i64::from(rect.x) >= i64::from(o.x)
                && i64::from(rect.x) < o.right()
                && i64::from(rect.y) >= i64::from(o.y)
                && i64::from(rect.y) < o.bottom()
        })
        .context("region selection cancelled: start of region is outside all outputs")?;
    let clipped = rect
        .intersect(output)
        .context("region selection cancelled: empty region")?;
    ensure!(!name.is_empty(), "selected output has no name");
    Ok(Region {
        output: name.to_string(),
        rect: Rect {
            x: clipped.x - output.x,
            y: clipped.y - output.y,
            ..clipped
        },
    })
}

pub fn pick_region() -> Result<Region> {
    select(Runtime::connect(true)?)
}

pub(crate) fn select(mut runtime: Runtime) -> Result<Region> {
    let result = run_selector(&mut runtime);
    // Destroy/unmap every overlay, including on cancellation or a failed draw.
    // The sync ensures the selector is removed before a screenshot is requested.
    runtime.state.selector = None;
    runtime.state.seats.clear();
    let cleanup = runtime.sync();
    match result {
        Ok(region) => {
            cleanup?;
            Ok(region)
        }
        Err(error) => Err(error),
    }
}

fn run_selector(runtime: &mut Runtime) -> Result<Region> {
    let state = &mut runtime.state;
    let compositor = state
        .compositor
        .as_ref()
        .context("region selection requires wl_compositor")?;
    let layer_shell = state
        .layer_shell
        .as_ref()
        .context("region selection requires layer-shell support")?;
    ensure!(
        !state.seats.is_empty(),
        "region selection requires an input seat"
    );
    let qh = runtime.queue.handle();
    let mut overlays = Vec::new();
    for (id, output) in &state.outputs {
        if output.rect().width == 0 || output.rect().height == 0 {
            continue;
        }
        let surface = compositor.create_surface(&qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            Some(&output.proxy),
            layer_shell::Layer::Overlay,
            "omabeam-region".into(),
            &qh,
            *id,
        );
        layer.set_anchor(
            layer_surface::Anchor::Top
                | layer_surface::Anchor::Bottom
                | layer_surface::Anchor::Left
                | layer_surface::Anchor::Right,
        );
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(layer_surface::KeyboardInteractivity::Exclusive);
        surface.commit();
        overlays.push(Overlay {
            output: *id,
            surface,
            layer,
            size: None,
            scale: output.scale,
            origin: output.position,
            buffers: Vec::new(),
            drawn: Vec::new(),
            shown: None,
            frame_pending: false,
            scratch: Vec::new(),
            dirty: true,
        });
    }
    ensure!(
        !overlays.is_empty(),
        "no outputs available for region selection"
    );
    state.selector = Some(Selector {
        overlays,
        drag: Drag::default(),
        result: None,
    });
    let started = Instant::now();
    loop {
        runtime.pump(Duration::from_millis(16))?;
        if let Some(result) = runtime.state.selector.as_mut().unwrap().result.take() {
            return result;
        }
        let selector = runtime.state.selector.as_ref().unwrap();
        if started.elapsed() >= SETUP_TIMEOUT && selector.overlays.iter().any(|o| o.size.is_none())
        {
            bail!("region selection overlay did not become ready");
        }
        render_overlays(&mut runtime.state, &qh)?;
    }
}

fn render_overlays(state: &mut State, qh: &QueueHandle<State>) -> Result<()> {
    let selector = state.selector.as_mut().unwrap();
    let selection = selector.drag.rect();
    for overlay in &mut selector.overlays {
        let Some((width, height)) = overlay.size else {
            continue;
        };
        let output = state
            .outputs
            .get(&overlay.output)
            .context("selected output disconnected")?;
        if overlay.scale != output.scale || overlay.origin != output.position {
            overlay.scale = output.scale;
            overlay.origin = output.position;
            overlay.dirty = true;
        }
        // The compositor paces redraws: one commit per frame callback.
        if !overlay.dirty || overlay.frame_pending {
            continue;
        }
        let scale = overlay.scale.max(1) as u32;
        let spec = BufferSpec::packed(
            width.checked_mul(scale).context("overlay width overflow")?,
            height
                .checked_mul(scale)
                .context("overlay height overflow")?,
            wl_shm::Format::Argb8888,
        )?;
        let bounds = Rect {
            x: output.position.0,
            y: output.position.1,
            width,
            height,
        };
        let frame = overlay_frame(spec, bounds, selection);
        let damage = match overlay.shown {
            Some(shown) if shown.spec == spec && shown.scale == scale => {
                changed(shown.frame, frame)
            }
            _ => Some([0, 0, spec.width, spec.height]),
        };
        let Some([x0, y0, x1, y1]) = damage else {
            // The rect changed elsewhere; this overlay's image did not.
            overlay.dirty = false;
            continue;
        };
        retain_parallel(&mut overlay.buffers, &mut overlay.drawn, |b| {
            b.busy || b.spec == spec
        });
        let free = overlay
            .buffers
            .iter()
            .position(|b| !b.busy && b.spec == spec);
        let index = if let Some(index) = free {
            index
        } else {
            // Wait for release instead of allocating endlessly if the compositor stalls.
            if overlay.buffers.len() >= 3 {
                continue;
            }
            overlay
                .buffers
                .push(Buffer::new(state.shm.as_ref().unwrap(), spec, qh)?);
            overlay.drawn.push(None);
            overlay.buffers.len() - 1
        };
        // Rewrite what this buffer holds, which may be older than what is shown.
        let rows = stale_rows(overlay.drawn[index], frame, spec.height);
        if !rows.is_empty() {
            draw_rows(spec, frame, rows.clone(), &mut overlay.scratch);
            overlay.buffers[index].write_rows(rows.start, &overlay.scratch)?;
        }
        overlay.drawn[index] = Some(frame);
        let buffer = &mut overlay.buffers[index];
        buffer.busy = true;
        overlay.surface.set_buffer_scale(scale as i32);
        overlay.surface.attach(Some(&buffer.proxy), 0, 0);
        if overlay.surface.version() >= 4 {
            overlay
                .surface
                .damage_buffer(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32);
        } else {
            overlay.surface.damage(0, 0, width as i32, height as i32);
        }
        overlay.surface.frame(qh, OverlayFrame(overlay.output));
        overlay.surface.commit();
        overlay.frame_pending = true;
        overlay.shown = Some(Shown { spec, scale, frame });
        overlay.dirty = false;
    }
    Ok(())
}

/// Removes the items `keep` rejects, and the entries at the same positions of
/// `parallel`.
fn retain_parallel<T, U>(items: &mut Vec<T>, parallel: &mut Vec<U>, keep: impl Fn(&T) -> bool) {
    let mut i = 0;
    while i < items.len() {
        if keep(&items[i]) {
            i += 1;
        } else {
            items.remove(i);
            parallel.remove(i);
        }
    }
}

/// Buffer-pixel edges of the selection on one overlay, as half-open ranges per
/// axis: the selection, and the part inside its 2 px border.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Spans {
    x: (u32, u32),
    y: (u32, u32),
    inner_x: (u32, u32),
    inner_y: (u32, u32),
}
/// An overlay image; `None` is dim throughout.
type Frame = Option<Spans>;
/// A committed frame and the buffer geometry it was committed with.
#[derive(Clone, Copy)]
struct Shown {
    spec: BufferSpec,
    scale: u32,
    frame: Frame,
}

const DIM: [u8; 4] = 0x66000000u32.to_ne_bytes();
const CLEAR: [u8; 4] = 0u32.to_ne_bytes();
const BORDER: [u8; 4] = 0xff82aaffu32.to_ne_bytes();

/// The selection on an overlay whose buffer `spec` covers `logical`.
fn overlay_frame(spec: BufferSpec, logical: Rect, selection: Option<Rect>) -> Frame {
    let r = selection?;
    let scale = spec.width as f64 / logical.width as f64;
    let columns = |from, to| {
        let (x, width) = (logical.x, spec.width);
        (edge(x, scale, width, from), edge(x, scale, width, to))
    };
    let rows = |from, to| {
        let (y, height) = (logical.y, spec.height);
        (edge(y, scale, height, from), edge(y, scale, height, to))
    };
    let (left, top) = (r.x as f64, r.y as f64);
    let (right, bottom) = (r.right() as f64, r.bottom() as f64);
    let (x, y) = (columns(left, right), rows(top, bottom));
    (x.0 < x.1 && y.0 < y.1).then(|| Spans {
        x,
        y,
        inner_x: columns(left + 2.0, right - 2.0),
        inner_y: rows(top + 2.0, bottom - 2.0),
    })
}

/// The first buffer pixel in `0..=len` whose logical coordinate reaches `at`.
/// This reproduces the old per-pixel f64 edge tests (see the reference test).
fn edge(origin: i32, scale: f64, len: u32, at: f64) -> u32 {
    ((at - f64::from(origin)) * scale)
        .ceil()
        .clamp(0.0, f64::from(len)) as u32
}

/// Draws whole `rows` of `frame` into `out`. There are three kinds of row
/// (dim, border, inside); each is drawn once, then copied.
fn draw_rows(spec: BufferSpec, frame: Frame, rows: Range<u32>, out: &mut Vec<u8>) {
    let stride = spec.stride as usize;
    out.clear();
    out.reserve(rows.len() * stride);
    let mut first = [None; 3];
    for y in rows {
        let kind = match frame {
            Some(s) if (s.y.0..s.y.1).contains(&y) => {
                1 + usize::from((s.inner_y.0..s.inner_y.1).contains(&y))
            }
            _ => 0,
        };
        let start = out.len();
        if let Some(row) = first[kind] {
            out.extend_from_within(row..row + stride);
            continue;
        }
        first[kind] = Some(start);
        out.extend(std::iter::repeat_n(DIM, spec.width as usize).flatten());
        out.resize(start + stride, 0);
        if let Some(s) = frame
            && kind > 0
        {
            let row = &mut out[start..];
            let mut paint = |columns: (u32, u32), pixel: [u8; 4]| {
                let bytes = &mut row[columns.0 as usize * 4..columns.1 as usize * 4];
                for p in bytes.chunks_exact_mut(4) {
                    p.copy_from_slice(&pixel);
                }
            };
            paint(s.x, BORDER);
            if kind == 2 && s.inner_x.0 < s.inner_x.1 {
                paint(s.inner_x, CLEAR);
            }
        }
    }
}

/// The buffer-pixel box `[x0, y0, x1, y1]` outside which two frames are the
/// same, or `None` when they are identical.
fn changed(a: Frame, b: Frame) -> Option<[u32; 4]> {
    if a == b {
        return None;
    }
    [a, b]
        .into_iter()
        .flatten()
        .map(|s| [s.x.0, s.y.0, s.x.1, s.y.1])
        .reduce(|p, q| {
            [
                p[0].min(q[0]),
                p[1].min(q[1]),
                p[2].max(q[2]),
                p[3].max(q[3]),
            ]
        })
}

/// The rows of a buffer holding `held` that differ from `frame`; all of them
/// when the buffer's content is unknown.
fn stale_rows(held: Option<Frame>, frame: Frame, height: u32) -> Range<u32> {
    match held {
        None => 0..height,
        Some(held) => changed(held, frame).map_or(0..0, |[_, top, _, bottom]| top..bottom),
    }
}

/// The old per-pixel renderer: the reference `draw_rows` must match.
#[cfg(test)]
pub(crate) fn draw_overlay(
    spec: BufferSpec,
    logical: Rect,
    selection: Option<Rect>,
) -> Result<Vec<u8>> {
    let mut bytes = vec![0; spec.byte_len()?];
    let scale = spec.width as f64 / logical.width as f64;
    for y in 0..spec.height {
        let gy = logical.y as f64 + y as f64 / scale;
        for x in 0..spec.width {
            let gx = logical.x as f64 + x as f64 / scale;
            let inside = selection.is_some_and(|r| {
                gx >= r.x as f64
                    && gx < r.right() as f64
                    && gy >= r.y as f64
                    && gy < r.bottom() as f64
            });
            let border = inside
                && selection.is_some_and(|r| {
                    gx < r.x as f64 + 2.0
                        || gx >= r.right() as f64 - 2.0
                        || gy < r.y as f64 + 2.0
                        || gy >= r.bottom() as f64 - 2.0
                });
            let pixel: u32 = if border {
                0xff82aaff
            } else if inside {
                0x00000000
            } else {
                0x66000000
            };
            let offset = y as usize * spec.stride as usize + x as usize * 4;
            bytes[offset..offset + 4].copy_from_slice(&pixel.to_ne_bytes());
        }
    }
    Ok(bytes)
}

fn move_pointer(state: &mut State, seat_id: u32, x: f64, y: f64) {
    let Some(seat) = state.seats.get(&seat_id) else {
        return;
    };
    let Some(output) = seat.output.and_then(|id| state.outputs.get(&id)) else {
        return;
    };
    let position = (
        output.position.0.saturating_add(x.floor() as i32),
        output.position.1.saturating_add(y.floor() as i32),
    );
    if let Some(selector) = &mut state.selector {
        selector.update(&state.outputs, |drag| drag.motion(seat_id, position));
    }
}
fn surface_output(state: &State, surface: &wl_surface::WlSurface) -> Option<u32> {
    state
        .selector
        .as_ref()?
        .overlays
        .iter()
        .find(|o| o.surface == *surface)
        .map(|o| o.output)
}
fn set_cursor(
    state: &mut State,
    pointer: &wl_pointer::WlPointer,
    seat_id: u32,
    serial: u32,
    qh: &QueueHandle<State>,
) -> Result<()> {
    if let Some(manager) = &state.cursor_manager {
        let device = manager.get_pointer(pointer, qh, ());
        device.set_shape(serial, cursor_device::Shape::Crosshair);
        device.destroy();
    } else {
        let Some(seat) = state.seats.get_mut(&seat_id) else {
            return Ok(());
        };
        if seat.cursor.is_none() {
            let surface = state
                .compositor
                .as_ref()
                .context("region selector has no compositor")?
                .create_surface(qh, ());
            let spec = BufferSpec::packed(24, 24, wl_shm::Format::Argb8888)?;
            let buffer = Buffer::new(state.shm.as_ref().unwrap(), spec, qh)?;
            let mut bytes = vec![0; spec.byte_len()?];
            for y in 0..24usize {
                for x in 0..24usize {
                    let pixel = if x == 11 || y == 11 {
                        0xffffffffu32
                    } else if (10..=12).contains(&x) || (10..=12).contains(&y) {
                        0xff000000
                    } else {
                        0
                    };
                    bytes[(y * 24 + x) * 4..(y * 24 + x + 1) * 4]
                        .copy_from_slice(&pixel.to_ne_bytes());
                }
            }
            buffer.write(&bytes)?;
            surface.attach(Some(&buffer.proxy), 0, 0);
            surface.damage(0, 0, 24, 24);
            surface.commit();
            seat.cursor = Some((surface, buffer));
        }
        pointer.set_cursor(serial, Some(&seat.cursor.as_ref().unwrap().0), 11, 11);
    }
    Ok(())
}

/// An overlay's `wl_surface.frame` callback, by output.
pub(crate) struct OverlayFrame(u32);
impl Dispatch<wl_callback::WlCallback, OverlayFrame> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        frame: &OverlayFrame,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `done` is the only event.
        if let Some(overlay) = state
            .selector
            .as_mut()
            .and_then(|s| s.overlays.iter_mut().find(|o| o.output == frame.0))
        {
            overlay.frame_pending = false;
        }
    }
}

impl Dispatch<layer_surface::ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &layer_surface::ZwlrLayerSurfaceV1,
        event: layer_surface::Event,
        output: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(selector) = &mut state.selector else {
            return;
        };
        match event {
            layer_surface::Event::Configure {
                serial,
                width,
                height,
            } => {
                proxy.ack_configure(serial);
                if width == 0 || height == 0 {
                    state.error = Some(anyhow::anyhow!("region selector received an empty output"));
                    return;
                }
                if let Some(overlay) = selector.overlays.iter_mut().find(|o| o.output == *output) {
                    overlay.size = Some((width, height));
                    overlay.dirty = true;
                }
            }
            layer_surface::Event::Closed => selector.cancel(),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(id) else {
            return;
        };
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            let mut lost_input = false;
            if !caps.contains(wl_seat::Capability::Pointer)
                && let Some(pointer) = seat.pointer.take()
            {
                if pointer.version() >= 3 {
                    pointer.release();
                }
                lost_input = true;
            }
            if !caps.contains(wl_seat::Capability::Keyboard)
                && let Some(keyboard) = seat.keyboard.take()
            {
                if keyboard.version() >= 3 {
                    keyboard.release();
                }
                lost_input = true;
            }
            if !caps.contains(wl_seat::Capability::Touch)
                && let Some(touch) = seat.touch.take()
            {
                if touch.version() >= 3 {
                    touch.release();
                }
                lost_input = true;
            }
            if lost_input && let Some(selector) = &mut state.selector {
                selector.cancel();
            }
            if caps.contains(wl_seat::Capability::Pointer) && seat.pointer.is_none() {
                seat.pointer = Some(proxy.get_pointer(qh, *id));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && seat.keyboard.is_none() {
                seat.keyboard = Some(proxy.get_keyboard(qh, *id));
            }
            if caps.contains(wl_seat::Capability::Touch) && seat.touch.is_none() {
                seat.touch = Some(proxy.get_touch(qh, *id));
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if state.selector.is_none() || !state.seats.contains_key(id) {
            return;
        }
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                let output = surface_output(state, &surface);
                if let Some(seat) = state.seats.get_mut(id) {
                    seat.output = output;
                }
                move_pointer(state, *id, surface_x, surface_y);
                if let Err(e) = set_cursor(state, proxy, *id, serial, qh) {
                    state.error = Some(e);
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => move_pointer(state, *id, surface_x, surface_y),
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                if let Some(selector) = &mut state.selector {
                    if button != 0x110 {
                        if button_state == wl_pointer::ButtonState::Pressed {
                            selector.cancel();
                        }
                    } else if button_state == wl_pointer::ButtonState::Pressed {
                        selector.update(&state.outputs, |drag| drag.start(*id));
                    } else {
                        selector.finish(*id, &state.outputs);
                    }
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(selector) = &mut state.selector else {
            return;
        };
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(key_state),
            ..
        } = event
        {
            let down = key_state == wl_keyboard::KeyState::Pressed;
            // Wayland key events use evdev codes. These controls need no text
            // interpretation or keyboard layout library; keymap FDs are dropped.
            if key == 1 && down {
                selector.cancel();
            }
            if selector.drag.owner.is_none_or(|owner| owner == *id) {
                if key == 57 {
                    selector.drag.moving = down;
                }
                if key == 42 || key == 54 {
                    selector.update(&state.outputs, |drag| drag.square = down);
                }
            }
        }
    }
}
impl Dispatch<wl_touch::WlTouch, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        seat_id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_touch::Event::Down {
                surface, id, x, y, ..
            } => {
                let output = surface_output(state, &surface);
                if let Some(seat) = state.seats.get_mut(seat_id) {
                    if seat.touch_id.is_some() {
                        return;
                    }
                    seat.output = output;
                    seat.touch_id = Some(id);
                }
                move_pointer(state, *seat_id, x, y);
                if let Some(selector) = &mut state.selector {
                    selector.update(&state.outputs, |drag| drag.start(*seat_id));
                }
            }
            wl_touch::Event::Motion { id, x, y, .. }
                if state
                    .seats
                    .get(seat_id)
                    .is_some_and(|s| s.touch_id == Some(id)) =>
            {
                move_pointer(state, *seat_id, x, y)
            }
            wl_touch::Event::Up { id, .. }
                if state
                    .seats
                    .get(seat_id)
                    .is_some_and(|s| s.touch_id == Some(id)) =>
            {
                if let Some(selector) = &mut state.selector {
                    selector.finish(*seat_id, &state.outputs);
                }
                if let Some(seat) = state.seats.get_mut(seat_id) {
                    seat.touch_id = None;
                }
            }
            wl_touch::Event::Cancel => {
                if let Some(selector) = &mut state.selector {
                    selector.cancel();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_crossing_outputs_is_clipped_and_output_relative() {
        let outputs = [
            (
                "left",
                Rect {
                    x: -200,
                    y: 50,
                    width: 200,
                    height: 100,
                },
            ),
            (
                "right",
                Rect {
                    x: 0,
                    y: 50,
                    width: 200,
                    height: 100,
                },
            ),
        ];
        let region = region_in_output(
            Rect {
                x: -20,
                y: 60,
                width: 50,
                height: 20,
            },
            outputs.into_iter(),
        )
        .unwrap();
        assert_eq!(
            region,
            Region {
                output: "left".into(),
                rect: Rect {
                    x: 180,
                    y: 10,
                    width: 20,
                    height: 20
                }
            }
        );
    }
    #[test]
    fn reverse_drag_single_click_square_and_move() {
        let mut drag = Drag::default();
        drag.motion(1, (100, 100));
        drag.start(1);
        drag.motion(1, (80, 90));
        assert_eq!(
            drag.rect(),
            Some(Rect {
                x: 80,
                y: 90,
                width: 21,
                height: 11
            })
        );
        drag.square = true;
        assert_eq!(drag.rect().unwrap().height, 21);
        drag.moving = true;
        drag.motion(1, (90, 100));
        assert_eq!(drag.rect().unwrap().x, 90);
        drag.motion(2, (500, 500));
        assert_eq!(drag.position, (90, 100));
        let drag = Drag {
            anchor: Some((10, 20)),
            position: (10, 20),
            ..Drag::default()
        };
        assert_eq!(
            drag.rect().unwrap(),
            Rect {
                x: 10,
                y: 20,
                width: 1,
                height: 1
            }
        );
    }
    #[test]
    fn overlay_is_transparent_inside_and_dim_outside_with_border() {
        let spec = BufferSpec::packed(20, 20, wl_shm::Format::Argb8888).unwrap();
        let bytes = draw_overlay(
            spec,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 20,
            },
            Some(Rect {
                x: 5,
                y: 5,
                width: 10,
                height: 10,
            }),
        )
        .unwrap();
        let pixel = |x: usize, y: usize| {
            u32::from_ne_bytes(
                bytes[(y * 20 + x) * 4..(y * 20 + x + 1) * 4]
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(pixel(0, 0), 0x66000000);
        assert_eq!(pixel(5, 5), 0xff82aaff);
        assert_eq!(pixel(10, 10), 0);
    }

    /// Deterministic pseudo-random values for broad boundary coverage.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, range: std::ops::Range<i32>) -> i32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            range.start + ((self.0 >> 33) % (range.end - range.start) as u64) as i32
        }
    }
    /// The first pixel `(x, y)` where two images differ.
    fn mismatch(a: &[u8], b: &[u8], width: u32) -> Option<(usize, usize)> {
        assert_eq!(a.len(), b.len());
        let i = a.chunks(4).zip(b.chunks(4)).position(|(p, q)| p != q)?;
        Some((i % width as usize, i / width as usize))
    }

    #[test]
    fn span_renderer_matches_the_per_pixel_reference() {
        let mut rng = Lcg(7);
        let mut out = Vec::new();
        // A 40x24 logical overlay at scales 1, 1.25, 1.5, and 2.
        for (width, height) in [(40, 24), (50, 30), (60, 36), (80, 48)] {
            let spec = BufferSpec::packed(width, height, wl_shm::Format::Argb8888).unwrap();
            let stride = spec.stride as usize;
            for (x, y) in [(0, 0), (-40, -24), (-1000, 7), (13, -5)] {
                let logical = Rect {
                    x,
                    y,
                    width: 40,
                    height: 24,
                };
                let rect = |dx: i32, dy: i32, width: u32, height: u32| {
                    Some(Rect {
                        x: x + dx,
                        y: y + dy,
                        width,
                        height,
                    })
                };
                let mut selections = vec![
                    None,
                    Some(logical),
                    rect(-10, -10, 25, 20),
                    rect(30, 15, 25, 20),
                    rect(-5, -5, 50, 34),
                    rect(40, 0, 5, 5),
                    rect(-5, 0, 5, 5),
                    Some(Rect {
                        x: i32::MIN,
                        y: i32::MIN,
                        width: u32::MAX,
                        height: u32::MAX,
                    }),
                    Some(Rect {
                        x: i32::MAX,
                        y: i32::MAX,
                        width: u32::MAX,
                        height: 1,
                    }),
                ];
                // Rects no wider than the border on each side.
                selections.extend((1..6).map(|size| rect(3, 4, size, size)));
                for _ in 0..300 {
                    let (dx, dy) = (rng.next(-12..48), rng.next(-12..32));
                    let (w, h) = (rng.next(1..30) as u32, rng.next(1..30) as u32);
                    selections.push(rect(dx, dy, w, h));
                }
                for selection in selections {
                    let reference = draw_overlay(spec, logical, selection).unwrap();
                    let frame = overlay_frame(spec, logical, selection);
                    draw_rows(spec, frame, 0..height, &mut out);
                    assert_eq!(
                        mismatch(&out, &reference, width),
                        None,
                        "{width}x{height} buffer at ({x}, {y}), {selection:?}"
                    );
                    let top = rng.next(0..height as i32) as u32;
                    let bottom = rng.next(top as i32..height as i32 + 1) as u32;
                    draw_rows(spec, frame, top..bottom, &mut out);
                    assert!(
                        out == reference[top as usize * stride..bottom as usize * stride],
                        "rows {top}..{bottom} of {width}x{height} at ({x}, {y}), {selection:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn dropped_buffers_take_their_drawn_record_with_them() {
        let mut buffers = vec![1, 2, 3, 4, 5, 6];
        let mut drawn = vec!['a', 'b', 'c', 'd', 'e', 'f'];
        retain_parallel(&mut buffers, &mut drawn, |b| b % 3 != 0 && *b != 1);
        assert_eq!((buffers, drawn), (vec![2, 4, 5], vec!['b', 'd', 'e']));
    }

    #[test]
    fn partial_redraws_keep_every_buffer_and_the_shown_image_exact() {
        // Buffers come back in any order, and the compositor copies only the
        // damaged box into what it shows, as wlroots does for shm buffers.
        let spec = BufferSpec::packed(60, 36, wl_shm::Format::Argb8888).unwrap();
        let logical = Rect {
            x: -40,
            y: 10,
            width: 40,
            height: 24,
        };
        let stride = spec.stride as usize;
        let fresh = || (vec![0; spec.byte_len().unwrap()], None);
        let mut rng = Lcg(11);
        let mut buffers: Vec<(Vec<u8>, Option<Frame>)> = Vec::new();
        let mut shown: Option<(Vec<u8>, Frame)> = None;
        let mut selection = None;
        let mut out = Vec::new();
        let mut partial = 0;
        for step in 0..3000 {
            let random = |rng: &mut Lcg| Rect {
                x: rng.next(-50..10),
                y: rng.next(0..40),
                width: rng.next(1..60) as u32,
                height: rng.next(1..40) as u32,
            };
            selection = match (rng.next(0..8), selection) {
                (0, _) => None,
                (1, same) => same,
                (2, _) | (_, None) => Some(random(&mut rng)),
                // A drag step: move or resize by a few pixels.
                (_, Some(r)) => Some(Rect {
                    x: r.x + rng.next(-1..2),
                    y: r.y + rng.next(-1..2),
                    width: (r.width as i32 + rng.next(-3..4)).max(1) as u32,
                    height: (r.height as i32 + rng.next(-3..4)).max(1) as u32,
                }),
            };
            let frame = overlay_frame(spec, logical, selection);
            let damage = match &shown {
                Some((_, on_screen)) => changed(*on_screen, frame),
                None => Some([0, 0, spec.width, spec.height]),
            };
            let Some([x0, y0, x1, y1]) = damage else {
                continue;
            };
            // Any buffer may be the free one; some are newly allocated.
            let index = match rng.next(0..4) as usize {
                i if i < buffers.len() => i,
                _ if buffers.len() < 3 => {
                    buffers.push(fresh());
                    buffers.len() - 1
                }
                i => i % buffers.len(),
            };
            if rng.next(0..40) == 0 {
                buffers[index] = fresh();
            }
            let (pixels, held) = &mut buffers[index];
            let rows = stale_rows(*held, frame, spec.height);
            draw_rows(spec, frame, rows.clone(), &mut out);
            pixels[rows.start as usize * stride..rows.end as usize * stride].copy_from_slice(&out);
            *held = Some(frame);
            partial += usize::from(rows.len() < spec.height as usize);
            let expected = draw_overlay(spec, logical, selection).unwrap();
            assert_eq!(
                mismatch(pixels, &expected, spec.width),
                None,
                "buffer {index} at step {step}"
            );
            let (image, on_screen) = shown.get_or_insert_with(|| (vec![0; expected.len()], None));
            for y in y0 as usize..y1 as usize {
                let span = y * stride + x0 as usize * 4..y * stride + x1 as usize * 4;
                image[span.clone()].copy_from_slice(&pixels[span]);
            }
            assert_eq!(
                mismatch(image, &expected, spec.width),
                None,
                "shown image at step {step}"
            );
            *on_screen = frame;
        }
        assert!(partial > 500, "only {partial} partial redraws");
    }
}
