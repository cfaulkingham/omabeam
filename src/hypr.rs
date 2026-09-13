use anyhow::{Context, Result, ensure};
use serde::Deserialize;

pub mod desktop;
mod ipc;

use crate::portal::PortalWindow;

const APP_CLASS: &str = "omabeam";

#[derive(Debug, Clone, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    pub address: String,
    pub class: String,
    pub title: String,
    pub workspace: Workspace,
    pub monitor: i64,
    pub floating: bool,
    pub mapped: bool,
    pub hidden: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub focus_history_id: i64,
    pub portal_id: Option<u64>,
    pub stable_id: String,
}

impl Client {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at[0],
            y: self.at[1],
            w: self.size[0].max(1),
            h: self.size[1].max(1),
        }
    }

    pub fn is_picker(&self) -> bool {
        self.class == APP_CLASS
    }

    pub fn visible_on_workspace(&self, workspace_id: i32) -> bool {
        self.mapped && !self.hidden && !self.is_picker() && self.workspace.id == workspace_id
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub scale: f32,
    pub refresh_rate: f32,
    pub transform: u32,
    pub focused: bool,
    pub reserved: [i32; 4],
    pub active_workspace: Workspace,
    pub special_workspace: Workspace,
}

impl Monitor {
    pub fn logical_size(&self) -> (f32, f32) {
        let scale = self.scale.max(0.01);
        let (width, height) = if self.transform % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        (width as f32 / scale, height as f32 / scale)
    }

    /// Logical pixels left after reserved bar/insets. Used so the floating
    /// picker can size itself to the focused screen instead of overflowing it.
    pub fn usable_size(&self) -> (f32, f32) {
        let (width, height) = self.logical_size();
        let [left, top, right, bottom] = self.reserved;
        (
            (width - (left + right) as f32).max(1.0),
            (height - (top + bottom) as f32).max(1.0),
        )
    }

    pub fn canvas(&self) -> Rect {
        let (w, h) = self.logical_size();
        Rect {
            x: self.x,
            y: self.y,
            w: w.round().max(1.0) as i32,
            h: h.round().max(1.0) as i32,
        }
    }

    pub fn label(&self) -> String {
        if self.description.trim().is_empty() {
            self.name.clone()
        } else {
            format!("{} · {}", self.name, self.description)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub clients: Vec<Client>,
    pub monitors: Vec<Monitor>,
    pub workspaces: Vec<Workspace>,
    pub active_workspace: Workspace,
}

impl Snapshot {
    pub fn load() -> Result<Self> {
        let ipc = ipc::Ipc::from_env()?;
        let clients = parse_clients(&ipc.query("clients")?)?;
        let monitors = parse_monitors(&ipc.query("monitors")?)?;
        let workspaces = parse_workspaces(&ipc.query("workspaces")?)?;
        let active = parse_active_workspace(&ipc.query("activeworkspace")?)?;
        Ok(Self {
            clients,
            monitors,
            workspaces,
            active_workspace: active,
        })
    }

    pub fn with_portal_windows(mut self, portal: &[PortalWindow]) -> Self {
        for client in &mut self.clients {
            client.portal_id = match_portal_id(client, portal);
        }
        self
    }

    pub fn focused_monitor(&self) -> Option<&Monitor> {
        self.monitors
            .iter()
            .find(|monitor| monitor.focused)
            .or_else(|| self.monitors.first())
    }

    pub fn monitor_by_id(&self, id: i64) -> Option<&Monitor> {
        self.monitors.iter().find(|monitor| monitor.id == id)
    }

    pub fn monitor_by_name(&self, name: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|monitor| monitor.name == name)
    }

    pub fn visible_clients(&self) -> impl Iterator<Item = &Client> {
        self.clients
            .iter()
            .filter(|client| client.mapped && !client.hidden && !client.is_picker())
    }

    pub fn tiles_on(&self, workspace_id: i32) -> Vec<&Client> {
        let mut tiles: Vec<&Client> = self
            .visible_clients()
            .filter(|client| client.workspace.id == workspace_id)
            .collect();
        tiles.sort_by_key(|client| (client.at[1], client.at[0], client.focus_history_id));
        tiles
    }

    pub fn listed_windows(&self) -> Vec<&Client> {
        let mut windows: Vec<&Client> = self.visible_clients().collect();
        windows.sort_by_key(|client| (client.workspace.id, client.focus_history_id, &client.class));
        windows
    }
}

/// Query compositor metadata directly; also used by `omabeam --hypr`.
pub fn query(command: &str) -> Result<String> {
    ensure!(
        matches!(
            command,
            "clients"
                | "monitors"
                | "workspaces"
                | "activeworkspace"
                | "activewindow"
                | "version"
                | "configerrors"
        ),
        "unsupported Hyprland query: {command}"
    );
    let reply = ipc::Ipc::from_env()?.query(command)?;
    serde_json::from_str::<serde_json::Value>(&reply)
        .with_context(|| format!("invalid Hyprland {command} JSON"))?;
    Ok(reply)
}

pub fn reload() -> Result<()> {
    ipc::Ipc::from_env()?.command("reload")
}

pub fn dispatch_lua(expr: &str) -> Result<()> {
    ipc::Ipc::from_env()?.command(&format!("dispatch {expr}"))
}

pub fn hide_picker() {
    let selector = picker_selector();
    if let Some(expr) = clear_screen_mask_lua(&selector) {
        let _ = dispatch_lua(&expr);
    }
    dispatch_window(
        &selector,
        r#"hl.dsp.window.resize({ x = 1, y = 1, relative = false, window = {window} })"#,
    );
    dispatch_window(
        &selector,
        r#"hl.dsp.window.move({ x = -5000, y = -5000, relative = false, window = {window} })"#,
    );
    if let Some(expr) = hide_picker_lua(&selector) {
        let _ = dispatch_lua(&expr);
    }
    hide_omabeam_overlay();
}

pub fn show_picker() {
    let selector = picker_selector();
    let Some(window) = lua_window_selector(&selector) else {
        return;
    };
    let workspace = lua_quote(&current_regular_workspace()).unwrap_or_else(|| r#""1""#.into());
    let _ = dispatch_lua(&format!(
        "hl.dsp.window.move({{ workspace = {workspace}, window = {window} }})"
    ));
    hide_omabeam_overlay();
}

pub fn activate_existing_picker() -> bool {
    let Ok(snapshot) = Snapshot::load() else {
        return false;
    };
    let Some(client) = snapshot.clients.iter().find(|client| client.is_picker()) else {
        return false;
    };
    let selector = address_selector(client).unwrap_or_else(|| "class:omabeam".into());
    let live = crate::live::current_status().is_some();
    if live {
        hide_picker();
        release_live_focus();
        return true;
    }
    if client.workspace.name.contains("special") {
        if !omabeam_overlay_visible(&snapshot) {
            let _ = dispatch_lua(r#"hl.dsp.workspace.toggle_special("omabeam")"#);
        }
        restore_picker();
    } else {
        dispatch_window(&selector, r#"hl.dsp.focus({ window = {window} })"#);
    }
    true
}

fn picker_selector() -> String {
    Snapshot::load()
        .ok()
        .and_then(|snapshot| {
            snapshot
                .clients
                .iter()
                .find(|client| client.is_picker())
                .and_then(address_selector)
        })
        .unwrap_or_else(|| "class:omabeam".into())
}

fn address_selector(client: &Client) -> Option<String> {
    let addr = parse_address(&client.address)?;
    Some(format!("address:0x{addr:x}"))
}

fn lua_quote(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 128 || value.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out.push('"');
    Some(out)
}

fn lua_window_selector(selector: &str) -> Option<String> {
    if selector == "class:omabeam" {
        return Some(r#""class:omabeam""#.into());
    }
    let hex = selector.strip_prefix("address:0x")?;
    if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!(r#""address:0x{hex}""#))
}

fn dispatch_window(selector: &str, template: &str) {
    let Some(window) = lua_window_selector(selector) else {
        return;
    };
    let _ = dispatch_lua(&template.replace("{window}", &window));
}

fn current_regular_workspace() -> String {
    Snapshot::load()
        .ok()
        .and_then(|snapshot| {
            snapshot
                .focused_monitor()
                .map(|monitor| monitor.active_workspace.name.clone())
                .filter(|name| !name.contains("special"))
                .or_else(|| {
                    let name = snapshot.active_workspace.name.clone();
                    (!name.contains("special")).then_some(name)
                })
        })
        .unwrap_or_else(|| "1".into())
}

fn hide_omabeam_overlay() {
    let Ok(snapshot) = Snapshot::load() else {
        return;
    };
    if omabeam_overlay_visible(&snapshot) {
        let _ = dispatch_lua(r#"hl.dsp.workspace.toggle_special("omabeam")"#);
    }
}

fn omabeam_overlay_visible(snapshot: &Snapshot) -> bool {
    snapshot
        .monitors
        .iter()
        .any(|monitor| monitor.special_workspace.name.contains("omabeam"))
}

pub fn hide_picker_lua(selector: &str) -> Option<String> {
    let window = lua_window_selector(selector)?;
    Some(format!(
        r#"hl.dsp.window.move({{ workspace = "special:omabeam", follow = false, window = {window} }})"#
    ))
}

fn clear_screen_mask_lua(selector: &str) -> Option<String> {
    let window = lua_window_selector(selector)?;
    Some(format!(
        r#"hl.dsp.window.set_prop({{ prop = "no_screen_share", value = "0", window = {window} }})"#
    ))
}

pub fn release_live_focus() {
    let _ = dispatch_lua(r#"hl.dsp.focus({ last = true })"#);
    let class = query("activewindow")
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|value| {
            value
                .get("class")
                .and_then(|class| class.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if class == "omabeam" || class.is_empty() {
        let _ = dispatch_lua(r#"hl.dsp.window.cycle_next({ tiled = true })"#);
    }
}

pub fn restore_picker() {
    show_picker();
    let selector = picker_selector();
    dispatch_window(
        &selector,
        r#"hl.dsp.window.resize({ x = 980, y = 680, relative = false, window = {window} })"#,
    );
    dispatch_window(&selector, r#"hl.dsp.window.center({ window = {window} })"#);
}

pub fn parse_address(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "0" || trimmed.eq_ignore_ascii_case("0x0") {
        return None;
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16).ok();
    }
    trimmed
        .parse::<u64>()
        .ok()
        .or_else(|| u64::from_str_radix(trimmed, 16).ok())
}

/// Preserve identity across refreshes, but clear a closed/replaced selection.
pub fn retain_selected_window(old: Option<&Client>, snapshot: &Snapshot) -> Option<String> {
    let old = old?;
    snapshot
        .visible_clients()
        .find(|client| {
            if !old.stable_id.is_empty() {
                client.stable_id == old.stable_id
            } else {
                client.address == old.address && client.class == old.class
            }
        })
        .map(|client| client.address.clone())
}

fn match_portal_id(client: &Client, portal: &[PortalWindow]) -> Option<u64> {
    fn unique<'a>(mut windows: impl Iterator<Item = &'a PortalWindow>) -> Option<u64> {
        let first = windows.next()?;
        windows.next().is_none().then_some(first.id)
    }
    let address = parse_address(&client.address);
    if let Some(address) = address {
        let mut matches = portal
            .iter()
            .filter(|w| w.address == Some(address))
            .peekable();
        if matches.peek().is_some() {
            return unique(matches);
        }
    }
    // A conflicting known address must never match on title alone.
    let eligible = || {
        portal
            .iter()
            .filter(|w| w.address.is_none() && w.class == client.class)
    };
    let mut exact = eligible().filter(|w| w.title == client.title).peekable();
    if exact.peek().is_some() {
        return unique(exact);
    }
    unique(eligible().filter(|w| titles_close(&w.title, &client.title)))
}

fn titles_close(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    !left.is_empty() && (left == right || left.starts_with(right) || right.starts_with(left))
}

#[derive(Deserialize)]
struct RawWorkspaceRef {
    id: i32,
    name: String,
}

#[derive(Deserialize)]
struct RawClient {
    address: String,
    class: Option<String>,
    title: Option<String>,
    workspace: RawWorkspaceRef,
    monitor: i64,
    floating: bool,
    mapped: Option<bool>,
    hidden: Option<bool>,
    at: [i32; 2],
    size: [i32; 2],
    #[serde(rename = "focusHistoryID", default)]
    focus_history_id: i64,
    #[serde(rename = "stableId", default)]
    stable_id: String,
}

#[derive(Deserialize)]
struct RawMonitor {
    id: i64,
    name: String,
    description: Option<String>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    scale: f32,
    #[serde(rename = "refreshRate", default)]
    refresh_rate: Option<f32>,
    #[serde(default)]
    transform: u32,
    focused: bool,
    reserved: Option<[i32; 4]>,
    #[serde(rename = "activeWorkspace")]
    active_workspace: RawWorkspaceRef,
    #[serde(rename = "specialWorkspace", default)]
    special_workspace: Option<RawWorkspaceRef>,
}

#[derive(Deserialize)]
struct RawWorkspace {
    id: i32,
    name: String,
}

pub fn parse_clients(json: &str) -> Result<Vec<Client>> {
    let raw: Vec<RawClient> =
        serde_json::from_str(json).context("invalid Hyprland clients JSON")?;
    Ok(raw
        .into_iter()
        .map(|client| Client {
            address: client.address,
            class: client.class.unwrap_or_default(),
            title: client.title.unwrap_or_default(),
            workspace: Workspace {
                id: client.workspace.id,
                name: client.workspace.name,
            },
            monitor: client.monitor,
            floating: client.floating,
            mapped: client.mapped.unwrap_or(true),
            hidden: client.hidden.unwrap_or(false),
            at: client.at,
            size: client.size,
            focus_history_id: client.focus_history_id,
            portal_id: None,
            stable_id: client.stable_id,
        })
        .collect())
}

pub fn parse_monitors(json: &str) -> Result<Vec<Monitor>> {
    let raw: Vec<RawMonitor> =
        serde_json::from_str(json).context("invalid Hyprland monitors JSON")?;
    Ok(raw
        .into_iter()
        .map(|monitor| Monitor {
            id: monitor.id,
            name: monitor.name,
            description: monitor.description.unwrap_or_default(),
            width: monitor.width,
            height: monitor.height,
            x: monitor.x,
            y: monitor.y,
            scale: monitor.scale,
            refresh_rate: monitor
                .refresh_rate
                .filter(|rate| *rate > 0.0)
                .unwrap_or(60.0),
            transform: monitor.transform,
            focused: monitor.focused,
            reserved: monitor.reserved.unwrap_or([0, 0, 0, 0]),
            active_workspace: Workspace {
                id: monitor.active_workspace.id,
                name: monitor.active_workspace.name,
            },
            special_workspace: monitor
                .special_workspace
                .map(|workspace| Workspace {
                    id: workspace.id,
                    name: workspace.name,
                })
                .unwrap_or(Workspace {
                    id: 0,
                    name: String::new(),
                }),
        })
        .collect())
}

pub fn parse_workspaces(json: &str) -> Result<Vec<Workspace>> {
    let mut raw: Vec<RawWorkspace> =
        serde_json::from_str(json).context("invalid Hyprland workspaces JSON")?;
    raw.sort_by_key(|workspace| workspace.id);
    Ok(raw
        .into_iter()
        .map(|workspace| Workspace {
            id: workspace.id,
            name: workspace.name,
        })
        .collect())
}

pub fn parse_active_workspace(json: &str) -> Result<Workspace> {
    let raw: RawWorkspace =
        serde_json::from_str(json).context("invalid Hyprland activeworkspace JSON")?;
    Ok(Workspace {
        id: raw.id,
        name: raw.name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> Client {
        parse_clients(r#"[{"address":"0x1","mapped":true,"hidden":false,"at":[0,0],"size":[100,100],"workspace":{"id":1,"name":"1"},"monitor":0,"class":"foot","title":"term","stableId":"window-1","floating":false,"focusHistoryID":0}]"#).unwrap().remove(0)
    }

    #[test]
    fn portal_matching_rejects_ambiguous_or_conflicting_identities() {
        let client = test_client();
        let one = PortalWindow {
            id: 1,
            class: "foot".into(),
            title: "term".into(),
            address: None,
        };
        let mut two = one.clone();
        two.id = 2;
        assert_eq!(match_portal_id(&client, &[one.clone()]), Some(1));
        assert_eq!(match_portal_id(&client, &[one.clone(), two.clone()]), None);
        two.address = Some(2);
        assert_eq!(match_portal_id(&client, &[two.clone()]), None);
        two.address = Some(1);
        assert_eq!(match_portal_id(&client, &[one.clone(), two]), Some(2));
        let mut fuzzy = one.clone();
        fuzzy.title = "term — editor".into();
        let mut other = fuzzy.clone();
        other.id = 3;
        assert_eq!(match_portal_id(&client, &[fuzzy.clone(), other]), None);
        assert_eq!(match_portal_id(&client, &[one, fuzzy]), Some(1));
    }

    #[test]
    fn refreshing_selection_tracks_moves_and_clears_closed_or_replaced_windows() {
        let old = test_client();
        let mut snapshot = Snapshot {
            clients: vec![old.clone()],
            monitors: vec![],
            workspaces: vec![],
            active_workspace: old.workspace.clone(),
        };
        snapshot.clients[0].at = [300, 200];
        snapshot.clients[0].size = [200, 150];
        assert_eq!(
            retain_selected_window(Some(&old), &snapshot),
            Some(old.address.clone())
        );
        snapshot.clients[0].stable_id = "replacement".into();
        assert_eq!(retain_selected_window(Some(&old), &snapshot), None);
        snapshot.clients.clear();
        assert_eq!(retain_selected_window(Some(&old), &snapshot), None);
        assert_eq!(retain_selected_window(None, &snapshot), None);
    }

    #[test]
    fn parses_client_snapshot() {
        let json = r#"[{
            "address": "0x55a8474cd360",
            "mapped": true,
            "hidden": false,
            "at": [12, 38],
            "size": [749, 814],
            "workspace": {"id": 1, "name": "1"},
            "floating": false,
            "monitor": 0,
            "class": "foot",
            "title": "term",
            "focusHistoryID": 1
        }]"#;
        let clients = parse_clients(json).unwrap();
        assert_eq!(clients[0].class, "foot");
        assert_eq!(
            clients[0].rect(),
            Rect {
                x: 12,
                y: 38,
                w: 749,
                h: 814
            }
        );
    }

    #[test]
    fn parses_scaled_monitor_canvas() {
        let json = r#"[{
            "id": 0,
            "name": "HDMI-A-1",
            "description": "ASUS",
            "width": 1920,
            "height": 1080,
            "x": 0,
            "y": 0,
            "scale": 1.25,
            "focused": true,
            "reserved": [0, 26, 0, 0],
            "activeWorkspace": {"id": 1, "name": "1"}
        }]"#;
        let monitors = parse_monitors(json).unwrap();
        let canvas = monitors[0].canvas();
        assert_eq!(canvas.w, 1536);
        assert_eq!(canvas.h, 864);
        assert_eq!(monitors[0].usable_size(), (1536.0, 838.0));
        assert_eq!(monitors[0].special_workspace.name, "");
    }

    #[test]
    fn parses_special_workspace_overlay() {
        let json = r#"[{
            "id": 0,
            "name": "HDMI-A-1",
            "description": "ASUS",
            "width": 1920,
            "height": 1080,
            "x": 0,
            "y": 0,
            "scale": 1.25,
            "focused": true,
            "reserved": [0, 26, 0, 0],
            "activeWorkspace": {"id": 1, "name": "1"},
            "specialWorkspace": {"id": -98, "name": "special:omabeam"}
        }]"#;
        let monitors = parse_monitors(json).unwrap();
        assert_eq!(monitors[0].special_workspace.name, "special:omabeam");
        assert!(monitors[0].special_workspace.id < 0);
        let snapshot = Snapshot {
            clients: Vec::new(),
            monitors,
            workspaces: Vec::new(),
            active_workspace: Workspace {
                id: 1,
                name: "1".into(),
            },
        };
        assert!(omabeam_overlay_visible(&snapshot));
    }

    #[test]
    fn hide_picker_lua_stays_off_the_output() {
        let expr = hide_picker_lua("class:omabeam").unwrap();
        assert!(expr.contains("follow = false"), "{expr}");
        assert!(expr.contains(r#"workspace = "special:omabeam""#), "{expr}");
        assert!(!expr.contains("follow = true"), "{expr}");
        assert!(hide_picker_lua(r#"address:0xabc"; os.execute(1)"#).is_none());
        assert_eq!(
            lua_window_selector("address:0xabc"),
            Some(r#""address:0xabc""#.into())
        );
        assert_eq!(lua_quote(r#"1"; evil"#).as_deref(), Some(r#""1\"; evil""#));
    }

    #[test]
    fn parses_hex_and_decimal_addresses() {
        assert_eq!(parse_address("0x55a8474cd360"), Some(0x55a8474cd360));
        assert_eq!(parse_address("0"), None);
        assert_eq!(parse_address("42"), Some(42));
    }
}
