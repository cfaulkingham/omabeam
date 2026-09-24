use super::*;
use crate::localsend::{self, Device, Discovery, PinRejected, SenderInfo};
use anyhow::Context as _;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::{AnyElement, Entity, Image, ImageFormat, img};
use gpui_omarchy::input as text_input;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// LocalSend application icon (Apache-2.0),
/// https://github.com/localsend/localsend app/assets/img/logo-256.png
const LOCALSEND_LOGO: &[u8] = include_bytes!("assets/localsend-logo.png");
const NO_DEVICE_TITLE: &str = "Open LocalSend on the client device";
const QR_CAPTION: &str = "Scan with a camera on the same network.";

/// Distinct from the picker's "omabeam": Hyprland treats a window with that
/// class as the picker. install.sh floats this one with its own rule.
const APP_ID: &str = "omabeam-send";
const WINDOW_W: f32 = 760.0;
const WINDOW_H: f32 = 560.0;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SendPhase {
    Starting,
    Ready,
    Sending {
        alias: String,
    },
    /// The receiver answered 401; the next send to it carries the typed PIN.
    NeedsPin {
        fingerprint: String,
        alias: String,
        incorrect: bool,
    },
    Sent {
        alias: String,
    },
    Failed {
        message: String,
    },
}

impl SendPhase {
    fn asks_pin_for(&self, device: &str) -> bool {
        matches!(self, Self::NeedsPin { fingerprint, .. } if fingerprint == device)
    }
}

/// The phase and hint a finished send leaves: a PIN request keeps the send
/// open for a retry, anything else ends it.
fn outcome(fingerprint: &str, alias: &str, result: anyhow::Result<()>) -> (SendPhase, String) {
    let error = match result {
        Ok(()) => {
            return (
                SendPhase::Sent {
                    alias: alias.into(),
                },
                format!("Sent to {alias}. They can open the link in a browser."),
            );
        }
        Err(error) => error,
    };
    match error.downcast_ref::<PinRejected>() {
        Some(rejected) => (
            SendPhase::NeedsPin {
                fingerprint: fingerprint.into(),
                alias: alias.into(),
                incorrect: rejected.pin_sent,
            },
            if rejected.pin_sent {
                format!("{rejected} Try again.")
            } else {
                format!("{rejected} Enter it to send the link.")
            },
        ),
        None => (
            SendPhase::Failed {
                message: error.to_string(),
            },
            error.to_string(),
        ),
    }
}

pub struct SendLink {
    focus: FocusHandle,
    runtime: tokio::runtime::Handle,
    url: String,
    sender: Option<Arc<SenderInfo>>,
    discovery: Option<Discovery>,
    devices: Vec<Device>,
    selected: Option<String>,
    phase: SendPhase,
    hint: SharedString,
    cancel: Option<CancellationToken>,
    /// Counts sends, so a cancelled send finishing late cannot overwrite a
    /// newer one.
    sends: u64,
    pin: Entity<InputState>,
    scroll: ScrollHandle,
    qr: Option<Arc<Image>>,
    logo: Arc<Image>,
}

impl SendLink {
    fn new(
        url: String,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Letters and arrows belong to the PIN field while it has focus; Enter
        // and Escape still reach the window (the field passes them on).
        const SHORTCUTS: &str = "OmaBeamSend && !Input";
        cx.bind_keys([
            KeyBinding::new("escape", Cancel, Some("OmaBeamSend")),
            KeyBinding::new("enter", Confirm, Some("OmaBeamSend")),
            KeyBinding::new("c", CopyShot, Some(SHORTCUTS)),
            KeyBinding::new("h", MoveLeft, Some(SHORTCUTS)),
            KeyBinding::new("left", MoveLeft, Some(SHORTCUTS)),
            KeyBinding::new("l", MoveRight, Some(SHORTCUTS)),
            KeyBinding::new("right", MoveRight, Some(SHORTCUTS)),
            KeyBinding::new("k", MoveUp, Some(SHORTCUTS)),
            KeyBinding::new("up", MoveUp, Some(SHORTCUTS)),
            KeyBinding::new("j", MoveDown, Some(SHORTCUTS)),
            KeyBinding::new("down", MoveDown, Some(SHORTCUTS)),
        ]);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let pin = cx.new(|cx| InputState::new(window, cx).placeholder("PIN").masked(true));
        // Typing enables the Send button.
        cx.subscribe(&pin, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        let qr = crate::qr::ShareQr::new(&url)
            .ok()
            .map(|code| Arc::new(Image::from_bytes(ImageFormat::Png, qr_png(&code.rows))));
        let view = Self {
            focus,
            runtime: runtime.clone(),
            url,
            sender: None,
            discovery: None,
            devices: Vec::new(),
            selected: None,
            phase: SendPhase::Starting,
            hint: "Looking for LocalSend on this network…".into(),
            cancel: None,
            sends: 0,
            pin,
            scroll: ScrollHandle::new(),
            qr,
            logo: Arc::new(Image::from_bytes(ImageFormat::Png, LOCALSEND_LOGO.to_vec())),
        };
        view.start(cx);
        view
    }

    fn start(&self, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        let task = runtime.spawn(async move {
            let info = tokio::task::spawn_blocking(SenderInfo::load_or_create)
                .await
                .context("LocalSend identity task stopped")??;
            let discovery = Discovery::start(&info).await?;
            anyhow::Ok((info, discovery))
        });
        cx.spawn(async move |view, cx| {
            let started = task.await;
            let _ = view.update(cx, |this, cx| {
                match started {
                    Ok(Ok((info, discovery))) => {
                        this.sender = Some(Arc::new(info));
                        this.discovery = Some(discovery);
                        this.phase = SendPhase::Ready;
                        this.refresh_devices();
                    }
                    Ok(Err(error)) => {
                        this.phase = SendPhase::Failed {
                            message: format!("Could not look for nearby devices. {error}"),
                        };
                    }
                    Err(_) => {
                        this.phase = SendPhase::Failed {
                            message: "Could not look for nearby devices.".into(),
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();

        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                if view
                    .update(cx, |this, cx| {
                        if this.discovery.is_some() && !this.is_busy() {
                            this.refresh_devices();
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn is_busy(&self) -> bool {
        matches!(self.phase, SendPhase::Sending { .. } | SendPhase::Starting)
    }

    fn refresh_devices(&mut self) {
        let Some(discovery) = &self.discovery else {
            return;
        };
        self.devices = discovery.devices();
        if !matches!(
            self.phase,
            SendPhase::Failed { .. } | SendPhase::Sent { .. } | SendPhase::NeedsPin { .. }
        ) {
            self.hint = discovery.hint().into();
        }
        if self
            .selected
            .as_ref()
            .is_none_or(|id| !self.devices.iter().any(|device| &device.fingerprint == id))
        {
            self.selected = self
                .devices
                .first()
                .map(|device| device.fingerprint.clone());
        }
    }

    fn selected_device(&self) -> Option<&Device> {
        let id = self.selected.as_ref()?;
        self.devices.iter().find(|device| &device.fingerprint == id)
    }

    fn move_selection(&mut self, delta: i32) {
        if self.devices.is_empty() || delta == 0 || self.is_busy() {
            return;
        }
        let index = self
            .selected
            .as_ref()
            .and_then(|id| {
                self.devices
                    .iter()
                    .position(|device| &device.fingerprint == id)
            })
            .unwrap_or(0);
        let next = (index as i32 + delta).rem_euclid(self.devices.len() as i32) as usize;
        self.selected = Some(self.devices[next].fingerprint.clone());
        self.scroll.scroll_to_item(next);
    }

    fn send_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        self.send_to(id, window, cx);
    }

    fn send_to(&mut self, fingerprint: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(sender) = self.sender.clone() else {
            return;
        };
        let Some(device) = self
            .devices
            .iter()
            .find(|device| device.fingerprint == fingerprint)
            .cloned()
        else {
            return;
        };
        let pin = if self.phase.asks_pin_for(&fingerprint) {
            let typed = self.pin.read(cx).value();
            if typed.is_empty() {
                self.pin.update(cx, |pin, cx| pin.focus(window, cx));
                return;
            }
            Some(typed.to_string())
        } else {
            None
        };
        // The PIN field goes away while sending; keep the keys working.
        self.focus.focus(window, cx);
        self.selected = Some(fingerprint.clone());
        self.phase = SendPhase::Sending {
            alias: device.alias.clone(),
        };
        self.hint = format!("Waiting for {} to accept…", device.alias).into();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.sends += 1;
        let send = self.sends;
        let url = self.url.clone();
        let alias = device.alias.clone();
        let runtime = self.runtime.clone();
        let task = runtime.spawn(async move {
            localsend::send_text(&sender, &device, &url, pin.as_deref(), cancel).await
        });
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |this, window, cx| {
                if this.sends != send {
                    return;
                }
                this.cancel = None;
                let (phase, hint) = match result {
                    Ok(result) => outcome(&fingerprint, &alias, result),
                    Err(_) => (
                        SendPhase::Failed {
                            message: "Send stopped.".into(),
                        },
                        "Send stopped.".into(),
                    ),
                };
                if matches!(phase, SendPhase::NeedsPin { .. }) {
                    this.pin.update(cx, |pin, cx| {
                        pin.set_value("", window, cx);
                        pin.focus(window, cx);
                    });
                }
                this.phase = phase;
                this.hint = hint.into();
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_link(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.url.clone()));
        if !matches!(self.phase, SendPhase::Sending { .. }) {
            self.hint = "Share link copied.".into();
            cx.notify();
        }
    }

    fn no_device(&self, _cx: &gpui_kit::App) -> AnyElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .py_4()
            .child(
                img(self.logo.clone())
                    .size(px(56.))
                    .object_fit(ObjectFit::Contain)
                    .flex_shrink_0(),
            )
            .child(
                div()
                    .w_full()
                    .text_center()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(NO_DEVICE_TITLE),
            )
            .into_any_element()
    }

    fn link_column(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let image = self.qr.clone();
        div()
            .id("share-link")
            .w(px(300.))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .p_4()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.border)
            .when_some(image, |column, image| {
                column.child(
                    div()
                        .id("share-qr")
                        .p_2()
                        .rounded(px(8.))
                        .bg(gpui_kit::rgb(0xffffff))
                        .child(
                            img(image)
                                .size(px(200.))
                                .object_fit(ObjectFit::Contain)
                                .flex_shrink_0(),
                        ),
                )
            })
            .child(
                div()
                    .w_full()
                    .text_center()
                    .text_xs()
                    .text_color(theme.secondary)
                    .child(QR_CAPTION),
            )
            .child(
                div()
                    .w_full()
                    .text_center()
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(self.url.clone()),
            )
            .child(
                button("copy-link", "Copy link", ButtonVariant::Secondary, cx)
                    .w_full()
                    .on_click(cx.listener(|this, _, _, cx| this.copy_link(cx))),
            )
            .into_any_element()
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
            self.phase = SendPhase::Ready;
            self.hint = "Send cancelled.".into();
            cx.notify();
            return;
        }
        cx.quit();
    }

    fn device_card(&self, number: usize, device: &Device, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let id = device.fingerprint.clone();
        let selected = self.selected.as_ref() == Some(&id);
        let busy = self.is_busy();
        let edge = if selected { theme.accent } else { theme.border };
        button(
            SharedString::from(format!("device-{id}")),
            "",
            ButtonVariant::Secondary,
            cx,
        )
        .selected(selected)
        .disabled(busy)
        .w_full()
        .h(px(58.))
        .px_3()
        .justify_start()
        .items_center()
        .gap_3()
        .border_color(edge)
        .hover(|style| style.bg(theme.hover_fill()).border_color(edge))
        .accessibility_label(format!("{number}. Send to {}", device.alias))
        .child(
            div()
                .size(px(32.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .bg(theme.accent.opacity(0.12))
                .text_color(theme.accent)
                .text_sm()
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(number.to_string()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_0()
                .child(
                    div()
                        .text_ellipsis()
                        .overflow_hidden()
                        .child(device.alias.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.secondary)
                        .text_ellipsis()
                        .overflow_hidden()
                        .child(if device.model.is_empty() {
                            "LocalSend".to_string()
                        } else {
                            device.model.clone()
                        }),
                ),
        )
        .when(selected, |card| {
            card.child(icon(IconName::Check).text_color(theme.accent))
        })
        .on_click(cx.listener(move |this, _, window, cx| this.send_to(id.clone(), window, cx)))
        .into_any_element()
    }
}

impl Focusable for SendLink {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SendLink {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = matches!(
            self.phase,
            SendPhase::Ready
                | SendPhase::NeedsPin { .. }
                | SendPhase::Sent { .. }
                | SendPhase::Failed { .. }
        );
        let sending = matches!(self.phase, SendPhase::Sending { .. });
        let pin_for_selected = self
            .selected
            .as_deref()
            .is_some_and(|id| self.phase.asks_pin_for(id));
        let pin_missing = pin_for_selected && self.pin.read(cx).value().is_empty();
        let status_color = match self.phase {
            SendPhase::Failed { .. }
            | SendPhase::NeedsPin {
                incorrect: true, ..
            } => cx.omarchy().danger,
            SendPhase::NeedsPin { .. } => cx.omarchy().warning,
            SendPhase::Sent { .. } => cx.omarchy().success,
            _ => cx.omarchy().secondary,
        };
        let pin_field = matches!(self.phase, SendPhase::NeedsPin { .. })
            .then(|| text_input("send-pin", &self.pin, window, cx));
        let no_devices = self.devices.is_empty();
        let mut cards = Vec::new();
        for (index, device) in self.devices.clone().into_iter().enumerate() {
            cards.push(self.device_card(index + 1, &device, cx));
        }
        focus_scope("omabeam-send")
            .size_full()
            .bg(cx.omarchy().background)
            .text_color(cx.omarchy().foreground)
            .child(
                div()
                    .id("send-link-root")
                    .key_context("OmaBeamSend")
                    .track_focus(&self.focus)
                    .size_full()
                    .flex()
                    .flex_col()
                    .on_action(
                        cx.listener(|this, _: &Confirm, window, cx| this.send_selected(window, cx)),
                    )
                    .on_action(cx.listener(|this, _: &Cancel, _, cx| this.cancel(cx)))
                    .on_action(cx.listener(|this, _: &CopyShot, _, cx| this.copy_link(cx)))
                    .on_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                        this.move_selection(-1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                        this.move_selection(1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        this.move_selection(-1);
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        this.move_selection(1);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(cx.omarchy().border)
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(brand::header("Send the share link", cx))
                            .child(
                                div().text_xs().text_color(cx.omarchy().secondary).child(
                                    "Scan or copy on the left. Send to a numbered computer on the right.",
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .child(self.link_column(cx))
                            .child(
                                div()
                                    .id("nearby-devices")
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .p_4()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .child("LocalSend"),
                                    )
                                    .child(
                                        div().text_xs().text_color(cx.omarchy().secondary).child(
                                            "Choose a numbered computer. It will be asked to accept the link.",
                                        ),
                                    )
                                    .when(no_devices, |list| list.child(self.no_device(cx)))
                                    .children(cards),
                            ),
                    )
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .border_t_1()
                            .border_color(cx.omarchy().border)
                            .bg(cx.omarchy().surface)
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(status_color)
                                    .child(self.hint.clone()),
                            )
                            .children(pin_field)
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .justify_end()
                                    .gap_2()
                                    .when(!no_devices, |row| {
                                        row.child(
                                            button(
                                                "send-selected",
                                                if sending {
                                                    "Waiting…"
                                                } else if pin_for_selected {
                                                    "Send with PIN"
                                                } else {
                                                    "Send"
                                                },
                                                ButtonVariant::Primary,
                                                cx,
                                            )
                                            .bg(cx.omarchy().accent)
                                            .text_color(cx.omarchy().background)
                                            .disabled(
                                                !ready
                                                    || pin_missing
                                                    || self.selected_device().is_none(),
                                            )
                                            .on_click(
                                                cx.listener(|this, _, window, cx| {
                                                    this.send_selected(window, cx)
                                                }),
                                            ),
                                        )
                                    })
                                    .child(
                                        button("done", "Done", ButtonVariant::Outline, cx)
                                            .on_click(cx.listener(|_this, _, _, cx| cx.quit())),
                                    ),
                            )
                            .child(
                                div().text_xs().text_color(cx.omarchy().secondary).child(
                                    "↑↓ choose    ↵ send    c copy    esc close",
                                ),
                            ),
                    ),
            )
    }
}

/// Black-on-white PNG. The window's SVG painter is a one-color mask, so an
/// SVG QR renders as an empty white square.
fn qr_png(rows: &[String]) -> Vec<u8> {
    let (side, pixels) = qr_pixels(rows);
    encode_gray_png(side, &pixels)
}

fn qr_pixels(rows: &[String]) -> (u32, Vec<u8>) {
    let quiet = 4usize;
    let modules = rows.len() + quiet * 2;
    let scale = (240 / modules).clamp(2, 8);
    let side = modules * scale;
    let mut pixels = vec![0xFFu8; side * side];
    for (y, row) in rows.iter().enumerate() {
        for (x, bit) in row.bytes().enumerate() {
            if bit != b'1' {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let px = (x + quiet) * scale + dx;
                    let py = (y + quiet) * scale + dy;
                    pixels[py * side + px] = 0;
                }
            }
        }
    }
    (side as u32, pixels)
}

fn encode_gray_png(side: u32, pixels: &[u8]) -> Vec<u8> {
    let width = side as usize;
    let mut raw = Vec::with_capacity((width + 1) * width);
    for row in pixels.chunks(width) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend(png_chunk(b"IHDR", &ihdr));
    png.extend(png_chunk(b"IDAT", &zlib_store(&raw)));
    png.extend(png_chunk(b"IEND", &[]));
    png
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    out
}

fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut rest = data;
    while !rest.is_empty() {
        let n = rest.len().min(65535);
        let final_block = n == rest.len();
        out.push(if final_block { 0x01 } else { 0x00 });
        let len = n as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(len ^ 0xFFFF).to_le_bytes());
        out.extend_from_slice(&rest[..n]);
        rest = &rest[n..];
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

pub fn open(url: String) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("omabeam-localsend")
        .build()
        .expect("LocalSend runtime");
    let handle = runtime.handle().clone();
    gpui_kit::application().run(move |cx| {
        gpui_omarchy::init(cx);
        let bounds = WindowBounds::centered(size(px(WINDOW_W), px(WINDOW_H)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(bounds),
                titlebar: Some(gpui_kit::TitlebarOptions {
                    title: Some("OmaBeam".into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                kind: WindowKind::Normal,
                is_resizable: true,
                app_id: Some(APP_ID.into()),
                window_min_size: Some(size(px(640.), px(420.))),
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| SendLink::new(url, handle, window, cx)),
        )
        .expect("open OmaBeam send window");
        cx.activate(true);
    });
    drop(runtime);
}

#[cfg(test)]
mod tests {
    use super::{APP_ID, LOCALSEND_LOGO, NO_DEVICE_TITLE, SendPhase, outcome, qr_png};
    use crate::localsend::PinRejected;

    #[test]
    fn an_empty_network_explains_localsend_and_offers_the_link() {
        assert!(LOCALSEND_LOGO.starts_with(b"\x89PNG"));
        assert_eq!(NO_DEVICE_TITLE, "Open LocalSend on the client device");
    }

    #[test]
    fn qr_png_scans_back_to_the_share_url() {
        let url = "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/";
        let rows = crate::qr::ShareQr::new(url).unwrap().rows;
        let (side, pixels) = decode_gray_png(&qr_png(&rows));
        let mut decoder = quircs::Quirc::default();
        let codes: Vec<_> = decoder.identify(side, side, &pixels).collect();
        assert_eq!(codes.len(), 1);
        assert_eq!(
            codes[0].as_ref().unwrap().decode().unwrap().payload,
            url.as_bytes()
        );
    }

    fn decode_gray_png(png: &[u8]) -> (usize, Vec<u8>) {
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let mut offset = 8;
        let mut width = 0usize;
        let mut height = 0usize;
        let mut raw = Vec::new();
        while offset + 8 <= png.len() {
            let len = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &png[offset + 4..offset + 8];
            let data = &png[offset + 8..offset + 8 + len];
            offset += 12 + len;
            match kind {
                b"IHDR" => {
                    width = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
                    height = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
                    assert_eq!(&data[8..13], &[8, 0, 0, 0, 0]);
                }
                b"IDAT" => raw.extend_from_slice(&inflate_stored(&data[2..data.len() - 4])),
                b"IEND" => break,
                _ => {}
            }
        }
        assert_eq!(width, height);
        assert_eq!(raw.len(), height * (width + 1));
        let mut pixels = Vec::with_capacity(width * height);
        for row in raw.chunks(width + 1) {
            assert_eq!(row[0], 0);
            pixels.extend_from_slice(&row[1..]);
        }
        (width, pixels)
    }

    fn inflate_stored(mut data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let header = data[0];
            let len = u16::from_le_bytes(data[1..3].try_into().unwrap()) as usize;
            let nlen = u16::from_le_bytes(data[3..5].try_into().unwrap()) as usize;
            assert_eq!(nlen, len ^ 0xFFFF);
            out.extend_from_slice(&data[5..5 + len]);
            data = &data[5 + len..];
            if header & 1 == 1 {
                break;
            }
        }
        assert!(data.is_empty());
        out
    }

    fn rejected(pin_sent: bool) -> anyhow::Result<()> {
        Err(PinRejected {
            alias: "Kitchen".into(),
            pin_sent,
        }
        .into())
    }

    #[test]
    fn a_pin_request_asks_for_the_pin_instead_of_failing() {
        let (phase, hint) = outcome("kitchen-id", "Kitchen", rejected(false));
        assert_eq!(
            phase,
            SendPhase::NeedsPin {
                fingerprint: "kitchen-id".into(),
                alias: "Kitchen".into(),
                incorrect: false,
            }
        );
        assert_eq!(hint, "Kitchen requires a PIN. Enter it to send the link.");
        assert!(phase.asks_pin_for("kitchen-id"));
        assert!(!phase.asks_pin_for("other-id"));

        let (phase, hint) = outcome("kitchen-id", "Kitchen", rejected(true));
        assert_eq!(
            phase,
            SendPhase::NeedsPin {
                fingerprint: "kitchen-id".into(),
                alias: "Kitchen".into(),
                incorrect: true,
            }
        );
        assert_eq!(hint, "Incorrect PIN for Kitchen. Try again.");
    }

    #[test]
    fn other_results_end_the_send() {
        let (phase, hint) = outcome("kitchen-id", "Kitchen", Ok(()));
        assert_eq!(
            phase,
            SendPhase::Sent {
                alias: "Kitchen".into()
            }
        );
        assert_eq!(
            hint,
            "Sent to Kitchen. They can open the link in a browser."
        );
        assert!(!phase.asks_pin_for("kitchen-id"));

        let (phase, hint) = outcome(
            "kitchen-id",
            "Kitchen",
            Err(anyhow::anyhow!("Kitchen declined the transfer.")),
        );
        assert_eq!(
            phase,
            SendPhase::Failed {
                message: "Kitchen declined the transfer.".into()
            }
        );
        assert_eq!(hint, "Kitchen declined the transfer.");
    }

    #[test]
    fn the_installer_floats_the_send_window_apart_from_the_picker() {
        // The picker is the window whose class is "omabeam".
        assert_ne!(APP_ID, "omabeam");
        let rule = format!("o.window({APP_ID:?}, {{");
        assert!(include_str!("../../install.sh").contains(&rule), "{rule}");
    }
}
