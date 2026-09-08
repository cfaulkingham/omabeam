//! Run in a Wayland session to inspect actual compositor pixels and session reuse.
use anyhow::{Context, Result, bail};
use omabeam_capture::{CaptureSession, CaptureTarget, Rect, Region, pick_region};
use std::time::Duration;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let (target, path) = match args.first().map(String::as_str) {
        Some("output") if args.len() == 3 => (CaptureTarget::Output(args[1].clone()), &args[2]),
        Some("window") if args.len() == 3 => (CaptureTarget::Toplevel(args[1].clone()), &args[2]),
        Some("region") if args.len() == 7 => (
            CaptureTarget::Region(Region {
                output: args[1].clone(),
                rect: Rect {
                    x: args[2].parse()?,
                    y: args[3].parse()?,
                    width: args[4].parse()?,
                    height: args[5].parse()?,
                },
            }),
            &args[6],
        ),
        Some("select") if args.len() == 2 => {
            let region = pick_region()?;
            eprintln!(
                "Selected {} at {},{} {}x{}",
                region.output, region.rect.x, region.rect.y, region.rect.width, region.rect.height
            );
            (CaptureTarget::Region(region), &args[1])
        }
        _ => bail!(
            "usage: smoke output NAME PATH | window STABLE_ID PATH | region NAME X Y W H PATH | select PATH"
        ),
    };
    let mut session = CaptureSession::new(target)?;
    let image = session.capture()?;
    std::fs::write(path, image.png()?)?;
    eprintln!(
        "Captured {}x{} (logical {}x{}) to {path}",
        image.image.width(),
        image.image.height(),
        image.logical_width,
        image.logical_height
    );
    std::fs::write(
        std::path::Path::new(path).with_extension("jpg"),
        image.jpeg(55)?,
    )?;
    for _ in 0..3 {
        // An idle compositor may return no damage. Keep and reuse the request.
        let _ = session
            .next_frame(Duration::from_millis(80))
            .context("subsequent capture")?;
    }
    Ok(())
}
