use super::*;

// Match the four-tile mark and lockup in omarchy-plugin/ShareContent.qml.
pub(super) fn header(subtitle: impl Into<SharedString>, cx: &gpui_kit::App) -> impl IntoElement {
    let theme = cx.omarchy();
    let mark = div()
        .size(px(38.))
        .flex_shrink_0()
        .rounded(px(6.))
        .bg(theme.accent.opacity(0.1))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(3.))
        .children((0..2).map(|row| {
            div().flex().gap(px(3.)).children((0..2).map(move |col| {
                div().size(px(7.)).rounded(px(1.)).bg(theme
                    .accent
                    .opacity(if row == 1 && col == 1 { 0.35 } else { 1. }))
            }))
        }));
    div()
        .flex()
        .items_center()
        .gap_3()
        .min_w_0()
        .child(mark)
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .gap(px(3.))
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child("OmaBeam"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.foreground.opacity(0.68))
                        .child(subtitle.into()),
                ),
        )
}
