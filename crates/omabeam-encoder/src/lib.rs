//! Versioned, bounded pipe protocol. This library has no FFmpeg references;
//! only the separate helper executable links to the system media libraries.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, ErrorKind, IoSliceMut, Read, Write};

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
    /// Frames between IDRs. Zero keeps the historical two-second interval.
    /// A positive value is an explicit interval; Cast uses one minute so a
    /// keyframe is not spent on a clock.
    #[serde(default)]
    pub gop_frames: u32,
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

    pub fn gop_frames(&self) -> u32 {
        if self.gop_frames == 0 {
            self.fps.saturating_mul(2).max(1)
        } else {
            self.gop_frames.max(1)
        }
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

/// Read `rows` rows of `width` bytes, sent back to back, into `plane`, whose
/// rows start `stride` bytes apart. Padded rows are filled in place by
/// vectored reads, so padding costs neither a staging copy nor a read per row.
pub fn read_plane(
    input: &mut impl Read,
    plane: &mut [u8],
    width: usize,
    stride: usize,
    rows: usize,
) -> Result<()> {
    if rows == 0 || width == 0 {
        return Ok(());
    }
    ensure!(
        width <= stride && stride * (rows - 1) + width <= plane.len(),
        "plane is smaller than its rows"
    );
    if width == stride {
        input.read_exact(&mut plane[..width * rows])?;
        return Ok(());
    }
    let mut slices: Vec<_> = plane
        .chunks_mut(stride)
        .take(rows)
        .map(|row| IoSliceMut::new(&mut row[..width]))
        .collect();
    let mut slices = slices.as_mut_slice();
    while !slices.is_empty() {
        match input.read_vectored(slices) {
            Ok(0) => return Err(io::Error::from(ErrorKind::UnexpectedEof).into()),
            Ok(n) => IoSliceMut::advance_slices(&mut slices, n),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Interleave planar chroma (`width` samples per row, rows back to back) into
/// an NV12 UV plane whose rows start `stride` bytes apart.
pub fn interleave_uv(
    u: &[u8],
    v: &[u8],
    plane: &mut [u8],
    width: usize,
    stride: usize,
    rows: usize,
) -> Result<()> {
    if rows == 0 || width == 0 {
        return Ok(());
    }
    ensure!(
        u.len() >= width * rows
            && v.len() >= width * rows
            && 2 * width <= stride
            && stride * (rows - 1) + 2 * width <= plane.len(),
        "chroma does not fit its plane"
    );
    let rows = plane
        .chunks_mut(stride)
        .zip(u.chunks_exact(width))
        .zip(v.chunks_exact(width))
        .take(rows);
    for ((row, u), v) in rows {
        for ((pair, u), v) in row[..2 * width].chunks_exact_mut(2).zip(u).zip(v) {
            pair[0] = *u;
            pair[1] = *v;
        }
    }
    Ok(())
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
    fn zero_gop_is_two_seconds_and_an_explicit_interval_is_kept() {
        let mut config = Config {
            version: VERSION,
            width: 16,
            height: 16,
            fps: 30,
            bitrate: 100_000,
            gop_frames: 0,
        };
        assert_eq!(config.gop_frames(), 60);
        config.gop_frames = 1_800;
        assert_eq!(config.gop_frames(), 1_800);
    }

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

    /// Hands out at most `step` bytes per call, splitting rows across calls
    /// the way a pipe does.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.step).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
        fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> std::io::Result<usize> {
            let mut total = 0;
            for buf in bufs {
                let limit = buf.len().min(self.step - total);
                let n = self.read(&mut buf[..limit])?;
                total += n;
                if n < buf.len() || total == self.step {
                    break;
                }
            }
            Ok(total)
        }
    }

    #[test]
    fn plane_rows_land_at_their_stride_however_the_input_arrives() {
        // 683 × 9 is the chroma of a 1366 × 18 frame: padded rows, odd count.
        for (width, stride, rows) in [(683, 704, 9), (8, 8, 5), (6, 32, 1), (1920, 1920, 3)] {
            let data: Vec<u8> = (0..width * rows).map(|i| (i * 31 % 251) as u8).collect();
            for step in [1, 7, 700, usize::MAX] {
                // The last row needs only `width` bytes, as in a tight buffer.
                let mut plane = vec![0xee; stride * (rows - 1) + width];
                let mut input = Trickle { data: &data, step };
                read_plane(&mut input, &mut plane, width, stride, rows).unwrap();
                assert!(input.data.is_empty(), "left input unread");
                for (row, expected) in plane.chunks(stride).zip(data.chunks(width)) {
                    assert_eq!(&row[..width], expected, "step {step}");
                    assert!(row[width..].iter().all(|&b| b == 0xee), "wrote padding");
                }
            }
            // A plain slice fills many rows per vectored read.
            let mut plane = vec![0xee; stride * rows];
            read_plane(&mut data.as_slice(), &mut plane, width, stride, rows).unwrap();
            for (row, expected) in plane.chunks(stride).zip(data.chunks(width)) {
                assert_eq!(&row[..width], expected);
            }
        }
    }

    #[test]
    fn a_short_input_or_a_plane_smaller_than_its_rows_is_an_error() {
        let data = [7; 20];
        let mut plane = [0; 32];
        assert!(read_plane(&mut &data[..], &mut plane, 6, 8, 4).is_err());
        assert!(read_plane(&mut &data[..], &mut plane, 9, 8, 2).is_err());
        assert!(read_plane(&mut &data[..], &mut plane[..28], 5, 8, 4).is_err());
        assert!(read_plane(&mut &data[..], &mut plane[..29], 5, 8, 4).is_ok());
        let (u, v) = ([1; 6], [2; 6]);
        assert!(interleave_uv(&u, &v, &mut plane, 3, 5, 2).is_err());
        assert!(interleave_uv(&u, &v[..5], &mut plane, 3, 8, 2).is_err());
        assert!(interleave_uv(&u, &v, &mut plane[..13], 3, 8, 2).is_err());
        assert!(interleave_uv(&u, &v, &mut plane[..14], 3, 8, 2).is_ok());
    }

    #[test]
    fn nv12_chroma_matches_the_former_interleave_loop() {
        // The helper's scalar loop before the chunked interleave.
        fn reference(i420: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
            let mut plane = vec![0xee; stride * h / 2];
            let chroma = w * h / 4;
            for y in 0..h / 2 {
                let row = &mut plane[y * stride..y * stride + w];
                for x in 0..w / 2 {
                    row[x * 2] = i420[w * h + y * w / 2 + x];
                    row[x * 2 + 1] = i420[w * h + chroma + y * w / 2 + x];
                }
            }
            plane
        }
        // 18 rows of luma give 9 of chroma.
        for (w, h, stride) in [(1366, 18, 1376), (64, 36, 64), (16, 16, 32)] {
            let i420: Vec<u8> = (0..w * h * 3 / 2).map(|i| (i * 131 % 253) as u8).collect();
            let (u, v) = i420[w * h..].split_at(w * h / 4);
            let mut plane = vec![0xee; stride * h / 2];
            interleave_uv(u, v, &mut plane, w / 2, stride, h / 2).unwrap();
            assert_eq!(plane, reference(&i420, w, h, stride), "{w}x{h}");
        }
    }
}
