//! The media driver runs only in this disposable child, never in the GUI or
//! transport process. stdout is exclusively the framed protocol.
mod video;
mod zero_copy;

/// Serializes hardware tests: CUDA/VA-API setup shifts process-global state
/// (fd counts, device handles) that parallel tests would otherwise observe.
#[cfg(test)]
pub(crate) static HW_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock the hardware tests, recovering from a poisoned mutex: a failed
/// hardware test must not cascade into the others.
#[cfg(test)]
pub(crate) fn lock_hw_tests() -> std::sync::MutexGuard<'static, ()> {
    HW_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
use anyhow::{Context, Result, bail, ensure};
use omabeam_encoder::{Config, MAX_HEADER, Reply, inspect_h264, read_json, write_json};
use std::{
    fs::File,
    io::{ErrorKind, IoSlice, Read, Write},
    os::fd::AsFd,
    os::unix::net::UnixStream,
};

fn run() -> Result<()> {
    ffmpeg_next::init()?;
    ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Error);
    let mut input = std::io::stdin().lock();
    // Unbuffered, so a reply leaves in one vectored write straight from the
    // packet, without std's line buffering.
    let mut output = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    let config: Config = read_json(&mut input)?;
    let frame_len = config.frame_len()?;
    if let Some(ring) = &config.gpu {
        ring.validate()?;
    }
    let mut encoder: Option<video::Hardware> = None;
    let mut zero_copy = match &config.gpu {
        Some(ring) => {
            let socket = UnixStream::connect(&ring.socket).context("cannot dial GPU socket")?;
            Some(zero_copy::ZeroCopy::open(&config, ring, socket)?)
        }
        None => None,
    };
    let mut header = Vec::with_capacity(4 + MAX_HEADER);
    loop {
        let mut force = [0];
        if input.read(&mut force)? == 0 {
            return Ok(());
        }
        let force_idr = match force[0] {
            0 => false,
            1 => true,
            2 => false,
            3 => true,
            _ => bail!("invalid frame request"),
        };
        // The zero-copy path owns its packet; the upload path borrows the
        // encoder's. One buffer unifies them for the shared reply below.
        let owned: Vec<u8>;
        let (packet, label, idr): (&[u8], String, bool) = match force[0] {
            0 | 1 => {
                ensure!(zero_copy.is_none(), "upload frame in a zero-copy session");
                let mut pts = [0; 8];
                input.read_exact(&mut pts)?;
                let pts = i64::from_le_bytes(pts);
                let device = match encoder.as_mut() {
                    Some(device) => {
                        device.read_frame(&mut input)?;
                        device.encode(pts, force_idr)?;
                        device
                    }
                    None => {
                        // Every probe attempt needs the same first frame, so only it
                        // is staged.
                        let mut pixels = vec![0; frame_len];
                        input.read_exact(&mut pixels)?;
                        encoder.insert(probe(&config, &pixels, pts)?)
                    }
                };
                let packet = device.packet();
                let idr = inspect_h264(packet).context("incompatible hardware output")?;
                (packet, device.label.clone(), idr)
            }
            _ => {
                let session = zero_copy
                    .as_mut()
                    .context("zero-copy frame without a GPU ring")?;
                let (pts, id) = omabeam_encoder::dmabuf::read_zero_copy_request(&mut input)?;
                let (bytes, key) = session.encode(pts, id, force_idr)?;
                let label = session.label().unwrap_or("GPU zero-copy").to_owned();
                owned = bytes;
                (owned.as_slice(), label, key)
            }
        };
        ensure!(!force_idr || idr, "hardware did not honor the IDR request");
        header.clear();
        write_json(
            &mut header,
            &Reply {
                encoder: label,
                bytes: packet.len(),
            },
        )?;
        write_all(
            &mut output,
            &mut [IoSlice::new(&header), IoSlice::new(packet)],
        )?;
    }
}

/// Open the first backend and frame format that turns this frame into an IDR.
fn probe(config: &Config, pixels: &[u8], pts: i64) -> Result<video::Hardware> {
    for candidate in video::candidates() {
        let mut failure = None;
        // A backend that rejects planar input, at open or on this first
        // frame, falls back to NV12.
        for &format in candidate.formats() {
            let attempt = (|| {
                let mut device = video::Hardware::open(config, &candidate, format)?;
                device.read_frame(&mut &pixels[..])?;
                device.encode(pts, true)?;
                ensure!(
                    inspect_h264(device.packet())?,
                    "probe did not return an IDR"
                );
                Ok::<_, anyhow::Error>(device)
            })();
            match attempt {
                Ok(device) => return Ok(device),
                Err(error) => failure = Some(error),
            }
        }
        if let Some(error) = failure {
            eprintln!("{}: {error:#}", candidate.label());
        }
    }
    bail!("No working hardware H.264 encoder found")
}

/// `Write::write_all` over several buffers, in as few writes as the pipe
/// allows.
fn write_all(output: &mut impl Write, mut slices: &mut [IoSlice<'_>]) -> std::io::Result<()> {
    while !slices.is_empty() {
        match output.write_vectored(slices) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut slices, n),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
