//! Versioned IPC and process ownership for the native Cast helper. The Rust
//! executable does not link Open Screen or share an address space with it.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::SocketAddr;

#[cfg(unix)]
mod process;
#[cfg(unix)]
pub use process::Helper;

pub const VERSION: u32 = 1;
pub const MAX_HEADER: usize = 4096;
pub const MAX_FRAME: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}
impl VideoConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (320..=1920).contains(&self.width)
                && self.width % 2 == 0
                && (240..=1080).contains(&self.height)
                && self.height % 2 == 0,
            "Cast requires even video dimensions between 320×240 and 1920×1080"
        );
        ensure!((1..=30).contains(&self.fps), "Cast supports up to 30 fps");
        ensure!(
            (300_000..=20_000_000).contains(&self.bitrate),
            "Invalid Cast bitrate"
        );
        Ok(())
    }
    pub fn connect(&self, endpoint: SocketAddr) -> Result<Value> {
        self.validate()?;
        ensure!(
            !endpoint.ip().is_unspecified() && endpoint.port() > 0,
            "Invalid Cast endpoint"
        );
        Ok(serde_json::json!({"version": VERSION, "command": "connect",
            "endpoint": endpoint.to_string(), "width": self.width, "height": self.height,
            "fps": self.fps, "bitrate": self.bitrate}))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receiver {
    pub id: String,
    pub name: String,
    pub model: String,
    pub busy: bool,
    pub addresses: Vec<SocketAddr>,
}

pub fn write_json(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        (1..=MAX_HEADER).contains(&bytes.len()),
        "Cast IPC header too large"
    );
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}

pub fn read_event(reader: &mut impl Read) -> Result<Value> {
    let mut size = [0; 4];
    reader
        .read_exact(&mut size)
        .context("Cast helper event stream closed")?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(
        (1..=MAX_HEADER).contains(&size),
        "Invalid Cast IPC header length"
    );
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    let event: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        event["version"].as_u64() == Some(VERSION.into()),
        "Cast helper version mismatch"
    );
    ensure!(event["event"].is_string(), "Invalid Cast helper event");
    ensure!(
        !event
            .get("bytes")
            .is_some_and(|size| size.as_u64() != Some(0)),
        "Unexpected media on Cast event channel"
    );
    Ok(event)
}

/// A single Annex-B access unit. Producers must restart with an IDR if the
/// helper discards a frame. A retry result means this access unit is still
/// held until the receiver's window opens, and must not be replaced by a delta.
pub fn write_frame(
    writer: &mut impl Write,
    sequence: u64,
    pts_us: u64,
    capture_age_us: u64,
    bytes: &[u8],
) -> Result<()> {
    ensure!(
        sequence > 0 && pts_us <= 7 * 24 * 3600 * 1_000_000,
        "Invalid Cast frame sequence/timestamp"
    );
    ensure!(capture_age_us <= 10_000_000, "Cast frame is too old");
    ensure!(bytes.len() <= MAX_FRAME, "Cast frame is too large");
    let keyframe = omabeam_encoder::inspect_h264(bytes)?;
    write_json(
        writer,
        &serde_json::json!({"version": VERSION, "sequence": sequence,
        "pts_us": pts_us, "capture_age_us": capture_age_us, "keyframe": keyframe,
        "bytes": bytes.len()}),
    )?;
    writer.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_and_versioned_events() {
        let event = serde_json::json!({"version":1,"event":"state","state":"streaming"});
        let mut wire = vec![];
        write_json(&mut wire, &event).unwrap();
        assert_eq!(read_event(&mut &wire[..]).unwrap(), event);
        for invalid in [
            vec![0; 4],
            u32::MAX.to_be_bytes().to_vec(),
            wire[..6].to_vec(),
        ] {
            assert!(read_event(&mut &invalid[..]).is_err());
        }
        for invalid in [
            serde_json::json!({"version":2,"event":"ready"}),
            serde_json::json!({"version":1,"event":"ready","bytes":5}),
            serde_json::json!({"version":1}),
        ] {
            let mut wire = vec![];
            write_json(&mut wire, &invalid).unwrap();
            assert!(read_event(&mut &wire[..]).is_err());
        }
    }
    #[test]
    fn reject_unsupported_video_before_connect() {
        let mut config = VideoConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate: 4_000_000,
        };
        assert!(config.validate().is_ok());
        config.width = 1281;
        assert!(config.validate().is_err());
        config.width = 1280;
        config.fps = 60;
        assert!(config.validate().is_err());
    }
    #[test]
    fn discovery_preserves_numeric_ipv6_scope_for_connection() {
        let receiver: Receiver = serde_json::from_value(serde_json::json!({
            "id": "fixture", "name": "Receiver", "model": "Test", "busy": false,
            "addresses": ["[fe80::1%12]:8009", "192.0.2.1:8009"]
        }))
        .unwrap();
        let SocketAddr::V6(address) = receiver.addresses[0] else {
            panic!("IPv6 expected")
        };
        assert_eq!(address.scope_id(), 12);
        assert_eq!(address.to_string(), "[fe80::1%12]:8009");
    }
    #[test]
    fn frame_key_flag_comes_from_annex_b() {
        let payload = [
            0, 0, 0, 1, 0x67, 66, 0x40, 31, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 1,
        ];
        let mut wire = vec![];
        write_frame(&mut wire, 1, 0, 5, &payload).unwrap();
        let size = u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
        let header: Value = serde_json::from_slice(&wire[4..4 + size]).unwrap();
        assert_eq!(header["keyframe"], true);
        assert_eq!(&wire[4 + size..], &payload);
        assert!(write_frame(&mut vec![], 1, 0, 0, &[1, 2, 3]).is_err());
    }
}
