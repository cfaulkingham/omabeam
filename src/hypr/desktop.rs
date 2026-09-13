//! A session-owned Hyprland output; no persistent compositor configuration.
use super::{Monitor, ipc::Ipc, parse_monitors};
use crate::live::status;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Right,
    Left,
    Above,
    Below,
}

impl Position {
    pub const ALL: [Self; 4] = [Self::Right, Self::Left, Self::Above, Self::Below];
    pub fn label(self) -> &'static str {
        match self {
            Self::Right => "Right",
            Self::Left => "Left",
            Self::Above => "Above",
            Self::Below => "Below",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.label().eq_ignore_ascii_case(value))
            .context("display position must be right, left, above, or below")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopConfig {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub position: Position,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            scale: 1,
            position: Position::Right,
        }
    }
}

impl DesktopConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (640..=3840).contains(&self.width)
                && (480..=3840).contains(&self.height)
                && u64::from(self.width) * u64::from(self.height) <= 3840 * 2160,
            "extended display must be at least 640×480 and no larger than 4K (landscape or portrait)"
        );
        ensure!(matches!(self.scale, 1 | 2), "desktop scale must be 1 or 2");
        ensure!(
            self.width % self.scale == 0 && self.height % self.scale == 0,
            "display dimensions must divide evenly by desktop scale"
        );
        Ok(())
    }

    pub fn from_args(args: &[String]) -> Result<Self> {
        ensure!(
            args.len() == 4,
            "extend needs WIDTH HEIGHT SCALE POSITION, for example: extend 1920 1080 1 right"
        );
        let value = Self {
            width: args[0].parse().context("invalid display width")?,
            height: args[1].parse().context("invalid display height")?,
            scale: args[2].parse().context("invalid desktop scale")?,
            position: Position::parse(&args[3])?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn placement(&self, monitors: &[Monitor]) -> Result<(i32, i32)> {
        self.validate()?;
        // Attach to the outermost display, so an existing monitor cannot overlap
        // the new one. Its edge stays reachable even with gaps in the layout.
        let rects: Vec<_> = monitors.iter().map(Monitor::canvas).collect();
        let edge = rects
            .iter()
            .max_by_key(|r| match self.position {
                Position::Right => i64::from(r.x) + i64::from(r.w),
                Position::Left => -i64::from(r.x),
                Position::Below => i64::from(r.y) + i64::from(r.h),
                Position::Above => -i64::from(r.y),
            })
            .context("extended desktop needs an active display to extend")?;
        let width = i64::from(self.width / self.scale);
        let height = i64::from(self.height / self.scale);
        let (x, y) = match self.position {
            Position::Right => (i64::from(edge.x) + i64::from(edge.w), i64::from(edge.y)),
            Position::Left => (i64::from(edge.x) - width, i64::from(edge.y)),
            Position::Below => (i64::from(edge.x), i64::from(edge.y) + i64::from(edge.h)),
            Position::Above => (i64::from(edge.x), i64::from(edge.y) - height),
        };
        Ok((
            x.try_into().context("desktop x is out of range")?,
            y.try_into().context("desktop y is out of range")?,
        ))
    }
}

fn output_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn hypr_number(value: f32) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 0.0005 {
        format!("{}", rounded as i32)
    } else {
        format!("{value:.4}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Freeze an output at its current layout coordinates. Omarchy's catch-all
/// `position = "auto"` would otherwise reflow it when a new display appears:
/// Hyprland places explicit monitors first, then shoves auto monitors to their
/// right, so an extra screen requested on the right becomes the origin.
fn pin_command(monitor: &Monitor) -> Result<String> {
    ensure!(
        output_name_is_safe(&monitor.name),
        "unrecognized display name"
    );
    Ok(format!(
        "eval hl.monitor({{ output = \"{}\", mode = \"{}x{}@{}\", position = \"{}x{}\", scale = {}, transform = {} }})",
        monitor.name,
        monitor.width,
        monitor.height,
        hypr_number(monitor.refresh_rate),
        monitor.x,
        monitor.y,
        hypr_number(monitor.scale),
        monitor.transform
    ))
}

fn extra_command(name: &str, config: &DesktopConfig, x: i32, y: i32) -> Result<String> {
    ensure!(output_name_is_safe(name), "unrecognized display name");
    Ok(format!(
        "eval hl.monitor({{ output = \"{name}\", mode = \"{}x{}@60\", position = \"{x}x{y}\", scale = {} }})",
        config.width, config.height, config.scale
    ))
}

fn pin_outputs(ipc: &Ipc, monitors: &[Monitor]) -> Result<()> {
    for monitor in monitors {
        ipc.command(&pin_command(monitor)?)
            .with_context(|| format!("Hyprland could not keep {} in place", monitor.name))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnedDisplay {
    name: String,
    instance: String,
}

impl OwnedDisplay {
    fn validate(&self) -> Result<()> {
        let suffix = self
            .name
            .strip_prefix("OMABEAM-")
            .context("unrecognized owned display name")?;
        ensure!(
            suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid owned display name"
        );
        ensure!(
            !self.instance.is_empty() && self.instance.len() <= 256,
            "invalid display session"
        );
        Ok(())
    }
}

pub struct VirtualDisplay {
    owned: OwnedDisplay,
    ipc: Ipc,
    cleanup: bool,
}

impl VirtualDisplay {
    /// Caller holds the session lock for this guard's complete lifetime.
    pub fn create(config: &DesktopConfig) -> Result<Self> {
        config.validate()?;
        let ipc = Ipc::from_env()?;
        let monitors = parse_monitors(&ipc.query("monitors")?)?;
        config.placement(&monitors)?;
        let name = format!("OMABEAM-{}", crate::live::random_token()?);
        ensure!(
            !parse_monitors(&ipc.query("monitors all")?)?
                .iter()
                .any(|m| m.name == name),
            "extended display name is already in use"
        );
        let owned = OwnedDisplay {
            name,
            instance: std::env::var("HYPRLAND_INSTANCE_SIGNATURE")?,
        };
        owned.validate()?;
        // Record intent before the first mutation. Even a lost acknowledgement
        // or a killed process can be recovered without guessing display names.
        status::write_display_state(&serde_json::to_vec(&owned)?)?;
        let guard = Self {
            owned,
            ipc,
            cleanup: true,
        };
        pin_outputs(&guard.ipc, &monitors)?;
        guard
            .ipc
            .command(&format!("output create headless {}", guard.name()))
            .context("Hyprland could not create the extended display")?;
        guard.resize(config)?;
        Ok(guard)
    }

    /// Reconfigure only this session's output. Exclude it when calculating
    /// placement so left/above displays remain attached after a size change.
    pub fn resize(&self, config: &DesktopConfig) -> Result<()> {
        config.validate()?;
        self.owned.validate()?;
        let monitors = parse_monitors(&self.ipc.query("monitors all")?)?;
        ensure!(
            monitors.iter().any(|m| m.name == self.name()),
            "extended display was removed"
        );
        let others: Vec<_> = parse_monitors(&self.ipc.query("monitors")?)?
            .into_iter()
            .filter(|m| m.name != self.name())
            .collect();
        pin_outputs(&self.ipc, &others)?;
        let (x, y) = config.placement(&others)?;
        self.ipc
            .command(&extra_command(self.name(), config, x, y)?)
            .context("Hyprland could not configure the extended display")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let monitors = parse_monitors(&self.ipc.query("monitors")?)?;
            if monitors.iter().any(|m| {
                m.name == self.name()
                    && m.width == config.width
                    && m.height == config.height
                    && (m.scale - config.scale as f32).abs() < 0.01
                    && m.x == x
                    && m.y == y
            }) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "Hyprland did not apply the requested extended display layout"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn name(&self) -> &str {
        &self.owned.name
    }

    fn remove(&mut self) -> Result<()> {
        self.cleanup = false;
        self.owned.validate()?;
        if parse_monitors(&self.ipc.query("monitors all")?)?
            .iter()
            .any(|m| m.name == self.name())
        {
            self.ipc
                .command(&format!("output remove {}", self.name()))?;
            ensure!(
                !parse_monitors(&self.ipc.query("monitors all")?)?
                    .iter()
                    .any(|m| m.name == self.name()),
                "Hyprland still reports the extended display after removal"
            );
        }
        status::clear_display_state();
        Ok(())
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if !self.cleanup {
            return;
        }
        if let Err(error) = self.remove() {
            eprintln!(
                "Could not remove extended display {}: {error:#}. Run omabeam --stop to retry cleanup.",
                self.name()
            );
        }
    }
}

/// Call only while holding the session lock, after the previous owner exited.
pub(crate) fn recover() -> Result<bool> {
    let Some(raw) = status::read_display_state()? else {
        return Ok(false);
    };
    let owned: OwnedDisplay =
        serde_json::from_slice(&raw).context("invalid extended display recovery file")?;
    owned.validate()?;
    let ipc = Ipc::for_instance(&owned.instance)?;
    if !ipc.socket_exists()? {
        // The compositor has exited; its virtual outputs no longer exist.
        status::clear_display_state();
        return Ok(true);
    }
    let mut guard = VirtualDisplay {
        owned,
        ipc,
        cleanup: true,
    };
    guard.remove()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, width: u32, height: u32, scale: f32, transform: u32) -> Monitor {
        parse_monitors(
            &serde_json::json!([{
                "id": 0, "name": "physical", "width": width, "height": height,
                "x": x, "y": y, "scale": scale, "transform": transform, "focused": true,
                "activeWorkspace": {"id": 1, "name": "1"}
            }])
            .to_string(),
        )
        .unwrap()
        .remove(0)
    }

    #[test]
    fn placement_attaches_to_outer_edge_without_overlapping_scaled_or_rotated_displays() {
        let monitors = vec![
            monitor(-1920, 100, 1920, 1080, 1., 0),
            monitor(0, 0, 3840, 2160, 2., 1),
        ];
        let mut config = DesktopConfig {
            width: 2560,
            height: 1440,
            scale: 2,
            position: Position::Right,
        };
        for (side, expected) in [
            (Position::Right, (1080, 0)),
            (Position::Left, (-3200, 100)),
            (Position::Above, (0, -720)),
            (Position::Below, (0, 1920)),
        ] {
            config.position = side;
            assert_eq!(config.placement(&monitors).unwrap(), expected);
        }
        assert!(config.placement(&[]).is_err());
    }

    #[test]
    fn pin_command_freezes_auto_monitors_at_their_current_layout_coordinates() {
        let physical = monitor(-1920, 100, 1920, 1080, 1.25, 1);
        assert_eq!(
            pin_command(&physical).unwrap(),
            r#"eval hl.monitor({ output = "physical", mode = "1920x1080@60", position = "-1920x100", scale = 1.25, transform = 1 })"#
        );
        let mut bad = physical;
        bad.name = "DP-1; output remove DP-1".into();
        assert!(pin_command(&bad).is_err());
        assert!(
            extra_command(
                "OMABEAM-0123456789abcdef0123456789abcdef",
                &DesktopConfig::default(),
                1920,
                0
            )
            .is_ok()
        );
        assert!(extra_command("DP-1;evil", &DesktopConfig::default(), 0, 0).is_err());
    }

    #[test]
    fn invalid_and_oversized_modes_never_reach_the_compositor() {
        for args in [
            ["1920", "1080", "0", "right"],
            ["1920", "1080", "3", "right"],
            ["0", "1080", "1", "right"],
            ["3840", "3840", "1", "right"],
            ["1921", "1080", "2", "right"],
            ["1920", "1080", "1", "right;exec"],
            ["-1920", "1080", "1", "right"],
        ] {
            assert!(
                DesktopConfig::from_args(&args.map(str::to_owned)).is_err(),
                "{args:?}"
            );
        }
        for args in [
            ["3840", "2160", "2", "left"],
            ["2160", "3840", "2", "above"],
        ] {
            assert!(DesktopConfig::from_args(&args.map(str::to_owned)).is_ok());
        }
    }

    #[test]
    fn only_unambiguous_owned_names_are_recoverable() {
        for name in [
            "DP-1",
            "HEADLESS-1",
            "OMABEAM-",
            "OMABEAM-../other",
            "OMABEAM-123; output remove DP-1",
        ] {
            assert!(
                OwnedDisplay {
                    name: name.into(),
                    instance: "test".into()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            OwnedDisplay {
                name: "OMABEAM-0123456789abcdef0123456789abcdef".into(),
                instance: "test".into()
            }
            .validate()
            .is_ok()
        );
    }
}
