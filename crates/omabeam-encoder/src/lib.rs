//! Versioned, bounded pipe protocol. This library has no FFmpeg references;
//! only the separate helper executable links to the system media libraries.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{Read, Write};

pub const VERSION: u32 = 1;
pub const MAX_HEADER: usize = 4096;
pub const MAX_PACKET: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}
impl Config {
    pub fn frame_len(&self) -> Result<usize> {
        ensure!(self.version == VERSION, "encoder helper version mismatch");
        ensure!(
            self.width >= 16
                && self.height >= 16
                && self.width.max(self.height) <= 3840
                && self.width.min(self.height) <= 2160
                && self.width % 2 == 0
                && self.height % 2 == 0,
            "invalid H.264 dimensions"
        );
        ensure!(
            (1..=60).contains(&self.fps) && (100_000..=50_000_000).contains(&self.bitrate),
            "invalid H.264 rate"
        );
        Ok(self.width as usize * self.height as usize * 3 / 2)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub encoder: String,
    pub bytes: usize,
}

pub fn write_json(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_HEADER, "encoder header too large");
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}

pub fn read_json<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    ensure!(
        (1..=MAX_HEADER).contains(&size),
        "invalid encoder header size"
    );
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Inspect the actual Annex B payload, not an encoder's packet flag. New peers
/// and recovery require an independently decodable IDR with parameter sets.
pub fn inspect_h264(bytes: &[u8]) -> Result<bool> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_PACKET,
        "invalid H.264 packet size"
    );
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push(i + prefix);
        i += prefix;
    }
    ensure!(
        !starts.is_empty() && bytes[..starts[0] - 1].iter().all(|b| *b == 0),
        "encoder did not produce Annex B H.264"
    );
    let (mut idr, mut sps, mut pps, mut picture) = (false, false, false, false);
    for at in starts {
        let nal = *bytes
            .get(at)
            .ok_or_else(|| anyhow::anyhow!("empty H.264 NAL"))?
            & 31;
        match nal {
            1 => picture = true,
            5 => {
                idr = true;
                picture = true;
            }
            7 => {
                // Match the constrained-baseline profile negotiated by str0m.
                ensure!(
                    bytes.get(at + 1) == Some(&66)
                        && bytes.get(at + 2).is_some_and(|flags| flags & 0x40 != 0),
                    "hardware encoder did not produce constrained-baseline H.264"
                );
                sps = true;
            }
            8 => pps = true,
            _ => {}
        }
    }
    ensure!(picture, "encoder packet contains no picture");
    ensure!(!idr || (sps && pps), "IDR is missing H.264 parameter sets");
    Ok(idr)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unbounded_headers_and_incompatible_bitstreams() {
        assert!(read_json::<Reply>(&mut u32::MAX.to_le_bytes().as_slice()).is_err());
        assert!(inspect_h264(&[0, 0, 1, 0x65, 1]).is_err());
        let mut frame = vec![
            0, 0, 0, 1, 0x67, 66, 0xe0, 31, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1,
        ];
        assert!(inspect_h264(&frame).unwrap());
        frame[5] = 100;
        assert!(inspect_h264(&frame).is_err());
        assert!(!inspect_h264(&[0, 0, 1, 0x41, 1]).unwrap());
    }
}
