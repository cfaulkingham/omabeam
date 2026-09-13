use super::*;
use crate::hypr::{Rect, desktop::Position};
use settings::field;

const RESOLUTIONS: [(u32, u32, &str); 5] = [
    (1920, 1080, "1920 × 1080"),
    (2560, 1440, "2560 × 1440"),
    (3840, 2160, "3840 × 2160"),
    (2048, 1536, "2048 × 1536"),
    (2560, 1600, "2560 × 1600"),
];

impl OmaBeam {
    pub(super) fn render_desktop_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let config = &self.desktop_config;
        let portrait = config.height > config.width;
        let resolution_changed = cx.listener(|this, index: &usize, _, cx| {
            let portrait = this.desktop_config.height > this.desktop_config.width;
            let (w, h, _) = RESOLUTIONS[*index];
            (this.desktop_config.width, this.desktop_config.height) =
                if portrait { (h, w) } else { (w, h) };
            cx.notify();
        });
        let resolution = menu(
            "desktop-resolution-menu",
            dropdown(
                "desktop-resolution",
                format!("{} × {}", config.width, config.height),
                self.busy,
                cx,
            ),
            RESOLUTIONS
                .iter()
                .map(|(w, h, label)| {
                    MenuItem::new(*label)
                        .checked(
                            config.width.max(config.height) == *w
                                && config.width.min(config.height) == *h,
                        )
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| resolution_changed(&index, window, cx),
        );
        let scale_changed = cx.listener(|this, index: &usize, _, cx| {
            this.desktop_config.scale = *index as u32 + 1;
            cx.notify();
        });
        let scale = menu(
            "desktop-scale-menu",
            dropdown(
                "desktop-scale",
                if config.scale == 2 {
                    "200% · larger text"
                } else {
                    "100%"
                },
                self.busy,
                cx,
            ),
            ["100%", "200% · larger text"]
                .iter()
                .enumerate()
                .map(|(i, label)| {
                    MenuItem::new(*label)
                        .checked(config.scale == i as u32 + 1)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| scale_changed(&index, window, cx),
        );
        let position_changed = cx.listener(|this, index: &usize, _, cx| {
            this.desktop_config.position = Position::ALL[*index];
            cx.notify();
        });
        let position = menu(
            "desktop-position-menu",
            dropdown("desktop-position", config.position.label(), self.busy, cx),
            Position::ALL
                .iter()
                .map(|p| {
                    MenuItem::new(p.label())
                        .checked(*p == config.position)
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| position_changed(&index, window, cx),
        );
        let orientation_changed = cx.listener(|this, index: &usize, _, cx| {
            if (*index == 1) != (this.desktop_config.height > this.desktop_config.width) {
                std::mem::swap(
                    &mut this.desktop_config.width,
                    &mut this.desktop_config.height,
                );
            }
            cx.notify();
        });
        let orientation = menu(
            "desktop-orientation-menu",
            dropdown(
                "desktop-orientation",
                if portrait { "Portrait" } else { "Landscape" },
                self.busy,
                cx,
            ),
            ["Landscape", "Portrait"]
                .iter()
                .enumerate()
                .map(|(i, label)| {
                    MenuItem::new(*label)
                        .checked(portrait == (i == 1))
                        .disabled(self.busy)
                })
                .collect(),
            move |index, window, cx| orientation_changed(&index, window, cx),
        );
        div().flex().flex_col().gap_3()
            .child(div().text_sm().child("Use a tablet, laptop, or another browser as an extra screen."))
            .child(div().flex().flex_wrap().gap_3()
                .child(field("Display resolution", resolution, cx))
                .child(field("Desktop scale", scale, cx))
                .child(field("Placement", position, cx))
                .child(field("Orientation", orientation, cx)))
            .child(div().text_xs().text_color(cx.omarchy().secondary)
                .child("Start sharing, open the link on your other device, and enter fullscreen. Move windows onto the extra screen using this computer’s mouse or keyboard."))
            .child(div().text_xs().text_color(cx.omarchy().secondary)
                .child("Stop sharing from the bar to remove the extra display. Closing the browser keeps it available for reconnection."))
    }

    pub(super) fn render_desktop_preview(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut displays: Vec<(Rect, String, bool)> = self
            .snapshot()
            .map(|snapshot| {
                snapshot
                    .monitors
                    .iter()
                    .map(|m| (m.canvas(), m.name.clone(), false))
                    .collect()
            })
            .unwrap_or_default();
        if let Some((x, y)) = self
            .snapshot()
            .and_then(|snapshot| self.desktop_config.placement(&snapshot.monitors).ok())
        {
            displays.push((
                Rect {
                    x,
                    y,
                    w: (self.desktop_config.width / self.desktop_config.scale) as i32,
                    h: (self.desktop_config.height / self.desktop_config.scale) as i32,
                },
                "Extra screen".into(),
                true,
            ));
        }
        let min_x = displays
            .iter()
            .map(|(r, _, _)| r.x as f32)
            .reduce(f32::min)
            .unwrap_or(0.);
        let min_y = displays
            .iter()
            .map(|(r, _, _)| r.y as f32)
            .reduce(f32::min)
            .unwrap_or(0.);
        let max_x = displays
            .iter()
            .map(|(r, _, _)| r.x as f32 + r.w as f32)
            .reduce(f32::max)
            .unwrap_or(1.);
        let max_y = displays
            .iter()
            .map(|(r, _, _)| r.y as f32 + r.h as f32)
            .reduce(f32::max)
            .unwrap_or(1.);
        let width = (max_x - min_x).max(1.);
        let height = (max_y - min_y).max(1.);
        let theme = cx.omarchy();
        div().flex().flex_col().gap_3().p_3().border_1().border_color(theme.border).rounded_lg()
            .child(div().text_sm().child("Desktop layout"))
            .child(div().relative().w_full().h(px(180.)).children(displays.into_iter().map(|(rect, name, extra)|
                div().absolute().left(relative((rect.x as f32 - min_x) / width))
                    .top(relative((rect.y as f32 - min_y) / height))
                    .w(relative(rect.w as f32 / width)).h(relative(rect.h as f32 / height))
                    .p_1().child(div().size_full().border_2().border_color(if extra { theme.accent } else { theme.border })
                        .bg(theme.surface).rounded_md().flex().items_center().justify_center()
                        .overflow_hidden().text_xs().child(name)))))
            .child(div().text_xs().text_color(theme.secondary)
                .child(format!("{} × {} pixels · {} × {} desktop space", self.desktop_config.width,
                    self.desktop_config.height, self.desktop_config.width / self.desktop_config.scale,
                    self.desktop_config.height / self.desktop_config.scale)))
            .child(div().text_xs().text_color(theme.secondary).child("The extra screen starts empty. Everything moved onto it will be visible to viewers."))
    }
}
