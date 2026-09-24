use super::*;

impl Render for OmaBeam {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_previews(cx);
        let compact = window.viewport_size().width < px(720.);
        let source_changed = cx.listener(|this, index: &usize, _, cx| {
            if this.busy {
                return;
            }
            if *index != 0 || !matches!(this.page, Page::Tiles | Page::Windows) {
                this.select_page(Page::from_index(*index));
            }
            this.status_for_page();
            cx.notify();
        });
        let mut choices = vec![
            ChoiceItem::new("window", "Window").disabled(self.busy),
            ChoiceItem::new("screen", "Screen").disabled(self.busy),
            ChoiceItem::new("area", "Area").disabled(self.busy),
        ];
        if !self.picker && !self.screenshot_mode {
            choices.push(ChoiceItem::new("extend", "Extend desktop").disabled(self.busy));
        }
        let tabs = tab_list(
            "source-types",
            choices,
            Some(self.page.index()),
            move |index, window, cx| source_changed(&index, window, cx),
            window,
            cx,
        );
        focus_scope("omabeam")
            .size_full()
            .bg(cx.omarchy().background)
            .text_color(cx.omarchy().foreground)
            .child(
                div()
                    .id("omabeam-root")
                    .key_context("OmaBeam")
                    .track_focus(&self.focus)
                    .size_full()
                    .flex()
                    .flex_col()
                    .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                        if this.focus.is_focused(window) {
                            this.confirm(cx);
                        } else {
                            cx.propagate();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                        if !this.busy {
                            this.cancel(cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &CopyShot, _, cx| {
                        this.run_capture(CaptureAction::Copy, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SaveShot, _, cx| {
                        this.run_capture(CaptureAction::Save, cx)
                    }))
                    .on_action(cx.listener(|this, _: &ShareFile, _, cx| {
                        this.run_capture(CaptureAction::Share, cx)
                    }))
                    .on_action(cx.listener(|this, _: &LiveShare, _, cx| this.start_live(cx)))
                    .on_action(cx.listener(|this, _: &MoveLeft, window, cx| {
                        if !this.busy {
                            this.move_selection(-1, 0);
                            this.focus.focus(window, cx);
                            cx.notify();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &MoveRight, window, cx| {
                        if !this.busy {
                            this.move_selection(1, 0);
                            this.focus.focus(window, cx);
                            cx.notify();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &MoveUp, window, cx| {
                        if !this.busy {
                            this.move_selection(0, -1);
                            this.focus.focus(window, cx);
                            cx.notify();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &MoveDown, window, cx| {
                        if !this.busy {
                            this.move_selection(0, 1);
                            this.focus.focus(window, cx);
                            cx.notify();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &NextPage, _, cx| {
                        if !this.busy {
                            this.cycle_page(1);
                            cx.notify();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &PrevPage, _, cx| {
                        if !this.busy {
                            this.cycle_page(-1);
                            cx.notify();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &ToggleHelp, _, cx| {
                        this.show_help = !this.show_help;
                        cx.notify();
                    }))
                    .child(self.render_header(cx))
                    .child(
                        div()
                            .id("picker-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.body_scroll)
                            .flex()
                            .flex_col()
                            .p_4()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                                            .child(if self.picker {
                                                "Choose what this app can see"
                                            } else if self.screenshot_mode {
                                                "What do you want to capture?"
                                            } else {
                                                "What do you want to share?"
                                            }),
                                    )
                                    .child(
                                        button("help", "?", ButtonVariant::Outline, cx)
                                            .accessibility_label("Keyboard help")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.show_help = !this.show_help;
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(tabs)
                            .child(
                                div()
                                    .flex()
                                    .gap_4()
                                    .when(compact, |d| d.flex_col())
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap_2()
                                            .child(self.render_source_toolbar(cx))
                                            .child(self.render_sources(cx)),
                                    )
                                    .child(div().flex_1().min_w_0().child(
                                        if self.page == Page::Extend {
                                            self.render_desktop_preview(cx).into_any_element()
                                        } else {
                                            self.render_preview(cx).into_any_element()
                                        },
                                    )),
                            )
                            .when(!self.picker && !self.screenshot_mode, |d| {
                                d.child(separator(cx))
                                    .child(self.render_stream_settings(cx))
                            })
                            .when(self.show_help, |d| d.child(self.render_help(cx)))
                            .when(!self.status.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.omarchy().secondary)
                                        .child(self.status.clone()),
                                )
                            })
                            .children(
                                self.snapshot
                                    .as_ref()
                                    .err()
                                    .map(|error| badge(error.clone(), Status::Error, cx)),
                            ),
                    )
                    .child(self.render_footer(cx)),
            )
    }
}

impl OmaBeam {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(cx.omarchy().border)
            .child(brand::header(
                if self.picker {
                    "Share with an app"
                } else if self.screenshot_mode {
                    "Capture a moment"
                } else {
                    "Choose what to share"
                },
                cx,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.omarchy().secondary)
                            .child(if self.demo {
                                "Demo · no capture or sharing"
                            } else if self.busy {
                                "Working…"
                            } else {
                                "Preview only"
                            }),
                    )
                    .when(!self.picker, |row| {
                        row.child(
                            button(
                                "capture-mode",
                                if self.screenshot_mode {
                                    "Switch to live share"
                                } else {
                                    "Screenshot"
                                },
                                ButtonVariant::Outline,
                                cx,
                            )
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.screenshot_mode = !this.screenshot_mode;
                                if this.page == Page::Extend {
                                    this.select_page(Page::Outputs);
                                }
                                this.status = "".into();
                                cx.notify();
                            })),
                        )
                    }),
            )
    }

    fn render_source_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_window = matches!(self.page, Page::Tiles | Page::Windows);
        let workspaces = self.occupied_workspaces();
        let mut items = vec![
            MenuItem::new("All windows")
                .checked(self.page == Page::Windows)
                .disabled(self.busy),
        ];
        items.extend(workspaces.iter().map(|(id, name, _)| {
            MenuItem::new(format!("Workspace {name}"))
                .checked(self.page == Page::Tiles && *id == self.workspace_id)
                .disabled(self.busy)
        }));
        let label = if self.page == Page::Windows {
            "All windows".to_string()
        } else {
            format!(
                "Workspace {}",
                workspaces
                    .iter()
                    .find(|(id, _, _)| *id == self.workspace_id)
                    .map(|(_, name, _)| name.clone())
                    .unwrap_or_else(|| self.workspace_id.to_string())
            )
        };
        let change = cx.listener(move |this, index: &usize, _, cx| {
            if *index == 0 {
                this.page = Page::Windows;
            } else if let Some((id, _, _)) = workspaces.get(index - 1) {
                this.page = Page::Tiles;
                this.workspace_id = *id;
                this.follow_workspace = false;
                if !this
                    .current_tiles()
                    .iter()
                    .any(|c| Some(&c.address) == this.selected_window.as_ref())
                {
                    this.selected_window = this
                        .current_tiles()
                        .into_iter()
                        .min_by_key(|c| c.focus_history_id)
                        .map(|c| c.address.clone());
                }
            }
            this.status = "".into();
            cx.notify();
        });
        div()
            .h(px(34.))
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.omarchy().secondary)
                    .child(if is_window {
                        "Pick a window"
                    } else if self.page == Page::Outputs {
                        "Pick a display"
                    } else if self.page == Page::Extend {
                        "Set up an extra display"
                    } else {
                        "Select an area"
                    }),
            )
            .when(is_window, |d| {
                d.child(div().w(px(180.)).child(menu(
                    "workspace-menu",
                    dropdown("workspace", label, self.busy, cx),
                    items,
                    move |index, window, cx| change(&index, window, cx),
                )))
            })
    }

    fn render_sources(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        match self.page {
            Page::Tiles => self.render_tiles(cx).into_any_element(),
            Page::Windows => self.render_windows(cx).into_any_element(),
            Page::Outputs => self.render_outputs(cx).into_any_element(),
            Page::Region => self.render_region(cx).into_any_element(),
            Page::Extend => self.render_desktop_settings(cx).into_any_element(),
        }
    }

    fn render_tiles(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let clients = self.current_tiles();
        let mut tile_views = Vec::new();
        if let Some(monitor) = self.monitor_for_workspace() {
            for tile in tiles_for(&clients, monitor) {
                tile_views.push(self.render_tile(&clients, tile, cx));
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .id("tile-map")
                    .relative()
                    .h(px(250.))
                    .w_full()
                    .bg(cx.omarchy().inset)
                    .border_1()
                    .border_color(cx.omarchy().border)
                    .rounded_md()
                    .overflow_hidden()
                    .children(tile_views)
                    .when(clients.is_empty(), |d| {
                        d.child(empty_state(
                            "No windows here",
                            "Choose another workspace or All windows.",
                            cx,
                        ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.omarchy().secondary)
                            .child("Arranged like your desktop"),
                    )
                    .child(
                        button("all-windows", "All windows", ButtonVariant::Outline, cx)
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = Page::Windows;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_tile(
        &self,
        clients: &[&Client],
        tile: Tile,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let Some(client) = clients.get(tile.index).copied() else {
            return div().into_any_element();
        };
        let selected = self.selected_window.as_deref() == Some(client.address.as_str());
        let address = client.address.clone();
        let theme = cx.omarchy();
        let thumbnail = self.thumbnails.get(&client.stable_id).cloned();
        with_tooltip(
            button(("tile", tile.index), "", ButtonVariant::Secondary, cx)
                .accessibility_label(format!(
                    "{}{}",
                    client_label(client),
                    if selected { ", selected" } else { "" }
                ))
                .selected(selected)
                .disabled(self.busy)
                .absolute()
                .left(relative(tile.rect.x))
                .top(relative(tile.rect.y))
                .w(relative(tile.rect.w))
                .h(relative(tile.rect.h))
                .p_2()
                .flex()
                .flex_col()
                .items_start()
                .justify_start()
                .gap_1()
                .overflow_hidden()
                .bg(if selected {
                    theme.selected_fill()
                } else {
                    theme.surface
                })
                .border_1()
                .border_color(if selected {
                    theme.accent
                } else {
                    theme.control_border()
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .w_full()
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_ellipsis()
                                .flex_1()
                                .child(client.class.clone()),
                        )
                        .when(selected, |d| d.child(icon(IconName::Check))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_ellipsis()
                        .w_full()
                        .text_color(theme.secondary)
                        .child(client.title.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_hidden()
                        .when_some(thumbnail, |d, image| {
                            d.child(img(image).size_full().object_fit(ObjectFit::Contain))
                        }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_window(address.clone());
                    this.focus.focus(window, cx);
                    cx.notify();
                })),
            client_label(client),
        )
        .into_any_element()
    }

    fn render_windows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let clients = self.current_windows();
        let mut rows = Vec::new();
        for (index, client) in clients.iter().enumerate() {
            let selected = self.selected_window.as_deref() == Some(client.address.as_str());
            let address = client.address.clone();
            let thumbnail = self.thumbnails.get(&client.stable_id).cloned();
            rows.push(
                button(("window", index), "", ButtonVariant::Secondary, cx)
                    .disabled(self.busy)
                    .selected(selected)
                    .accessibility_label(client_label(client))
                    .w_full()
                    .h(px(66.))
                    .p_2()
                    .gap_3()
                    .justify_start()
                    .bg(if selected {
                        cx.omarchy().selected_fill()
                    } else {
                        cx.omarchy().surface
                    })
                    .border_color(if selected {
                        cx.omarchy().accent
                    } else {
                        cx.omarchy().border
                    })
                    .child(
                        div()
                            .w(px(68.))
                            .h(px(46.))
                            .bg(cx.omarchy().inset)
                            .when_some(thumbnail, |d, image| {
                                d.child(img(image).size_full().object_fit(ObjectFit::Contain))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().text_ellipsis().child(client_label(client)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.omarchy().secondary)
                                    .child(format!("Workspace {}", client.workspace.name)),
                            ),
                    )
                    .when(selected, |d| d.child(icon(IconName::Check)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_window(address.clone());
                        this.focus.focus(window, cx);
                        cx.notify();
                    })),
            );
        }
        div()
            .id("window-list")
            .h(px(282.))
            .overflow_y_scroll()
            .track_scroll(&self.windows_scroll)
            .flex()
            .flex_col()
            .gap_2()
            .children(rows)
            .when(clients.is_empty(), |d| {
                d.child(empty_state(
                    "No windows",
                    "Open a window, or choose Screen or Area.",
                    cx,
                ))
            })
    }

    fn render_outputs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let monitors = self
            .snapshot()
            .map(|s| s.monitors.as_slice())
            .unwrap_or_default();
        let mut rows = Vec::new();
        for (index, monitor) in monitors.iter().enumerate() {
            let name = monitor.name.clone();
            let selected = self.selected_output.as_ref() == Some(&name);
            let (w, h) = monitor.logical_size();
            rows.push(
                button(("display", index), "", ButtonVariant::Secondary, cx)
                    .disabled(self.busy)
                    .selected(selected)
                    .w_full()
                    .h(px(90.))
                    .p_3()
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_center()
                    .gap_2()
                    .bg(if selected {
                        cx.omarchy().selected_fill()
                    } else {
                        cx.omarchy().surface
                    })
                    .border_color(if selected {
                        cx.omarchy().accent
                    } else {
                        cx.omarchy().border
                    })
                    .child(div().text_sm().text_ellipsis().child(monitor.label()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.omarchy().secondary)
                            .child(format!(
                                "{w:.0} × {h:.0} · Workspace {}{}",
                                monitor.active_workspace.name,
                                if selected { " · Selected" } else { "" }
                            )),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_output(name.clone());
                        this.focus.focus(window, cx);
                        cx.notify();
                    })),
            );
        }
        div()
            .id("displays")
            .h(px(282.))
            .overflow_y_scroll()
            .track_scroll(&self.outputs_scroll)
            .flex()
            .flex_col()
            .gap_2()
            .children(rows)
            .when(monitors.is_empty(), |d| {
                d.child(empty_state(
                    "No displays",
                    "Check the connection to Hyprland.",
                    cx,
                ))
            })
    }

    fn render_region(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = match &self.selected_region {
            Some(Selection::Region { output, w, h, .. }) => format!("{w} × {h} on {output}"),
            _ => "Choose a rectangle on your screen.".into(),
        };
        div()
            .h(px(250.))
            .p_4()
            .flex()
            .flex_col()
            .justify_center()
            .items_start()
            .gap_3()
            .bg(cx.omarchy().inset)
            .border_1()
            .border_color(cx.omarchy().border)
            .rounded_md()
            .child(div().text_sm().child(label))
            .child(
                button(
                    "choose-area",
                    if self.selected_region.is_some() {
                        "Reselect area"
                    } else {
                        "Choose area"
                    },
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.busy)
                .on_click(cx.listener(|this, _, _, cx| this.choose_region(cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.omarchy().secondary)
                    .child("Return here to preview before sharing."),
            )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.preview_frame.as_ref();
        let title = match self.page {
            Page::Tiles | Page::Windows => self.selected_client().map(client_label),
            Page::Outputs => self.selected_monitor().map(Monitor::label),
            Page::Region => self.selected_region.as_ref().map(|_| "Custom area".into()),
            Page::Extend => Some("Extended desktop".into()),
        }
        .unwrap_or_else(|| "Nothing selected".into());
        let privacy = match self.page {
            Page::Tiles | Page::Windows => "Only this window. Other windows stay out of view.",
            Page::Outputs => "Everything on this display is visible, including notifications.",
            Page::Region => "Anything entering this rectangle will be visible.",
            Page::Extend => "The extra display is created when you start sharing.",
        };
        let message = self.preview_error.clone().unwrap_or_else(|| {
            if self.preview_key.is_some() {
                "Preparing preview…".into()
            } else {
                "Choose a source to see its preview.".into()
            }
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .h(px(34.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().text_xs().text_color(cx.omarchy().secondary).child(
                        if self.screenshot_mode {
                            "Capture preview"
                        } else {
                            "Viewer preview"
                        },
                    ))
                    .children(frame.map(|f| {
                        div()
                            .text_xs()
                            .text_color(cx.omarchy().secondary)
                            .child(format!("{} × {}", f.width, f.height))
                    })),
            )
            .child(
                div()
                    .h(px(250.))
                    .w_full()
                    .bg(cx.omarchy().inset)
                    .border_1()
                    .border_color(cx.omarchy().border)
                    .rounded_md()
                    .overflow_hidden()
                    .when_some(frame, |d, f| {
                        d.child(
                            img(f.image.clone())
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        )
                    })
                    .when(frame.is_none(), |d| {
                        d.child(
                            div()
                                .size_full()
                                .p_4()
                                .flex()
                                .flex_col()
                                .justify_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.omarchy().secondary)
                                        .child(message),
                                )
                                .when(self.preview_error.is_some(), |d| {
                                    d.child(
                                        button(
                                            "retry-preview",
                                            "Retry preview",
                                            ButtonVariant::Secondary,
                                            cx,
                                        )
                                        .disabled(self.busy || self.preview_pending)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.preview_updated =
                                                    Instant::now() - Duration::from_secs(10);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(if matches!(self.page, Page::Outputs | Page::Region) {
                        cx.omarchy().warning
                    } else {
                        cx.omarchy().secondary
                    })
                    .child(privacy),
            )
            .when(self.page == Page::Outputs, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(cx.omarchy().secondary)
                        .child("The picker closes when sharing starts."),
                )
            })
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.can_confirm()
            && !self.busy
            && !self.demo
            && (!self.cast_mode || self.screenshot_mode || self.cast_receiver_id.is_some());
        let primary = if self.busy {
            "Working…"
        } else if self.picker {
            "Share with app"
        } else if self.screenshot_mode {
            "Copy screenshot"
        } else if self.cast_mode {
            "Start casting"
        } else if self.page == Page::Extend {
            "Extend desktop"
        } else {
            "Start sharing"
        };
        let token_changed = cx.listener(|this, value: &bool, _, cx| {
            if !this.busy {
                this.allow_token = *value;
                cx.notify();
            }
        });
        div()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(cx.omarchy().border)
            .bg(cx.omarchy().surface)
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.omarchy().secondary)
                    .child(if self.busy {
                        "Preparing your source…"
                    } else if self.demo {
                        "Demo · sharing is disabled"
                    } else if ready {
                        "Ready to share"
                    } else {
                        "Select a source and check its preview"
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .when(self.picker, |d| {
                        d.child(
                            switch(
                                "remember-share",
                                "Remember this share",
                                self.allow_token,
                                cx,
                            )
                            .on_change(move |value, _, window, cx| {
                                token_changed(&value, window, cx)
                            }),
                        )
                    })
                    .when(self.screenshot_mode && !self.picker, |d| {
                        d.child(
                            button("save-shot", "Save", ButtonVariant::Secondary, cx)
                                .disabled(!ready)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_capture(CaptureAction::Save, cx)
                                })),
                        )
                        .child(
                            button("send-shot", "LocalSend", ButtonVariant::Secondary, cx)
                                .disabled(!ready)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_capture(CaptureAction::Share, cx)
                                })),
                        )
                    })
                    .child(
                        button("cancel", "Cancel", ButtonVariant::Outline, cx)
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        button("start-share", primary, ButtonVariant::Primary, cx)
                            .bg(cx.omarchy().accent)
                            .text_color(cx.omarchy().background)
                            .disabled(!ready)
                            .child(icon(IconName::ChevronRight))
                            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                    ),
            )
    }

    fn render_help(&self, cx: &gpui_kit::App) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .p_2()
            .text_xs()
            .bg(cx.omarchy().inset)
            .child(keycap("↑↓←→ / hjkl", cx))
            .child("Choose window")
            .child(keycap("Tab", cx))
            .child("Next control")
            .child(keycap("Ctrl+Tab", cx))
            .child("Source type")
            .child(keycap("Enter", cx))
            .child("Share / capture")
            .child(keycap("Esc", cx))
            .child("Cancel")
    }
}
