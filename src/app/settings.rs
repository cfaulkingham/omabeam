use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamPreset {
    Balanced,
    Text,
    Motion,
}

impl StreamPreset {
    pub const ALL: [Self; 3] = [Self::Balanced, Self::Text, Self::Motion];
    pub fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced",
            Self::Text => "Crisp text",
            Self::Motion => "Smooth motion",
        }
    }
    pub fn values(self) -> (u32, u8, Option<u32>) {
        match self {
            Self::Balanced => (15, 55, None),
            Self::Text => (15, 90, None),
            Self::Motion => (60, 55, Some(1280)),
        }
    }
    pub fn apply(self, config: &mut LiveConfig) {
        (config.fps, config.quality, config.max_width) = self.values();
    }
    pub fn matching(config: &LiveConfig) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.values() == (config.fps, config.quality, config.max_width))
    }
}

impl OmaBeam {
    pub(super) fn render_stream_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let preset = StreamPreset::matching(&self.live_config);
        let preset_changed = cx.listener(|this, index: &usize, _, cx| {
            StreamPreset::ALL[*index].apply(&mut this.live_config);
            cx.notify();
        });
        let quality = menu(
            "quality-menu",
            dropdown(
                "quality",
                preset.map_or("Custom", StreamPreset::label),
                self.busy,
                cx,
            ),
            StreamPreset::ALL
                .iter()
                .map(|p| {
                    MenuItem::new(p.label())
                        .checked(Some(*p) == preset)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| preset_changed(&index, window, cx),
        );
        let cursor_changed = cx.listener(|this, value: &bool, _, cx| {
            if !this.busy {
                this.live_config.cursor = *value;
                cx.notify();
            }
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.omarchy().secondary)
                            .child("Quality"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_start()
                            .gap_3()
                            .child(div().w(px(160.)).child(quality))
                            .child(
                                switch(
                                    "stream-cursor",
                                    "Show cursor",
                                    self.live_config.cursor,
                                    cx,
                                )
                                .on_change(move |value, _, window, cx| {
                                    cursor_changed(&value, window, cx)
                                }),
                            )
                            .child(
                                button(
                                    "advanced",
                                    if self.show_advanced {
                                        "▾ Advanced"
                                    } else {
                                        "▸ Advanced"
                                    },
                                    ButtonVariant::Outline,
                                    cx,
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_advanced = !this.show_advanced;
                                    cx.notify();
                                })),
                            ),
                    ),
            )
            .when(self.show_advanced, |root| {
                root.child(self.render_advanced(cx))
            })
    }

    fn render_advanced(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fps_values = [15, 30, 60];
        let fps = self.live_config.fps;
        let changed = cx.listener(move |this, index: &usize, _, cx| {
            this.live_config.fps = fps_values[*index];
            cx.notify();
        });
        let fps_menu = menu(
            "fps-menu",
            dropdown("fps", format!("{fps} fps"), self.busy, cx),
            fps_values
                .iter()
                .map(|v| {
                    MenuItem::new(format!("{v} fps"))
                        .checked(*v == fps)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        let values = [55, 72, 90];
        let quality = self.live_config.quality;
        let changed = cx.listener(move |this, index: &usize, _, cx| {
            this.live_config.quality = values[*index];
            cx.notify();
        });
        let jpeg_menu = menu(
            "jpeg-menu",
            dropdown("jpeg", quality.to_string(), self.busy, cx),
            values
                .iter()
                .map(|v| {
                    MenuItem::new(v.to_string())
                        .checked(*v == quality)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        let widths = [None, Some(1280), Some(1920)];
        let width = self.live_config.max_width;
        let changed = cx.listener(move |this, index: &usize, _, cx| {
            this.live_config.max_width = widths[*index];
            cx.notify();
        });
        let width_menu = menu(
            "width-menu",
            dropdown("width", width_label(width), self.busy, cx),
            widths
                .iter()
                .map(|v| {
                    MenuItem::new(width_label(*v))
                        .checked(*v == width)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        div().flex().flex_col().gap_2().p_3().bg(cx.omarchy().inset)
            .child(div().flex().flex_wrap().gap_3()
                .child(field("Frame rate", fps_menu, cx))
                .child(field("JPEG quality", jpeg_menu, cx))
                .child(field("Maximum width", width_menu, cx)))
            .child(div().text_xs().text_color(cx.omarchy().secondary).child(if self.live_config.bind.is_loopback() {
                "Only available on this computer unless you forward the connection."
            } else { "Anyone who can reach this computer and has the link can view. HTTP is unencrypted." }))
    }
}

pub(super) fn dropdown(
    id: &'static str,
    label: impl Into<SharedString>,
    disabled: bool,
    cx: &gpui_kit::App,
) -> gpui_omarchy::Button {
    button(id, label, ButtonVariant::Outline, cx)
        .disabled(disabled)
        .w_full()
        .justify_between()
        .bg(cx.omarchy().normal_fill())
        .child(icon(IconName::ChevronDown).size(px(14.)))
}

pub(super) fn field(
    label: &str,
    control: impl IntoElement,
    cx: &gpui_kit::App,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w(px(180.))
        .flex_none()
        .child(
            div()
                .text_xs()
                .text_color(cx.omarchy().secondary)
                .child(label.to_string()),
        )
        .child(control)
}

fn width_label(width: Option<u32>) -> String {
    width.map_or("Native".into(), |v| format!("{v} px"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_preserve_network_cursor_and_custom_cli_values() {
        let mut config = LiveConfig {
            port: 4321,
            cursor: true,
            bind: "127.0.0.1".parse().unwrap(),
            ..LiveConfig::default()
        };
        assert_eq!(
            StreamPreset::matching(&config),
            Some(StreamPreset::Balanced)
        );
        for preset in StreamPreset::ALL {
            preset.apply(&mut config);
            config.validate().unwrap();
            assert_eq!(StreamPreset::matching(&config), Some(preset));
            assert_eq!(config.port, 4321);
            assert!(config.cursor && config.bind.is_loopback());
        }
        config.fps = 24;
        assert_eq!(StreamPreset::matching(&config), None);
        assert_eq!(config.fps, 24);
    }
}
