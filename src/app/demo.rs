//! Deterministic native UI review on machines without Hyprland. All actions
//! that could capture, copy, save, or stream are disabled in demo mode.
use crate::hypr::{Client, Monitor, Snapshot, Workspace};

pub(super) fn snapshot() -> Snapshot {
    let workspace = Workspace {
        id: 2,
        name: "2".into(),
    };
    let clients = [
        ("Editor", "Source preview", [0, 0], [1280, 1440]),
        ("Browser", "Capture documentation", [1280, 0], [1280, 720]),
        ("Terminal", "cargo test", [1280, 720], [1280, 720]),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (class, title, at, size))| Client {
        address: format!("demo-{i}"),
        class: class.into(),
        title: title.into(),
        workspace: workspace.clone(),
        monitor: 0,
        floating: false,
        mapped: true,
        hidden: false,
        at,
        size,
        focus_history_id: i as i64,
        portal_id: None,
        stable_id: format!("demo-{i}"),
    })
    .collect();
    Snapshot {
        clients,
        monitors: vec![Monitor {
            id: 0,
            name: "DEMO-1".into(),
            description: "Example display".into(),
            width: 2560,
            height: 1440,
            x: 0,
            y: 0,
            scale: 1.,
            focused: true,
            reserved: [0; 4],
            active_workspace: workspace.clone(),
            special_workspace: Workspace {
                id: 0,
                name: String::new(),
            },
        }],
        workspaces: vec![workspace.clone()],
        active_workspace: workspace,
    }
}
