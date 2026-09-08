use crate::{Rect, Region, connection::*, pixels::BufferSpec};
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) struct Overlay {
    output: u32,
    surface: wl_surface::WlSurface,
    layer: layer_surface::ZwlrLayerSurfaceV1,
    size: Option<(u32, u32)>,
    scale: i32,
    pub buffers: Vec<Buffer>,
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
    fn dirty(&mut self) {
        for overlay in &mut self.overlays {
            overlay.dirty = true;
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
            buffers: Vec::new(),
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
        if overlay.scale != output.scale {
            overlay.scale = output.scale;
            overlay.dirty = true;
        }
        if !overlay.dirty {
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
        overlay.buffers.retain(|b| b.busy || b.spec == spec);
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
            overlay.buffers.len() - 1
        };
        let buffer = &mut overlay.buffers[index];
        let bounds = Rect {
            x: output.position.0,
            y: output.position.1,
            width,
            height,
        };
        let bytes = draw_overlay(spec, bounds, selection)?;
        buffer.write(&bytes)?;
        buffer.busy = true;
        overlay.surface.set_buffer_scale(scale as i32);
        overlay.surface.attach(Some(&buffer.proxy), 0, 0);
        overlay.surface.damage(0, 0, width as i32, height as i32);
        overlay.surface.commit();
        overlay.dirty = false;
    }
    Ok(())
}

fn draw_overlay(spec: BufferSpec, logical: Rect, selection: Option<Rect>) -> Result<Vec<u8>> {
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
    if let Some(selector) = &mut state.selector {
        selector.drag.motion(
            seat_id,
            (
                output.position.0.saturating_add(x.floor() as i32),
                output.position.1.saturating_add(y.floor() as i32),
            ),
        );
        selector.dirty();
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
                        selector.drag.start(*id);
                        selector.dirty();
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
                    selector.drag.square = down;
                    selector.dirty();
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
                    selector.drag.start(*seat_id);
                    selector.dirty();
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
}
