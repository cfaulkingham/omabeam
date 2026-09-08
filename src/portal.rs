use crate::hypr::{Client, Monitor, Rect};

/// A window advertised by xdg-desktop-portal-hyprland via `XDPH_WINDOW_SHARING_LIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalWindow {
    pub id: u64,
    pub class: String,
    pub title: String,
    pub address: Option<u64>,
}

/// Selection returned on stdout for `hyprland-share-picker` compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Screen {
        name: String,
    },
    Window {
        id: u64,
    },
    Region {
        output: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
}

impl Selection {
    pub fn encode(&self, allow_token: bool) -> String {
        let flag = if allow_token { "r" } else { "" };
        let body = match self {
            Self::Screen { name } => format!("screen:{name}"),
            Self::Window { id } => format!("window:{id}"),
            Self::Region { output, x, y, w, h } => {
                format!("region:{output}@{x},{y},{w},{h}")
            }
        };
        format!("[SELECTION]{flag}/{body}")
    }
}

/// Parse `XDPH_WINDOW_SHARING_LIST`.
///
/// Format, repeated:
/// `{id}[HC>]{class}[HT>]{title}[HE>]{address}[HA>]`
pub fn parse_window_list(raw: &str) -> Vec<PortalWindow> {
    let mut windows = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        let Some(id_end) = rest.find("[HC>]") else {
            break;
        };
        let Some(class_end) = rest.find("[HT>]") else {
            break;
        };
        let Some(title_end) = rest.find("[HE>]") else {
            break;
        };
        let Some(addr_end) = rest.find("[HA>]") else {
            break;
        };
        if id_end > class_end || class_end > title_end || title_end > addr_end {
            break;
        }
        let id_str = &rest[..id_end];
        let class = rest[id_end + 5..class_end].to_string();
        let title = rest[class_end + 5..title_end].to_string();
        let address_raw = &rest[title_end + 5..addr_end];
        if let Ok(id) = id_str.parse::<u64>() {
            windows.push(PortalWindow {
                id,
                class,
                title,
                address: crate::hypr::parse_address(address_raw),
            });
        }
        rest = &rest[addr_end + 5..];
    }
    windows
}

pub fn region_for_client(client: &Client, monitor: &Monitor) -> Selection {
    let rect = client.rect();
    let local = output_local(&rect, monitor);
    Selection::Region {
        output: monitor.name.clone(),
        x: local.x,
        y: local.y,
        w: local.w,
        h: local.h,
    }
}

fn output_local(rect: &Rect, monitor: &Monitor) -> Rect {
    Rect {
        x: (rect.x - monitor.x).max(0),
        y: (rect.y - monitor.y).max(0),
        w: rect.w,
        h: rect.h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xdph_window_list() {
        let raw =
            "12[HC>]foot[HT>]term title[HE>]0x55a8474cd360[HA>]34[HC>]chromium[HT>]Wiki[HE>]0[HA>]";
        let windows = parse_window_list(raw);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, 12);
        assert_eq!(windows[0].class, "foot");
        assert_eq!(windows[0].title, "term title");
        assert_eq!(windows[0].address, Some(0x55a8474cd360));
        assert_eq!(windows[1].id, 34);
        assert_eq!(windows[1].address, None);
    }

    #[test]
    fn encodes_picker_protocol() {
        assert_eq!(
            Selection::Screen {
                name: "HDMI-A-1".into()
            }
            .encode(true),
            "[SELECTION]r/screen:HDMI-A-1"
        );
        assert_eq!(
            Selection::Window { id: 42 }.encode(false),
            "[SELECTION]/window:42"
        );
        assert_eq!(
            Selection::Region {
                output: "HDMI-A-1".into(),
                x: 10,
                y: 20,
                w: 300,
                h: 200
            }
            .encode(true),
            "[SELECTION]r/region:HDMI-A-1@10,20,300,200"
        );
    }
}
