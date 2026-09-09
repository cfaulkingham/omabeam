use anyhow::{Context, Result, bail};
use chrono::Local;
use omabeam_capture::{CaptureSession, CaptureTarget, Region};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::hypr::{Client, Monitor, Rect, hide_picker};
use crate::portal::Selection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureAction {
    Copy,
    Save,
    Share,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CaptureRequest {
    Window(Rect),
    Toplevel(String),
    Output(String),
    Region(Selection),
}

impl CaptureRequest {
    pub fn target(&self) -> Result<CaptureTarget> {
        fn rect(x: i32, y: i32, w: i32, h: i32) -> Result<omabeam_capture::Rect> {
            if w <= 0 || h <= 0 {
                bail!("screen capture requires a nonempty region");
            }
            Ok(omabeam_capture::Rect {
                x,
                y,
                width: w as u32,
                height: h as u32,
            })
        }
        Ok(match self {
            Self::Window(r) => CaptureTarget::DesktopRect(rect(r.x, r.y, r.w, r.h)?),
            Self::Toplevel(id) => CaptureTarget::Toplevel(id.clone()),
            Self::Output(name) => CaptureTarget::Output(name.clone()),
            Self::Region(Selection::Region { output, x, y, w, h }) => {
                CaptureTarget::Region(Region {
                    output: output.clone(),
                    rect: rect(*x, *y, *w, *h)?,
                })
            }
            Self::Region(_) => bail!("screen capture expected a region selection"),
        })
    }
}

pub fn pick_region() -> Result<Selection> {
    hide_picker();
    let region = omabeam_capture::pick_region()?;
    Ok(Selection::Region {
        output: region.output,
        x: region.rect.x,
        y: region.rect.y,
        w: region.rect.width.try_into()?,
        h: region.rect.height.try_into()?,
    })
}

pub fn capture(request: &CaptureRequest, action: CaptureAction) -> Result<Option<PathBuf>> {
    hide_picker();
    match action {
        CaptureAction::Copy => {
            copy_image(request)?;
            Ok(None)
        }
        CaptureAction::Save | CaptureAction::Share => {
            let path = save_image(request)?;
            if action == CaptureAction::Share {
                share_file(&path)?;
            }
            Ok(Some(path))
        }
    }
}

pub fn request_for_client(client: &Client) -> Result<CaptureRequest> {
    if client.stable_id.is_empty() {
        bail!(
            "This window cannot be captured separately. Choose Area to explicitly share its visible screen region."
        );
    }
    Ok(CaptureRequest::Toplevel(client.stable_id.clone()))
}

pub fn request_for_monitor(monitor: &Monitor) -> CaptureRequest {
    CaptureRequest::Output(monitor.name.clone())
}

fn png_image(request: &CaptureRequest) -> Result<Vec<u8>> {
    CaptureSession::new(request.target()?)?
        .capture()?
        .png()
        .context("screen capture PNG encoding failed")
}

fn copy_image(request: &CaptureRequest) -> Result<()> {
    let png = png_image(request)?;
    let mut child = Command::new("/usr/bin/wl-copy")
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run wl-copy")?;
    let write = child
        .stdin
        .take()
        .context("wl-copy has no stdin")?
        .write_all(&png)
        .context("failed to write screenshot to wl-copy");
    // Close stdin and reap the child even if the clipboard receiver exits early.
    let status = child.wait().context("failed to wait for wl-copy")?;
    write?;
    if !status.success() {
        bail!("wl-copy failed");
    }
    Ok(())
}

fn save_image(request: &CaptureRequest) -> Result<PathBuf> {
    let png = png_image(request)?;
    let path = screenshot_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, png).with_context(|| format!("failed to save {}", path.display()))?;
    Ok(path)
}

fn share_file(path: &Path) -> Result<()> {
    let status = Command::new("omarchy")
        .args(["share", "file"])
        .arg(path)
        .status()
        .context("failed to run omarchy share")?;
    if !status.success() {
        bail!("omarchy share failed");
    }
    Ok(())
}

fn screenshot_path() -> Result<PathBuf> {
    let stamp = Local::now().format("%Y-%m-%d-%H%M%S");
    let dir = pictures_dir()?.join("omabeam");
    Ok(dir.join(format!("omabeam-{stamp}.png")))
}

fn pictures_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("OMARCHY_SCREENSHOT_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("XDG_PICTURES_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Pictures"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_output_relative_regions_separate_from_layout_coordinates() {
        let target = CaptureRequest::Region(Selection::Region {
            output: "DP-2".into(),
            x: 20,
            y: 30,
            w: 100,
            h: 50,
        })
        .target()
        .unwrap();
        assert_eq!(
            target,
            CaptureTarget::Region(Region {
                output: "DP-2".into(),
                rect: omabeam_capture::Rect {
                    x: 20,
                    y: 30,
                    width: 100,
                    height: 50
                }
            })
        );
        assert!(matches!(
            CaptureRequest::Window(Rect {
                x: -1920,
                y: 30,
                w: 100,
                h: 50
            })
            .target()
            .unwrap(),
            CaptureTarget::DesktopRect(omabeam_capture::Rect { x: -1920, .. })
        ));
    }

    #[test]
    fn preserves_true_window_capture_and_rejects_invalid_regions() {
        assert_eq!(
            CaptureRequest::Toplevel("18000026".into())
                .target()
                .unwrap(),
            CaptureTarget::Toplevel("18000026".into())
        );
        assert!(
            CaptureRequest::Region(Selection::Screen {
                name: "DP-1".into()
            })
            .target()
            .is_err()
        );
        assert!(
            CaptureRequest::Window(Rect {
                x: 0,
                y: 0,
                w: -1,
                h: 100
            })
            .target()
            .is_err()
        );
    }
}
