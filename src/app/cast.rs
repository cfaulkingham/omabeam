use super::*;
use settings::field;

impl OmaBeam {
    fn refresh_cast_receivers(&mut self, cx: &mut Context<Self>) {
        if self.cast_scanning || self.busy {
            return;
        }
        self.cast_scanning = true;
        self.cast_error = None;
        let operation = cx
            .background_executor()
            .spawn(async { crate::live::cast::discover(Duration::from_secs(5)) });
        cx.spawn(async move |view, cx| {
            let result = operation.await;
            let _ = view.update(cx, |this, cx| {
                this.cast_scanning = false;
                match result {
                    Ok(receivers) => {
                        if receivers.is_empty() {
                            this.cast_error = Some("No video receivers found. Check the same LAN, multicast UDP 5353, and Wi-Fi client isolation, then refresh.".into());
                        }
                        this.cast_receivers = receivers;
                        if this
                            .cast_receiver_id
                            .as_ref()
                            .is_some_and(|id| !this.cast_receivers.iter().any(|r| &r.id == id))
                        {
                            this.cast_receiver_id = None;
                        }
                    }
                    Err(error) => {
                        this.cast_error = Some(format!("Could not find Cast receivers: {error:#}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_destination(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let available = crate::live::cast::available();
        let changed = cx.listener(|this, index: &usize, _, cx| {
            let cast = *index == 1;
            if cast == this.cast_mode || this.busy {
                return;
            }
            this.cast_mode = cast;
            if cast {
                this.browser_config = Some((
                    this.live_config.clone(),
                    this.desktop_config.clone(),
                    this.fps_selected,
                ));
                this.live_config.set_fps(30);
                this.live_config.max_width = Some(1280);
                this.desktop_config.width = 1280;
                this.desktop_config.height = 720;
                this.refresh_cast_receivers(cx);
            } else if let Some((config, desktop, fps_selected)) = this.browser_config.take() {
                this.live_config = config;
                this.desktop_config = desktop;
                this.fps_selected = fps_selected;
            }
            cx.notify();
        });
        let destination = menu(
            "destination-menu",
            dropdown(
                "destination",
                if self.cast_mode {
                    "Google Cast"
                } else {
                    "Browser link"
                },
                self.busy,
                cx,
            ),
            vec![
                MenuItem::new("Browser link")
                    .checked(!self.cast_mode)
                    .disabled(self.busy),
                MenuItem::new(if available {
                    "Google Cast"
                } else {
                    "Google Cast unavailable in this build"
                })
                .checked(self.cast_mode)
                .disabled(self.busy || !available),
            ],
            move |index, window, cx| changed(&index, window, cx),
        );
        let ids: Vec<String> = self.cast_receivers.iter().map(|r| r.id.clone()).collect();
        let selected = cx.listener(move |this, index: &usize, _, cx| {
            if !this.busy
                && let Some(id) = ids.get(*index)
            {
                this.cast_receiver_id = Some(id.clone());
                cx.notify();
            }
        });
        let selected_name = self
            .cast_receiver_id
            .as_ref()
            .and_then(|id| self.cast_receivers.iter().find(|r| &r.id == id))
            .map(|r| r.name.as_str());
        let receiver = menu(
            "cast-receiver-menu",
            dropdown(
                "cast-receiver",
                selected_name.unwrap_or(if self.cast_scanning {
                    "Looking for receivers…"
                } else {
                    "Choose a receiver"
                }),
                self.busy || self.cast_receivers.is_empty(),
                cx,
            ),
            self.cast_receivers
                .iter()
                .map(|r| {
                    let suffix =
                        r.id.chars()
                            .rev()
                            .take(6)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>();
                    MenuItem::new(format!(
                        "{} · {} · {}{}",
                        r.name,
                        r.model,
                        suffix,
                        if r.busy { " · in use" } else { "" }
                    ))
                    .checked(self.cast_receiver_id.as_ref() == Some(&r.id))
                    .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| selected(&index, window, cx),
        );
        div().flex().flex_col().gap_2()
            .child(div().flex().flex_wrap().items_end().gap_3()
                .child(field("Destination", destination, cx))
                .when(self.cast_mode, |row| row.child(field("Receiver",receiver,cx))
                    .child(button("cast-refresh",if self.cast_scanning {"Searching…"} else {"Refresh"},ButtonVariant::Secondary,cx)
                        .disabled(self.busy || self.cast_scanning)
                        .on_click(cx.listener(|this,_,_,cx|this.refresh_cast_receivers(cx))))))
            .when(self.cast_mode, |column| column.child(div().text_xs().text_color(cx.omarchy().secondary)
                .child(self.cast_error.clone().unwrap_or_else(||
                    "Choose a receiver on the same local network. Starting Cast may replace what is playing there.".into()))))
    }

    pub(super) fn render_cast_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let full_hd = self.live_config.max_width.is_some_and(|w| w >= 1920);
        let changed = cx.listener(|this, index: &usize, _, cx| {
            let (w, h) = if *index == 1 {
                (1920, 1080)
            } else {
                (1280, 720)
            };
            this.live_config.max_width = Some(w);
            this.desktop_config.width = w;
            this.desktop_config.height = h;
            cx.notify();
        });
        let profile = menu(
            "cast-profile-menu",
            dropdown(
                "cast-profile",
                if full_hd { "1080p" } else { "720p" },
                self.busy,
                cx,
            ),
            ["720p", "1080p"]
                .into_iter()
                .enumerate()
                .map(|(i, label)| {
                    MenuItem::new(label)
                        .checked(full_hd == (i == 1))
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        let changed = cx.listener(|this, index: &usize, _, cx| {
            this.live_config.set_fps(if *index == 1 { 30 } else { 15 });
            this.fps_selected = true;
            cx.notify();
        });
        let fps = menu(
            "cast-fps-menu",
            dropdown(
                "cast-fps",
                format!("{} fps", self.live_config.fps),
                self.busy,
                cx,
            ),
            [15, 30]
                .into_iter()
                .map(|rate| {
                    MenuItem::new(format!("{rate} fps"))
                        .checked(self.live_config.fps == rate)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        let changed = cx.listener(|this, index: &usize, _, cx| {
            this.live_config.encoder = crate::live::EncoderMode::ALL[*index];
            cx.notify();
        });
        let encoder = menu(
            "cast-encoder-menu",
            dropdown(
                "cast-encoder",
                self.live_config.encoder.label(),
                self.busy,
                cx,
            ),
            crate::live::EncoderMode::ALL
                .into_iter()
                .map(|mode| {
                    MenuItem::new(mode.label())
                        .checked(self.live_config.encoder == mode)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        let cursor_changed = cx.listener(|this, value: &bool, _, cx| {
            this.live_config.cursor = *value;
            cx.notify();
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .child(field("Quality", profile, cx))
                    .child(field("Frame rate", fps, cx))
                    .child(field("Encoder", encoder, cx))
                    .child(
                        switch("cast-cursor", "Show cursor", self.live_config.cursor, cx)
                            .on_change(move |value, _, window, cx| {
                                cursor_changed(&value, window, cx)
                            }),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.omarchy().secondary)
                    .child("Video only. Device compatibility is under qualification."),
            )
    }
}
