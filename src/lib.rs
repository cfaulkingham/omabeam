//! Hyprland tile sharing: compositor snapshot, XDPH picker protocol, and capture helpers.

pub mod app;
pub mod capture;
pub mod hypr;
pub mod layout;
pub mod live;
pub mod localsend;
pub mod portal;
pub mod qr;

pub use capture::{CaptureAction, CaptureRequest};
pub use hypr::{Client, Monitor, Snapshot, Workspace};
pub use layout::{RelativeRect, Tile};
pub use live::LiveStatus;
pub use portal::{PortalWindow, Selection, parse_window_list};
