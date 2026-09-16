use super::*;
use crate::live::EncoderMode;
use omabeam_capture::PixelMode;

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
    pub fn values(self) -> (u32, u8, Option<u32>, PixelMode) {
        match self {
            Self::Balanced => (15, 55, None, PixelMode::Logical),
            Self::Text => (15, 90, None, PixelMode::Native),
            Self::Motion => (60, 55, Some(1280), PixelMode::Logical),
        }
    }
    pub fn apply(self, config: &mut LiveConfig) {
        let (fps, quality, max_width, pixel_mode) = self.values();
        config.set_fps(fps);
        config.quality = quality;
        config.max_width = max_width;
        config.pixel_mode = pixel_mode;
    }
    pub fn matching(config: &LiveConfig) -> Option<Self> {
        Self::ALL.into_iter().find(|p| {
            p.values()
                == (
                    config.fps,
                    config.quality,
                    config.max_width,
                    config.pixel_mode,
                )
        })
    }
}

impl OmaBeam {
    pub(super) fn render_stream_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let preset = StreamPreset::matching(&self.live_config);
        let preset_changed = cx.listener(|this, index: &usize, _, cx| {
            StreamPreset::ALL[*index].apply(&mut this.live_config);
            this.fps_selected = true;
            cx.notify();
        });
        let preset_menu = menu(
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
        let webrtc = self.live_config.webrtc;
        let transport_changed = cx.listener(|this, index: &usize, _, cx| {
            this.live_config.webrtc = *index == 0;
            cx.notify();
        });
        let transport_menu = menu(
            "transport-menu",
            dropdown(
                "transport",
                if webrtc { "H.264 / WebRTC" } else { "JPEG" },
                self.busy,
                cx,
            ),
            ["H.264 / WebRTC", "JPEG"]
                .iter()
                .enumerate()
                .map(|(i, label)| {
                    MenuItem::new(*label)
                        .checked(webrtc == (i == 0))
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| transport_changed(&index, window, cx),
        );
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child("Stream settings"),
                    )
                    .child(
                        button("advanced", "Advanced", ButtonVariant::Secondary, cx)
                            .px(px(4.))
                            .gap(px(4.))
                            .text_color(cx.omarchy().secondary)
                            .child(
                                icon(if self.show_advanced {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(px(18.)),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_advanced = !this.show_advanced;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .px_3()
                    .child(field("Preset", preset_menu, cx))
                    .child(field("Video transport", transport_menu, cx))
                    .child(field(
                        "Cursor",
                        div().h(px(28.)).flex().items_center().child(
                            switch("stream-cursor", "Show cursor", self.live_config.cursor, cx)
                                .on_change(move |value, _, window, cx| {
                                    cursor_changed(&value, window, cx)
                                }),
                        ),
                        cx,
                    )),
            )
            .when(self.show_advanced, |root| {
                root.child(self.render_advanced(cx))
            })
    }

    fn render_advanced(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fps_values = [15, 30, 60];
        let fps = self.live_config.fps;
        let changed = cx.listener(move |this, index: &usize, _, cx| {
            this.live_config.set_fps(fps_values[*index]);
            this.fps_selected = true;
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
        let modes = [PixelMode::Logical, PixelMode::Native];
        let pixel_mode = self.live_config.pixel_mode;
        let changed = cx.listener(move |this, index: &usize, _, cx| {
            this.live_config.pixel_mode = modes[*index];
            cx.notify();
        });
        let pixel_menu = menu(
            "pixels-menu",
            dropdown("pixels", pixel_label(pixel_mode), self.busy, cx),
            modes
                .iter()
                .map(|mode| {
                    MenuItem::new(pixel_label(*mode))
                        .checked(*mode == pixel_mode)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        let bitrates = [2_000_000, 4_000_000, 8_000_000, 16_000_000];
        let bitrate = self.live_config.h264_bitrate;
        let changed = cx.listener(move |this, index: &usize, _, cx| {
            this.live_config.h264_bitrate = bitrates[*index];
            cx.notify();
        });
        let bitrate_menu = menu(
            "bitrate-menu",
            dropdown(
                "bitrate",
                format!("{} Mbit/s", bitrate as f64 / 1_000_000.0),
                self.busy,
                cx,
            ),
            bitrates
                .iter()
                .map(|v| {
                    MenuItem::new(format!("{} Mbit/s", v / 1_000_000))
                        .checked(*v == bitrate)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        let changed = cx.listener(|this, index: &usize, _, cx| {
            this.live_config.encoder = EncoderMode::ALL[*index];
            cx.notify();
        });
        let encoder_menu = menu(
            "encoder-menu",
            dropdown("encoder", self.live_config.encoder.label(), self.busy, cx),
            EncoderMode::ALL
                .iter()
                .map(|mode| {
                    MenuItem::new(mode.label())
                        .checked(*mode == self.live_config.encoder)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| changed(&index, window, cx),
        );
        div().flex().flex_col().gap_2().p_3().bg(cx.omarchy().inset)
            .border_1()
            .border_color(cx.omarchy().divider())
            .child(div().flex().flex_wrap().gap_3()
                .child(field("Frame rate", fps_menu, cx))
                .child(field("JPEG quality", jpeg_menu, cx))
                .child(field("Maximum width", width_menu, cx))
                .child(field("Pixel detail", pixel_menu, cx))
                .when(self.live_config.webrtc, |row| row.child(field("H.264 target bitrate", bitrate_menu, cx))
                    .child(field("Encoder", encoder_menu, cx))))
            .when(self.live_config.webrtc, |root| root.child(div().text_xs().text_color(cx.omarchy().secondary)
                .child("Auto uses a working hardware encoder when available and falls back to software. Hardware requires GPU encoding; if unavailable, viewers can use JPEG. Preview and snapshots use JPEG.")))
            .child(div().text_xs().text_color(cx.omarchy().secondary)
                .child("Native pixels preserve fine text on scaled displays and use more bandwidth. Maximum width still applies."))
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
    width.map_or("No limit".into(), |v| format!("{v} px"))
}

fn pixel_label(mode: PixelMode) -> &'static str {
    match mode {
        PixelMode::Logical => "Logical pixels",
        PixelMode::Native => "Native pixels",
    }
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
            assert_eq!(
                config.h264_bitrate,
                LiveConfig::default_h264_bitrate(config.fps)
            );
        }
        config.h264_bitrate = 8_000_000;
        StreamPreset::Motion.apply(&mut config);
        assert_eq!(config.h264_bitrate, 8_000_000);
        config.fps = 24;
        assert_eq!(StreamPreset::matching(&config), None);
        assert_eq!(config.fps, 24);
    }

    #[test]
    fn crisp_text_selects_native_pixels_and_other_presets_restore_logical_pixels() {
        let mut config = LiveConfig::default();
        StreamPreset::Text.apply(&mut config);
        assert_eq!(config.pixel_mode, PixelMode::Native);
        assert_eq!(config.quality, 90);
        assert_eq!(config.max_width, None);
        config.pixel_mode = PixelMode::Logical;
        assert_eq!(StreamPreset::matching(&config), None);
        for preset in [StreamPreset::Balanced, StreamPreset::Motion] {
            StreamPreset::Text.apply(&mut config);
            preset.apply(&mut config);
            assert_eq!(config.pixel_mode, PixelMode::Logical);
        }
    }
}
