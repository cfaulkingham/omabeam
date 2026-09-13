//! The media driver runs only in this disposable child, never in the GUI or
//! transport process. stdout is exclusively the framed protocol.
mod video;
use anyhow::{Context, Result, bail, ensure};
use omabeam_encoder::{Config, Reply, inspect_h264, read_json, write_json};
use std::io::{Read, Write};

fn run() -> Result<()> {
    ffmpeg_next::init()?;
    ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Error);
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let config: Config = read_json(&mut input)?;
    let mut pixels = vec![0; config.frame_len()?];
    let mut encoder: Option<video::Hardware> = None;
    loop {
        let mut force = [0];
        if input.read(&mut force)? == 0 {
            return Ok(());
        }
        ensure!(force[0] <= 1, "invalid frame request");
        let mut pts = [0; 8];
        input.read_exact(&mut pts)?;
        let pts = i64::from_le_bytes(pts);
        input.read_exact(&mut pixels)?;
        let packet = if let Some(encoder) = encoder.as_mut() {
            encoder.encode(&pixels, pts, force[0] != 0)?
        } else {
            let mut chosen = None;
            for candidate in video::candidates() {
                let attempt = (|| {
                    let mut device = video::Hardware::open(&config, &candidate)?;
                    let packet = device.encode(&pixels, pts, true)?;
                    ensure!(inspect_h264(&packet)?, "probe did not return an IDR");
                    Ok::<_, anyhow::Error>((device, packet))
                })();
                match attempt {
                    Ok(result) => {
                        chosen = Some(result);
                        break;
                    }
                    Err(error) => eprintln!("{}: {error:#}", candidate.label()),
                }
            }
            let Some((device, packet)) = chosen else {
                bail!("No working hardware H.264 encoder found")
            };
            encoder = Some(device);
            packet
        };
        let idr = inspect_h264(&packet).context("incompatible hardware output")?;
        ensure!(
            force[0] == 0 || idr,
            "hardware did not honor the IDR request"
        );
        write_json(
            &mut output,
            &Reply {
                encoder: encoder.as_ref().unwrap().label.clone(),
                bytes: packet.len(),
            },
        )?;
        output.write_all(&packet)?;
        output.flush()?;
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
