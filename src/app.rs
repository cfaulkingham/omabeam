use gpui_kit::{
    AppContext, ClipboardItem, Context, FocusHandle, Focusable, Image, InteractiveElement,
    IntoElement, KeyBinding, ObjectFit, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, Window, WindowBounds, WindowDecorations,
    WindowKind, WindowOptions, actions, div, img, prelude::FluentBuilder, px, relative, size,
};
use gpui_omarchy::{
    ActiveTheme, ButtonVariant, ChoiceItem, IconName, MenuItem, Status, badge, button, empty_state,
    focus_scope, icon, keycap, menu, separator, switch, tab_list, with_tooltip,
};

use crate::capture::{
    CaptureAction, CaptureRequest, capture, pick_region, request_for_client, request_for_monitor,
};
use crate::hypr::{Client, Monitor, Snapshot, hide_picker, restore_picker};
use crate::layout::{Tile, nearest_in_direction, tiles_for};
use crate::live::{LiveConfig, LiveSource, prefs::StreamPrefs, spawn_daemon};
use crate::portal::{PortalWindow, Selection, parse_window_list};

mod brand;
mod cast;
mod demo;
mod desktop;
mod preview;
mod send;
mod settings;
mod view;
use preview::{PreviewFrame, PreviewKey, PreviewWorker, ThumbnailWorker};
use settings::dropdown;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

actions!(
    omabeam,
    [
        Confirm, Cancel, CopyShot, SaveShot, ShareFile, LiveShare, MoveLeft, MoveRight, MoveUp,
        MoveDown, NextPage, PrevPage, ToggleHelp
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Tiles,
    Windows,
    Outputs,
    Region,
    Extend,
}

impl Page {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Tiles,
            1 => Self::Outputs,
            2 => Self::Region,
            3 => Self::Extend,
            _ => Self::Tiles,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Tiles | Self::Windows => 0,
            Self::Outputs => 1,
            Self::Region => 2,
            Self::Extend => 3,
        }
    }
}

pub struct Options {
    pub picker: bool,
    pub demo: bool,
    pub allow_token: bool,
    pub live_config: LiveConfig,
    pub fps_explicit: bool,
    /// Remembered standalone-picker choices; empty for portal and demo pickers.
    pub stream_prefs: StreamPrefs,
}

impl Options {
    pub fn from_args(args: &[String], live_config: LiveConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            args.iter()
                .all(|arg| matches!(arg.as_str(), "--allow-token" | "--picker" | "--demo-picker")),
            "unexpected picker argument"
        );
        let allow_token = args.iter().any(|arg| arg == "--allow-token");
        let picker = allow_token
            || args.iter().any(|arg| arg == "--picker")
            || std::env::var_os("XDPH_WINDOW_SHARING_LIST").is_some();
        let demo = args.iter().any(|a| a == "--demo-picker");
        anyhow::ensure!(
            !(demo && picker),
            "--demo-picker cannot run as a portal picker"
        );
        Ok(Self {
            demo,
            picker,
            allow_token,
            fps_explicit: live_config.fps != LiveConfig::default().fps,
            live_config,
            stream_prefs: StreamPrefs::default(),
        })
    }
}

pub fn print_help() {
    println!(
        "\
OmaBeam — share a window, screen, or area on Omarchy and Hyprland

Usage:
  omabeam              Open the standalone capture picker
  omabeam --picker     Act as an xdg-desktop-portal-hyprland share picker
  omabeam --allow-token
                         Same as --picker, with restore-token enabled
  omabeam --status     Print live-share JSON if a share is running
  omabeam --share-qr   Print the running share's QR module grid as JSON
  omabeam --stop       End the running live share
  omabeam --send-link    Send the running live-share URL to a nearby OmaSend
                         or LocalSend device. The URL is read from the
                         session file, not from argv.
  omabeam --hypr COMMAND
                         Query Hyprland JSON: clients, monitors, workspaces,
                         activeworkspace, activewindow, version, configerrors;
                         or reload its configuration with COMMAND=reload
  omabeam --live output NAME
  omabeam --live window ADDRESS STABLE_ID [LABEL]
  omabeam --live region OUTPUT X Y W H
  omabeam --live extend WIDTH HEIGHT SCALE POSITION
                         Create an extra desktop; SCALE is 1 or 2;
                         POSITION is right, left, above, or below
  omabeam --cast-devices                 Discover native Cast receivers (JSON)
  omabeam --cast-demo RECEIVER_ID        Cast synthetic video for qualification
  omabeam --cast RECEIVER_ID -- output NAME
  omabeam --cast RECEIVER_ID -- extend WIDTH HEIGHT SCALE POSITION
    Cast accepts the same window/region sources. Default canvas is 720p;
    --width 1920 selects 1080p. Native Cast is under device qualification.
  omabeam --demo       Synthetic stream without a Wayland desktop (localhost)
  omabeam --demo-picker Preview the native picker with synthetic sources; no sharing

Live options (also apply when opening the picker):
  --fps N               1–120 (H.264 max 60; default 60 for extend, 15 otherwise)
  --quality N           JPEG quality 1–95 (default 55)
  --width N             Limit encoded width in the selected pixel mode
  --native-pixels       Preserve captured pixels (default logical resolution)
  --webrtc              H.264 over WebRTC (default) with automatic JPEG fallback
  --jpeg                JPEG/MJPEG instead of H.264
  --webrtc-port N       UDP port for WebRTC (default 9848; 0 selects a port)
  --h264-bitrate N      Target bits/s for H.264 (default 4000000 at 15 FPS,
                         scaled with FPS up to 16000000)
  --encoder MODE        auto (default), hardware, or software
  --check-encoders      Test H.264 encoding and report the selected backend
  --cursor              Include cursor
  --bind ADDRESS        Default 0.0.0.0 (local network); 127.0.0.1 for this computer only
  --port N              Default 9847; 0 chooses an available port
  --                    Treat remaining source arguments literally

Keys:
  h/j/k/l or arrows      Move between tiles
  Tab / Shift+Tab        Move between controls
  Ctrl+Tab               Cycle Window, Screen, Area, Extend desktop
  Enter                  Start sharing (or copy in Screenshot mode)
  c                      Copy a screenshot of the tile
  s                      Save a screenshot
  f                      Share the screenshot with LocalSend
  v                      Live-share the tile on the local network
  n                      Send the live-share link to a nearby device (bar)
  ?                      Show keyboard help
  Esc                    Cancel

Live share closes this window so you can use the tile. OmaBeam then
scans the LAN and can send the link with the LocalSend protocol — the
LocalSend app is not required on this machine. The other computer needs
OmaSend or LocalSend open to accept. The Omarchy bar screen icon stays
active; open it to copy the URL, send it again, or stop sharing.
"
    );
}

pub struct OmaBeam {
    focus: FocusHandle,
    picker: bool,
    demo: bool,
    allow_token: bool,
    page: Page,
    snapshot: Result<Snapshot, SharedString>,
    portal: Vec<PortalWindow>,
    workspace_id: i32,
    selected_window: Option<String>,
    selected_output: Option<String>,
    live_config: LiveConfig,
    stream_prefs: StreamPrefs,
    cast_mode: bool,
    browser_config: Option<(LiveConfig, crate::hypr::desktop::DesktopConfig, bool)>,
    cast_receivers: Vec<omabeam_cast::Receiver>,
    cast_receiver_id: Option<String>,
    cast_scanning: bool,
    cast_error: Option<String>,
    fps_selected: bool,
    /// Remembered FPS for window, screen, and area sharing.
    base_fps: u32,
    /// Settings to restore when the picker leaves the Extend page.
    extend_saved: Option<ExtendSaved>,
    desktop_config: crate::hypr::desktop::DesktopConfig,
    follow_workspace: bool,
    status: SharedString,
    busy: bool,
    show_help: bool,
    screenshot_mode: bool,
    show_advanced: bool,
    selected_region: Option<Selection>,
    preview_worker: PreviewWorker,
    thumbnail_worker: ThumbnailWorker,
    preview_key: Option<PreviewKey>,
    preview_frame: Option<PreviewFrame>,
    preview_error: Option<String>,
    preview_updated: Instant,
    thumbnail_index: usize,
    thumbnail_attempts: HashMap<String, Instant>,
    thumbnails: HashMap<String, Arc<Image>>,
    body_scroll: ScrollHandle,
    windows_scroll: ScrollHandle,
    outputs_scroll: ScrollHandle,
}

impl OmaBeam {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, options: Options) -> Self {
        cx.bind_keys([
            KeyBinding::new(
                "escape",
                Cancel,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "enter",
                Confirm,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "c",
                CopyShot,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "s",
                SaveShot,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "f",
                ShareFile,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "v",
                LiveShare,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "h",
                MoveLeft,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "left",
                MoveLeft,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "l",
                MoveRight,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "right",
                MoveRight,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "k",
                MoveUp,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "up",
                MoveUp,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "j",
                MoveDown,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "down",
                MoveDown,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "ctrl-tab",
                NextPage,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "ctrl-shift-tab",
                PrevPage,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
            KeyBinding::new(
                "shift-/",
                ToggleHelp,
                Some("OmaBeam && !Popover && !OmarchyOptionGroup"),
            ),
        ]);

        let portal = std::env::var("XDPH_WINDOW_SHARING_LIST")
            .map(|raw| parse_window_list(&raw))
            .unwrap_or_default();
        let snapshot = if options.demo {
            Ok(demo::snapshot())
        } else {
            Snapshot::load()
        }
        .map(|snapshot| snapshot.with_portal_windows(&portal))
        .map_err(|err| SharedString::from(err.to_string()));
        let workspace_id = snapshot
            .as_ref()
            .ok()
            .map(|snapshot| snapshot.active_workspace.id)
            .unwrap_or(1);
        let selected_window = snapshot.as_ref().ok().and_then(|snapshot| {
            snapshot
                .tiles_on(workspace_id)
                .into_iter()
                .min_by_key(|c| c.focus_history_id)
                .map(|client| client.address.clone())
        });
        let selected_output = snapshot
            .as_ref()
            .ok()
            .and_then(Snapshot::focused_monitor)
            .map(|monitor| monitor.name.clone());
        let focus = cx.focus_handle();
        focus.focus(window, cx);

        // One refresh at a time; command execution stays off the UI thread.
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(750))
                    .await;
                match view.update(cx, |this, _| !this.busy) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => break,
                }
                let demo = view.update(cx, |this, _| this.demo).unwrap_or(false);
                let snapshot = cx
                    .background_executor()
                    .spawn(async move {
                        if demo {
                            Ok(demo::snapshot())
                        } else {
                            Snapshot::load()
                        }
                    })
                    .await;
                if view
                    .update(cx, |this, cx| {
                        if !this.busy {
                            this.refresh_snapshot(snapshot);
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

        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if view
                    .update(cx, |this, cx| {
                        if !this.busy {
                            this.update_previews(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut preview_worker = preview::preview_worker(options.demo);
        let preview_error = preview_worker
            .start()
            .err()
            .map(|error| format!("Could not start preview: {error}"));
        let (fps_selected, base_fps) = starting_fps(options.fps_explicit, &options.stream_prefs);

        Self {
            focus,
            picker: options.picker,
            demo: options.demo,
            allow_token: options.allow_token,
            page: Page::Tiles,
            snapshot,
            portal,
            workspace_id,
            selected_window,
            selected_output,
            fps_selected,
            base_fps,
            extend_saved: None,
            live_config: options.live_config,
            stream_prefs: options.stream_prefs,
            cast_mode: false,
            browser_config: None,
            cast_receivers: Vec::new(),
            cast_receiver_id: None,
            cast_scanning: false,
            cast_error: None,
            desktop_config: Default::default(),
            follow_workspace: true,
            status: "".into(),
            busy: false,
            show_help: false,
            screenshot_mode: false,
            show_advanced: false,
            selected_region: None,
            preview_worker,
            thumbnail_worker: preview::thumbnail_worker(options.demo),
            preview_key: None,
            preview_frame: None,
            preview_error,
            preview_updated: Instant::now() - Duration::from_secs(1),
            thumbnail_index: 0,
            thumbnail_attempts: HashMap::new(),
            thumbnails: HashMap::new(),
            body_scroll: ScrollHandle::new(),
            windows_scroll: ScrollHandle::new(),
            outputs_scroll: ScrollHandle::new(),
        }
    }

    fn refresh_snapshot(&mut self, result: anyhow::Result<Snapshot>) {
        let snapshot = match result {
            Ok(snapshot) => snapshot.with_portal_windows(&self.portal),
            Err(error) => {
                self.snapshot = Err(error.to_string().into());
                self.selected_window = None;
                self.selected_output = None;
                self.selected_region = None;
                return;
            }
        };
        self.selected_window = crate::hypr::retain_selected_window(
            self.snapshot().and_then(|old| {
                old.clients
                    .iter()
                    .find(|c| Some(&c.address) == self.selected_window.as_ref())
            }),
            &snapshot,
        );
        if self.follow_workspace && snapshot.active_workspace.id > 0 {
            self.workspace_id = snapshot.active_workspace.id;
            if self.page == Page::Tiles
                && !snapshot
                    .tiles_on(self.workspace_id)
                    .iter()
                    .any(|c| Some(&c.address) == self.selected_window.as_ref())
            {
                self.selected_window = None;
            }
        }
        if !snapshot
            .monitors
            .iter()
            .any(|m| Some(&m.name) == self.selected_output.as_ref())
        {
            self.selected_output = None;
        }
        if let Some(Selection::Region { output, .. }) = &self.selected_region {
            if !snapshot.monitors.iter().any(|m| &m.name == output) {
                self.selected_region = None;
            }
        }
        self.snapshot = Ok(snapshot);
    }

    fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref().ok()
    }

    fn current_tiles(&self) -> Vec<&Client> {
        self.snapshot()
            .map(|snapshot| snapshot.tiles_on(self.workspace_id))
            .unwrap_or_default()
    }

    fn current_windows(&self) -> Vec<&Client> {
        self.snapshot()
            .map(Snapshot::listed_windows)
            .unwrap_or_default()
    }

    fn selected_client(&self) -> Option<&Client> {
        let address = self.selected_window.as_deref()?;
        self.snapshot()?.clients.iter().find(|client| {
            client.address == address && client.mapped && !client.hidden && !client.is_picker()
        })
    }

    fn selected_monitor(&self) -> Option<&Monitor> {
        let name = self.selected_output.as_deref()?;
        self.snapshot()?.monitor_by_name(name)
    }

    fn select_window(&mut self, address: String) {
        let info = self.snapshot().and_then(|snapshot| {
            snapshot
                .clients
                .iter()
                .find(|client| client.address == address)
                .map(|client| (client.class.clone(), client.title.clone(), client.monitor))
        });
        self.selected_window = Some(address);
        if let Some((class, title, monitor)) = info {
            self.selected_output = self
                .snapshot()
                .and_then(|snapshot| snapshot.monitor_by_id(monitor))
                .map(|monitor| monitor.name.clone());
            self.status = format!("{} — {}", class, truncate(&title, 72)).into();
        }
    }

    fn select_output(&mut self, name: String) {
        self.selected_output = Some(name);
        if let Some(monitor) = self.selected_monitor() {
            self.status = format!("Entire screen selected: {}", monitor.label()).into();
        }
    }

    fn move_selection(&mut self, dx: i32, dy: i32) {
        match self.page {
            Page::Tiles => {
                let clients = self.current_tiles();
                let Some(monitor) = self.monitor_for_workspace() else {
                    return;
                };
                let tiles = tiles_for(&clients, monitor);
                let current = clients
                    .iter()
                    .position(|client| {
                        Some(client.address.as_str()) == self.selected_window.as_deref()
                    })
                    .unwrap_or(0);
                if let Some(next) = nearest_in_direction(current, &tiles, dx, dy)
                    && let Some(client) = clients.get(next)
                {
                    self.select_window(client.address.clone());
                }
            }
            Page::Windows => {
                let windows = self.current_windows();
                let next = self.step_list(
                    &windows
                        .iter()
                        .map(|client| client.address.clone())
                        .collect::<Vec<_>>(),
                    dy,
                    true,
                );
                if let Some(index) = next {
                    self.windows_scroll.scroll_to_item(index);
                }
            }
            Page::Outputs => {
                let names = self
                    .snapshot()
                    .map(|snapshot| {
                        snapshot
                            .monitors
                            .iter()
                            .map(|monitor| monitor.name.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if let Some(index) = self.step_list(&names, dy, false) {
                    self.outputs_scroll.scroll_to_item(index);
                }
            }
            Page::Region | Page::Extend => {}
        }
    }

    fn step_list(&mut self, items: &[String], delta: i32, windows: bool) -> Option<usize> {
        if items.is_empty() || delta == 0 {
            return None;
        }
        let current = if windows {
            self.selected_window.as_deref()
        } else {
            self.selected_output.as_deref()
        };
        let index = items
            .iter()
            .position(|item| Some(item.as_str()) == current)
            .unwrap_or(0);
        let next = (index as i32 + delta).rem_euclid(items.len() as i32) as usize;
        let value = items[next].clone();
        if windows {
            self.select_window(value);
        } else {
            self.select_output(value);
        }
        Some(next)
    }

    fn monitor_for_workspace(&self) -> Option<&Monitor> {
        let snapshot = self.snapshot()?;
        snapshot
            .tiles_on(self.workspace_id)
            .first()
            .and_then(|client| snapshot.monitor_by_id(client.monitor))
            .or_else(|| snapshot.focused_monitor())
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.demo || self.busy || !self.can_confirm() {
            return;
        }
        if self.picker {
            self.emit_selection(cx);
        } else if self.screenshot_mode {
            self.run_capture(CaptureAction::Copy, cx);
        } else {
            self.start_live(cx);
        }
    }

    fn emit_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.portal_selection() else {
            self.status =
                "Select a uniquely identified window, or choose Area to share a screen region."
                    .into();
            cx.notify();
            return;
        };
        println!("{}", selection.encode(self.allow_token));
        let _ = std::io::Write::flush(&mut std::io::stdout());
        cx.quit();
    }

    fn portal_selection(&self) -> Option<Selection> {
        match self.page {
            Page::Extend => None,
            Page::Outputs => self.selected_monitor().map(|monitor| Selection::Screen {
                name: monitor.name.clone(),
            }),
            Page::Region => self.selected_region.clone(),
            Page::Tiles | Page::Windows => {
                let client = self.selected_client()?;
                if let Some(id) = client.portal_id {
                    return Some(Selection::Window { id });
                }
                None
            }
        }
    }

    fn choose_region(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            self.selected_region = Some(Selection::Region {
                output: "DEMO-1".into(),
                x: 40,
                y: 80,
                w: 640,
                h: 360,
            });
            cx.notify();
            return;
        }
        if self.busy {
            return;
        }
        self.preview_key = None;
        if let Some(frame) = self.preview_frame.take() {
            frame.image.remove_asset(cx);
        }
        self.busy = true;
        self.status = "Drag to select an area. Escape cancels.".into();
        cx.notify();
        let operation = cx.background_executor().spawn(async { pick_region() });
        cx.spawn(async move |view, cx| {
            let result = operation.await;
            // Hyprland IPC stays off the UI thread; `busy` holds until it ends.
            cx.background_executor()
                .spawn(async { restore_picker() })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(selection @ Selection::Region { .. }) => {
                        this.selected_region = Some(selection);
                        this.status = "Area selected. Check the preview before sharing.".into();
                    }
                    Err(err) => this.status = friendly_error(&err.to_string()).into(),
                    _ => this.status = "Could not read that area.".into(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn capture_request(&self) -> Option<anyhow::Result<CaptureRequest>> {
        match self.page {
            Page::Extend => None,
            Page::Outputs => self.selected_monitor().map(|m| Ok(request_for_monitor(m))),
            Page::Region => self
                .selected_region
                .clone()
                .map(|r| Ok(CaptureRequest::Region(r))),
            Page::Tiles | Page::Windows => self.selected_client().map(request_for_client),
        }
    }

    fn run_capture(&mut self, action: CaptureAction, cx: &mut Context<Self>) {
        if self.demo || self.busy || self.picker {
            return;
        }
        if self.page == Page::Region && self.selected_region.is_none() {
            self.choose_region(cx);
            return;
        }
        let request = self.capture_request();
        let Some(request) = request else {
            self.status = "Select a tile first.".into();
            cx.notify();
            return;
        };
        match request {
            Ok(request) => self.run_capture_request(request, action, cx),
            Err(error) => {
                self.status = error.to_string().into();
                cx.notify();
            }
        }
    }

    fn run_capture_request(
        &mut self,
        request: CaptureRequest,
        action: CaptureAction,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.status = match action {
            CaptureAction::Copy => "Capturing and copying…",
            CaptureAction::Save => "Capturing and saving…",
            CaptureAction::Share => "Capturing and opening LocalSend…",
        }
        .into();
        cx.notify();

        let operation = cx
            .background_executor()
            .spawn(async move { capture(&request, action) });
        cx.spawn(async move |view, cx| {
            let result = operation.await;
            if result.is_err() {
                // Hyprland IPC stays off the UI thread; `busy` holds until it ends.
                cx.background_executor()
                    .spawn(async { restore_picker() })
                    .await;
            }
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(path) => {
                        let message = match (action, path) {
                            (CaptureAction::Copy, _) => {
                                "Screenshot copied to the clipboard.".to_string()
                            }
                            (CaptureAction::Save, Some(path)) => {
                                format!("Screenshot saved to {}.", path.display())
                            }
                            (CaptureAction::Share, _) => {
                                "Screenshot sent to LocalSend.".to_string()
                            }
                            (CaptureAction::Save, None) => "Screenshot saved.".to_string(),
                        };
                        this.status = message.clone().into();
                        desktop_notify(&message);
                        cx.quit();
                    }
                    Err(err) => {
                        this.status = friendly_error(&err.to_string()).into();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn live_source(&self) -> Option<LiveSource> {
        match self.page {
            Page::Extend => Some(LiveSource::Extend(self.desktop_config.clone())),
            Page::Tiles | Page::Windows => {
                let client = self.selected_client()?;
                if client.is_picker() {
                    return None;
                }
                Some(LiveSource::Window {
                    address: client.address.clone(),
                    stable_id: client.stable_id.clone(),
                    label: format!("{} — {}", client.class, truncate(&client.title, 48)),
                })
            }
            Page::Outputs => self.selected_monitor().map(|monitor| LiveSource::Output {
                name: monitor.name.clone(),
            }),
            Page::Region => match self.selected_region.clone()? {
                Selection::Region { output, x, y, w, h } => {
                    Some(LiveSource::Region { output, x, y, w, h })
                }
                _ => None,
            },
        }
    }

    fn start_live(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        if self.screenshot_mode {
            self.screenshot_mode = false;
            self.status = "Check the stream preview, then start sharing.".into();
            cx.notify();
            return;
        }
        if self.busy {
            return;
        }
        if self.picker {
            self.status = "Live share is for the standalone picker.".into();
            cx.notify();
            return;
        }
        if !self.can_confirm() {
            self.status = "Choose a source and wait for its preview before sharing.".into();
            cx.notify();
            return;
        }
        let Some(source) = self.live_source() else {
            self.status = "Choose a tile, window, or screen before starting live share.".into();
            cx.notify();
            return;
        };
        self.start_live_source(source, cx);
    }

    fn start_live_source(&mut self, source: LiveSource, cx: &mut Context<Self>) {
        if self.cast_mode && self.cast_receiver_id.is_none() {
            self.status = "Choose a Cast receiver before starting.".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.status = if self.cast_mode {
            "Connecting to your Cast receiver…"
        } else if self.page == Page::Extend {
            "Creating your extended desktop…"
        } else {
            "Starting live share…"
        }
        .into();
        cx.notify();
        let config = self.live_config.clone();
        let cast_id = self.cast_receiver_id.clone().filter(|_| self.cast_mode);
        let operation = cx.background_executor().spawn(async move {
            // Hide the picker before capture starts; its Hyprland IPC stays off
            // the UI thread.
            hide_picker();
            if let Some(id) = cast_id {
                crate::live::cast::spawn_daemon(&id, &source, &config).map(|_| None)
            } else {
                spawn_daemon(&source, &config).map(Some)
            }
        });
        cx.spawn(async move |view, cx| {
            let result = operation.await;
            if result.is_err() {
                // Hyprland IPC stays off the UI thread; `busy` holds until it ends.
                cx.background_executor()
                    .spawn(async { restore_picker() })
                    .await;
            }
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Some(url)) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        let sent = crate::localsend::spawn_window(&url);
                        desktop_notify(if sent.is_ok() {
                            "Live share started. Its URL was copied. Choose a nearby device to send it, or use the OmaBeam bar icon."
                        } else {
                            "Live share started and its URL was copied. Use the OmaBeam bar icon to send it, copy it again, or stop sharing."
                        });
                        cx.quit();
                    }
                    Ok(None) => {
                        desktop_notify("Casting started. Use the OmaBeam bar icon to stop.");
                        cx.quit();
                    }
                    Err(err) => {
                        this.status = friendly_error(&err.to_string()).into();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn cycle_page(&mut self, delta: i32) {
        let count = if self.picker || self.screenshot_mode {
            3
        } else {
            4
        };
        let next = (self.page.index() as i32 + delta).rem_euclid(count) as usize;
        self.select_page(Page::from_index(next));
        self.status_for_page();
    }

    fn select_page(&mut self, page: Page) {
        apply_page_defaults(
            &mut self.live_config,
            &mut self.extend_saved,
            self.page,
            page,
            self.cast_mode,
            self.fps_selected,
            self.base_fps,
        );
        self.page = page;
    }

    fn status_for_page(&mut self) {
        self.status = "".into();
    }

    fn occupied_workspaces(&self) -> Vec<(i32, String, usize)> {
        let Some(snapshot) = self.snapshot() else {
            return Vec::new();
        };
        snapshot
            .workspaces
            .iter()
            .filter_map(|workspace| {
                let count = snapshot.tiles_on(workspace.id).len();
                (count > 0).then_some((workspace.id, workspace.name.clone(), count))
            })
            .collect()
    }
}

/// The picker's starting `(fps_selected, base_fps)`. Only `--fps` chooses a
/// rate; a remembered one is the base rate, so Extend still starts at 60 FPS.
fn starting_fps(cli_fps: bool, prefs: &StreamPrefs) -> (bool, u32) {
    (cli_fps, prefs.valid_fps().unwrap_or(15))
}

/// Stream settings that entering the Extend page replaced. Leaving the page
/// restores them, so choices made there stay on that page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExtendSaved {
    cursor: bool,
    pixel_mode: omabeam_capture::PixelMode,
    /// `None` when entered in Cast mode, which keeps its width.
    max_width: Option<Option<u32>>,
}

/// Automatic stream settings for a page change; never remembered. `base_fps`
/// is the rate for window, screen, and area sharing. `saved` holds what
/// entering Extend replaced until the picker leaves that page.
fn apply_page_defaults(
    config: &mut LiveConfig,
    saved: &mut Option<ExtendSaved>,
    from: Page,
    to: Page,
    cast_mode: bool,
    fps_selected: bool,
    base_fps: u32,
) {
    if !fps_selected {
        config.set_fps(if cast_mode {
            30
        } else if to == Page::Extend {
            60
        } else {
            base_fps
        });
    }
    if to == Page::Extend && from != Page::Extend {
        *saved = Some(ExtendSaved {
            cursor: config.cursor,
            pixel_mode: config.pixel_mode,
            max_width: (!cast_mode).then_some(config.max_width),
        });
        // A second screen should show the host pointer and retain its
        // configured pixel resolution, including a 2× desktop scale.
        config.cursor = true;
        config.pixel_mode = omabeam_capture::PixelMode::Native;
        // Cast keeps its width: it selects the 720p or 1080p canvas.
        if !cast_mode {
            config.max_width = None;
        }
    } else if from == Page::Extend
        && to != Page::Extend
        && let Some(before) = saved.take()
    {
        config.cursor = before.cursor;
        config.pixel_mode = before.pixel_mode;
        // The destination can change on the Extend page; Cast keeps its canvas.
        if !cast_mode && let Some(max_width) = before.max_width {
            config.max_width = max_width;
        }
    }
}

impl Focusable for OmaBeam {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

fn client_label(client: &Client) -> String {
    match (client.class.trim(), client.title.trim()) {
        ("", "") => "Untitled window".into(),
        ("", title) => title.into(),
        (class, "") => class.into(),
        (class, title) => format!("{class} — {title}"),
    }
}

fn friendly_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("region selection cancelled") {
        "Area selection cancelled. Nothing was shared.".into()
    } else if lower.contains("wl-copy") {
        "Couldn’t copy the screenshot. Check that wl-copy is installed and try again.".into()
    } else if lower.contains("capture")
        || lower.contains("wayland")
        || lower.contains("selected output")
    {
        format!("Couldn’t capture the selection. {raw}")
    } else if lower.contains("omarchy share") {
        "Couldn’t open LocalSend. Check that Omarchy sharing is available and try again.".into()
    } else if lower.contains("live share") || lower.contains("bind") {
        format!("Couldn’t start live sharing. {raw}")
    } else {
        format!("Something went wrong: {raw}")
    }
}

/// Show a desktop notification without waiting for `notify-send`, which can
/// stall on a slow notification daemon; a throwaway thread reaps it.
pub fn desktop_notify(message: &str) {
    let Ok(mut child) = std::process::Command::new("/usr/bin/notify-send")
        .args(["OmaBeam", message])
        .spawn()
    else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("omabeam-notify".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

fn truncate(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

const PICKER_PREFERRED_W: f32 = 960.0;
const PICKER_PREFERRED_H: f32 = 650.0;
const PICKER_MAX_W: f32 = 1100.0;
const PICKER_MAX_H: f32 = 800.0;
const PICKER_MIN_W: f32 = 560.0;
const PICKER_MIN_H: f32 = 360.0;
const PICKER_MARGIN: f32 = 32.0;

fn picker_window_size() -> gpui_kit::Size<gpui_kit::Pixels> {
    Snapshot::load()
        .ok()
        .and_then(|snapshot| snapshot.focused_monitor().cloned())
        .map(|monitor| picker_size_for_monitor(&monitor))
        .unwrap_or_else(|| size(px(PICKER_PREFERRED_W), px(PICKER_PREFERRED_H)))
}

fn picker_size_for_monitor(monitor: &Monitor) -> gpui_kit::Size<gpui_kit::Pixels> {
    let (usable_w, usable_h) = monitor.usable_size();
    let max_w = (usable_w - PICKER_MARGIN).max(1.0).min(PICKER_MAX_W);
    let max_h = (usable_h - PICKER_MARGIN).max(1.0).min(PICKER_MAX_H);
    let width = PICKER_PREFERRED_W.min(max_w).max(PICKER_MIN_W.min(max_w));
    let height = PICKER_PREFERRED_H.min(max_h).max(PICKER_MIN_H.min(max_h));
    size(px(width.round()), px(height.round()))
}

pub fn open_send_link(url: String) {
    send::open(url);
}

pub fn open(options: Options) {
    gpui_kit::application().run(move |cx| {
        gpui_omarchy::init(cx);
        let bounds = WindowBounds::centered(picker_window_size(), cx);
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
                window_min_size: Some(size(px(PICKER_MIN_W), px(PICKER_MIN_H))),
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| OmaBeam::new(window, cx, options)),
        )
        .expect("open OmaBeam window");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use omabeam_capture::PixelMode::{self, Logical, Native};

    #[test]
    fn demo_picker_cannot_be_combined_with_portal_mode() {
        let args = vec!["--demo-picker".into(), "--picker".into()];
        assert!(Options::from_args(&args, LiveConfig::default()).is_err());
        let (config, rest) =
            LiveConfig::parse_args(&["--demo-picker".into(), "--fps".into(), "24".into()]).unwrap();
        assert_eq!(config.fps, 24);
        assert_eq!(rest, vec!["--demo-picker"]);
    }

    #[test]
    fn send_link_is_a_known_command() {
        let (config, rest) = LiveConfig::parse_args(&[
            "--send-link".into(),
            "http://192.168.1.24:9847/s/abc/".into(),
        ])
        .unwrap();
        assert_eq!(config, LiveConfig::default());
        assert_eq!(rest, vec!["--send-link", "http://192.168.1.24:9847/s/abc/"]);
    }

    #[test]
    fn source_order_starts_with_the_safer_tile_choice() {
        assert_eq!(Page::from_index(0), Page::Tiles);
        assert_eq!(Page::Tiles.index(), 0);
        assert_eq!(Page::Outputs.index(), 1);
        assert_eq!(Page::Windows.index(), Page::Tiles.index());
        assert_eq!(Page::from_index(2), Page::Region);
        assert_eq!(Page::from_index(3), Page::Extend);
        assert_eq!(Page::Extend.index(), 3);
    }

    #[test]
    fn extend_starts_at_60_fps_and_full_resolution_while_other_pages_keep_the_base_rate() {
        // Remembered 30 FPS with a 1280-pixel cap, cursor off, logical pixels.
        let mut config = LiveConfig {
            max_width: Some(1280),
            ..LiveConfig::default()
        };
        config.set_fps(30);
        let mut saved = None;
        apply_page_defaults(
            &mut config,
            &mut saved,
            Page::Outputs,
            Page::Tiles,
            false,
            false,
            30,
        );
        assert_eq!(config.fps, 30);
        assert_eq!(config.max_width, Some(1280));
        apply_page_defaults(
            &mut config,
            &mut saved,
            Page::Tiles,
            Page::Extend,
            false,
            false,
            30,
        );
        assert_eq!(config.fps, 60);
        assert_eq!(config.h264_bitrate, LiveConfig::default_h264_bitrate(60));
        assert!(config.cursor);
        assert_eq!(config.pixel_mode, omabeam_capture::PixelMode::Native);
        assert_eq!(config.max_width, None);
        apply_page_defaults(
            &mut config,
            &mut saved,
            Page::Extend,
            Page::Windows,
            false,
            false,
            30,
        );
        assert_eq!(config.fps, 30);
        assert_eq!(config.h264_bitrate, LiveConfig::default_h264_bitrate(30));
    }

    #[test]
    fn a_chosen_fps_is_kept_on_every_page() {
        for cast_mode in [false, true] {
            let mut config = LiveConfig::default();
            config.set_fps(24);
            let mut saved = None;
            let mut from = Page::Tiles;
            for to in [
                Page::Windows,
                Page::Outputs,
                Page::Region,
                Page::Extend,
                Page::Tiles,
            ] {
                apply_page_defaults(&mut config, &mut saved, from, to, cast_mode, true, 30);
                assert_eq!(config.fps, 24, "{from:?} to {to:?}, cast {cast_mode}");
                from = to;
            }
        }
    }

    #[test]
    fn cast_uses_30_fps_on_every_page_and_keeps_its_canvas_width() {
        for page in [
            Page::Tiles,
            Page::Windows,
            Page::Outputs,
            Page::Region,
            Page::Extend,
        ] {
            let mut config = LiveConfig {
                max_width: Some(1920),
                ..LiveConfig::default()
            };
            apply_page_defaults(&mut config, &mut None, Page::Tiles, page, true, false, 15);
            assert_eq!(config.fps, 30, "{page:?}");
            // The width selects the 1080p Cast canvas, not a cap on the desktop.
            assert_eq!(config.max_width, Some(1920), "{page:?}");
        }
    }

    /// Page changes as `OmaBeam::select_page` makes them, with no chosen FPS.
    struct Picker {
        config: LiveConfig,
        saved: Option<ExtendSaved>,
        page: Page,
        cast_mode: bool,
    }

    impl Picker {
        /// On Tiles with the cursor off, logical pixels, and `max_width`.
        fn new(max_width: Option<u32>, cast_mode: bool) -> Self {
            let config = LiveConfig {
                max_width,
                ..LiveConfig::default()
            };
            Self {
                config,
                saved: None,
                page: Page::Tiles,
                cast_mode,
            }
        }

        /// Select `page`, then return the cursor, pixel mode, and width.
        fn select(&mut self, page: Page) -> (bool, PixelMode, Option<u32>) {
            apply_page_defaults(
                &mut self.config,
                &mut self.saved,
                self.page,
                page,
                self.cast_mode,
                false,
                15,
            );
            self.page = page;
            (
                self.config.cursor,
                self.config.pixel_mode,
                self.config.max_width,
            )
        }
    }

    #[test]
    fn leaving_extend_restores_the_cursor_pixels_and_width_it_replaced() {
        let mut picker = Picker::new(Some(1280), false);
        assert_eq!(picker.select(Page::Extend), (true, Native, None));
        assert_eq!(picker.select(Page::Tiles), (false, Logical, Some(1280)));
    }

    #[test]
    fn every_page_after_extend_restores_and_each_visit_saves_afresh() {
        for page in [Page::Windows, Page::Outputs, Page::Region] {
            let mut picker = Picker::new(Some(1280), false);
            picker.select(Page::Extend);
            assert_eq!(
                picker.select(page),
                (false, Logical, Some(1280)),
                "{page:?}"
            );
            assert_eq!(picker.saved, None, "{page:?}");
        }
        // Choices made after one visit are what the next visit restores.
        let mut picker = Picker::new(Some(1280), false);
        picker.select(Page::Extend);
        picker.select(Page::Windows);
        picker.config.cursor = true;
        picker.config.max_width = Some(1920);
        picker.select(Page::Extend);
        assert_eq!(picker.select(Page::Outputs), (true, Logical, Some(1920)));
        assert_eq!(picker.saved, None);
    }

    #[test]
    fn selecting_extend_again_keeps_the_choices_made_there() {
        let mut picker = Picker::new(Some(1280), false);
        picker.select(Page::Extend);
        let saved = picker.saved;
        assert_eq!(picker.select(Page::Extend), (true, Native, None));
        // The viewer turns the cursor off on the Extend page.
        picker.config.cursor = false;
        assert_eq!(picker.select(Page::Extend), (false, Native, None));
        assert_eq!(picker.saved, saved);
        assert_eq!(picker.select(Page::Tiles), (false, Logical, Some(1280)));
    }

    #[test]
    fn cast_keeps_its_canvas_width_through_an_extend_visit() {
        let mut picker = Picker::new(Some(1920), true);
        assert_eq!(picker.select(Page::Extend), (true, Native, Some(1920)));
        assert_eq!(picker.select(Page::Tiles), (false, Logical, Some(1920)));
    }

    #[test]
    fn a_destination_switch_on_extend_keeps_each_destinations_width() {
        // Browser link to Google Cast on the Extend page, then 1080p: leaving
        // keeps the Cast canvas and restores the cursor and pixels.
        let mut picker = Picker::new(None, false);
        picker.select(Page::Extend);
        picker.cast_mode = true;
        picker.config.max_width = Some(1920);
        assert_eq!(picker.select(Page::Tiles), (false, Logical, Some(1920)));
        // Google Cast back to Browser link restores the browser settings
        // (src/app/cast.rs); the Cast canvas width must not return as a cap.
        let mut picker = Picker::new(Some(1280), true);
        picker.select(Page::Extend);
        picker.cast_mode = false;
        picker.config = LiveConfig::default();
        assert_eq!(picker.select(Page::Tiles), (false, Logical, None));
    }

    #[test]
    fn only_a_command_line_fps_counts_as_chosen_at_startup() {
        let remembered = |fps| StreamPrefs {
            fps: Some(fps),
            ..StreamPrefs::default()
        };
        assert_eq!(starting_fps(false, &remembered(30)), (false, 30));
        assert_eq!(starting_fps(false, &StreamPrefs::default()), (false, 15));
        assert_eq!(starting_fps(true, &remembered(30)), (true, 30));
        assert_eq!(starting_fps(false, &remembered(500)), (false, 15));
    }

    #[test]
    fn command_errors_are_explained_in_user_language() {
        assert_eq!(
            friendly_error("region selection cancelled"),
            "Area selection cancelled. Nothing was shared."
        );
        assert!(friendly_error("wl-copy failed").contains("Couldn’t copy"));
        assert!(friendly_error("screen capture failed").contains("Couldn’t capture"));
    }

    fn test_monitor(width: u32, height: u32, scale: f32, reserved_top: i32) -> Monitor {
        Monitor {
            id: 0,
            name: "eDP-1".into(),
            description: String::new(),
            width,
            height,
            x: 0,
            y: 0,
            scale,
            refresh_rate: 60.,
            transform: 0,
            focused: true,
            reserved: [0, reserved_top, 0, 0],
            active_workspace: crate::hypr::Workspace {
                id: 1,
                name: "1".into(),
            },
            special_workspace: crate::hypr::Workspace {
                id: 0,
                name: String::new(),
            },
        }
    }

    #[test]
    fn picker_fits_a_1080p_laptop_at_1_6_scale() {
        let monitor = test_monitor(1920, 1080, 1.6, 26);
        let (usable_w, usable_h) = monitor.usable_size();
        let size = picker_size_for_monitor(&monitor);
        assert!(usable_h < 650.0, "logical usable height is {usable_h}");
        assert!(size.width <= px(usable_w - PICKER_MARGIN));
        assert!(size.height <= px(usable_h - PICKER_MARGIN));
        assert!(size.height <= px(PICKER_MAX_H));
        assert!(size.height < px(usable_h));
    }
}
