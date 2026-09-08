use super::*;
use crate::localsend::{self, Device, Discovery, SenderInfo, ShareUrl};
use anyhow::Context as _;
use gpui_kit::AnyElement;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const WINDOW_W: f32 = 440.0;
const WINDOW_H: f32 = 560.0;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SendPhase {
    Starting,
    Ready,
    Sending { alias: String },
    Sent { alias: String },
    Failed { message: String },
}

pub struct SendLink {
    focus: FocusHandle,
    runtime: tokio::runtime::Handle,
    url: String,
    host: String,
    sender: Option<Arc<SenderInfo>>,
    discovery: Option<Discovery>,
    devices: Vec<Device>,
    selected: Option<String>,
    phase: SendPhase,
    hint: SharedString,
    cancel: Option<CancellationToken>,
    scroll: ScrollHandle,
}

impl SendLink {
    fn new(
        url: String,
        host: String,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.bind_keys([
            KeyBinding::new("escape", Cancel, Some("OmaBeamSend")),
            KeyBinding::new("enter", Confirm, Some("OmaBeamSend")),
            KeyBinding::new("c", CopyShot, Some("OmaBeamSend")),
            KeyBinding::new("h", MoveLeft, Some("OmaBeamSend")),
            KeyBinding::new("left", MoveLeft, Some("OmaBeamSend")),
            KeyBinding::new("l", MoveRight, Some("OmaBeamSend")),
            KeyBinding::new("right", MoveRight, Some("OmaBeamSend")),
            KeyBinding::new("k", MoveUp, Some("OmaBeamSend")),
            KeyBinding::new("up", MoveUp, Some("OmaBeamSend")),
            KeyBinding::new("j", MoveDown, Some("OmaBeamSend")),
            KeyBinding::new("down", MoveDown, Some("OmaBeamSend")),
        ]);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let view = Self {
            focus,
            runtime: runtime.clone(),
            url,
            host,
            sender: None,
            discovery: None,
            devices: Vec::new(),
            selected: None,
            phase: SendPhase::Starting,
            hint: "Starting LocalSend discovery…".into(),
            cancel: None,
            scroll: ScrollHandle::new(),
        };
        view.start(cx);
        view
    }

    fn start(&self, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        let task = runtime.spawn(async move {
            let info = tokio::task::spawn_blocking(SenderInfo::generate)
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
            SendPhase::Failed { .. } | SendPhase::Sent { .. }
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

    fn send_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        self.send_to(id, cx);
    }

    fn send_to(&mut self, fingerprint: String, cx: &mut Context<Self>) {
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
        self.selected = Some(fingerprint);
        self.phase = SendPhase::Sending {
            alias: device.alias.clone(),
        };
        self.hint = format!("Waiting for {} to accept…", device.alias).into();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let url = self.url.clone();
        let alias = device.alias.clone();
        let runtime = self.runtime.clone();
        let task = runtime
            .spawn(async move { localsend::send_text(&sender, &device, &url, cancel).await });
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.cancel = None;
                match result {
                    Ok(Ok(())) => {
                        this.phase = SendPhase::Sent {
                            alias: alias.clone(),
                        };
                        this.hint =
                            format!("Sent to {alias}. They can open the link in a browser.").into();
                    }
                    Ok(Err(error)) => {
                        this.phase = SendPhase::Failed {
                            message: error.to_string(),
                        };
                        this.hint = error.to_string().into();
                    }
                    Err(_) => {
                        this.phase = SendPhase::Failed {
                            message: "Send stopped.".into(),
                        };
                        this.hint = "Send stopped.".into();
                    }
                }
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

    fn device_card(&self, device: &Device, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.omarchy().clone();
        let id = device.fingerprint.clone();
        let selected = self.selected.as_ref() == Some(&id);
        let busy = self.is_busy();
        let initials = device_initials(&device.alias);
        let hash = initials.bytes().fold(0x811c9dc5u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x01000193)
        });
        let colors = [theme.accent, theme.success, theme.warning, theme.danger];
        let avatar_color = colors[hash as usize % colors.len()];
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
        .accessibility_label(format!("Send to {}", device.alias))
        .child(
            div()
                .size(px(32.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(avatar_color.opacity(0.12))
                .text_color(avatar_color)
                .text_xs()
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(initials),
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
        .on_click(cx.listener(move |this, _, _, cx| this.send_to(id.clone(), cx)))
        .into_any_element()
    }
}

impl Focusable for SendLink {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SendLink {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = matches!(
            self.phase,
            SendPhase::Ready | SendPhase::Sent { .. } | SendPhase::Failed { .. }
        );
        let sending = matches!(self.phase, SendPhase::Sending { .. });
        let status_color = if matches!(self.phase, SendPhase::Failed { .. }) {
            cx.omarchy().danger
        } else if matches!(self.phase, SendPhase::Sent { .. }) {
            cx.omarchy().success
        } else {
            cx.omarchy().secondary
        };
        let mut cards = Vec::new();
        for device in self.devices.clone() {
            cards.push(self.device_card(&device, cx));
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
                    .on_action(cx.listener(|this, _: &Confirm, _, cx| this.send_selected(cx)))
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
                            .child(brand::header("Send share link", cx))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.omarchy().secondary)
                                    .child(format!("Nearby devices can open {}", self.host)),
                            ),
                    )
                    .child(
                        div()
                            .id("nearby-devices")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .when(cards.is_empty(), |list| {
                                list.child(empty_state(
                                    "No devices yet",
                                    "OmaBeam is scanning this network. The other computer needs OmaSend or LocalSend open to receive. You can still copy the link.",
                                    cx,
                                ))
                            })
                            .children(cards),
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
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        button("copy-link", "Copy link", ButtonVariant::Secondary, cx)
                                            .on_click(cx.listener(|this, _, _, cx| this.copy_link(cx))),
                                    )
                                    .child(
                                        button(
                                            "send-selected",
                                            if sending { "Waiting…" } else { "Send" },
                                            ButtonVariant::Primary,
                                            cx,
                                        )
                                        .bg(cx.omarchy().accent)
                                        .text_color(cx.omarchy().background)
                                        .disabled(!ready || self.selected_device().is_none())
                                        .on_click(cx.listener(|this, _, _, cx| this.send_selected(cx))),
                                    )
                                    .child(
                                        button("done", "Done", ButtonVariant::Outline, cx)
                                            .on_click(cx.listener(|_this, _, _, cx| cx.quit())),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.omarchy().secondary)
                                    .child("↑↓ choose    ↵ send    c copy    esc close"),
                            ),
                    ),
            )
    }
}

fn device_initials(alias: &str) -> String {
    let letters: String = alias
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if letters.is_empty() {
        "TS".into()
    } else {
        letters
    }
}

pub fn open(url: String) {
    let host = localsend::parse_share_url(&url)
        .map(|share: ShareUrl| share.display_host())
        .unwrap_or_else(|| url.clone());
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
                app_id: Some("omabeam".into()),
                window_min_size: Some(size(px(360.), px(400.))),
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| SendLink::new(url, host, handle, window, cx)),
        )
        .expect("open OmaBeam send window");
        cx.activate(true);
    });
    drop(runtime);
}

#[cfg(test)]
mod tests {
    use super::device_initials;

    #[test]
    fn initials_use_the_visible_name() {
        assert_eq!(device_initials("Kitchen PC"), "KP");
        assert_eq!(device_initials("pixel"), "P");
        assert_eq!(device_initials("   "), "TS");
    }
}
