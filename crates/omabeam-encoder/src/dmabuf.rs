//! Zero-copy capture descriptors shared by the app and the encoder helper.
//!
//! The app captures into dmabufs and the helper imports them for GPU encoding,
//! so pixels never cross the CPU. Metadata crosses the stdin pipe as JSON;
//! dmabuf fds cross a Unix socket via SCM_RIGHTS, in plane order. This module
//! has no FFmpeg references, like the rest of this crate.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// `force` bytes marking a zero-copy frame request: pts + buffer id follow,
/// with no pixel bytes. `0`/`1` keep their existing meaning (delta / IDR).
pub const ZERO_COPY_REQUEST: u8 = 2;
pub const ZERO_COPY_REQUEST_IDR: u8 = 3;

/// Buffers in a capture ring: one in capture, one in encode, spares for a
/// readback or a resize racing the encoder.
pub const RING_BUFFERS: u32 = 4;
pub const MAX_RING_BUFFERS: u32 = 8;

/// `XR24` / `AR24` in little-endian fourcc form.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;
pub const FOURCC_ARGB8888: u32 = 0x3432_5241;

/// Linear dmabuf layout. Capture prefers it so a fallback readback is a
/// plain row copy; a driver-chosen modifier still travels in the descriptor.
pub const MOD_LINEAR: u64 = 0;
/// No layout constraint. A compositor offering only this accepts whatever
/// the allocator picks; capture resolves it to linear (see `pick_layout`).
pub const MOD_INVALID: u64 = 0x00FF_FFFF_FFFF_FFFF;

/// One plane of a dmabuf. Its fd travels over the socket; everything else
/// travels in the JSON announcement, in the same plane order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufPlane {
    pub offset: u32,
    pub stride: u32,
}

/// One capture buffer, without its fds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufDescriptor {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
}

impl DmabufDescriptor {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.width >= 16
                && self.height >= 16
                && self.width.max(self.height) <= 3840
                && self.width.min(self.height) <= 2160
                && self.width.is_multiple_of(2)
                && self.height.is_multiple_of(2),
            "invalid dmabuf dimensions"
        );
        ensure!(
            self.fourcc == FOURCC_XRGB8888 || self.fourcc == FOURCC_ARGB8888,
            "unsupported dmabuf pixel format"
        );
        ensure!(
            !self.planes.is_empty() && self.planes.len() <= 4,
            "dmabuf must describe one to four planes"
        );
        for plane in &self.planes {
            ensure!(
                plane.stride >= self.width * 4 && plane.stride <= 32768 * 4,
                "invalid dmabuf stride"
            );
            ensure!(plane.offset <= 512 * 1024 * 1024, "invalid dmabuf offset");
        }
        Ok(())
    }
}

/// Which ring buffer a frame lives in. The generation changes every time the
/// app rebuilds the ring (resize, reconnect); a stale id must be rejected,
/// never encoded, so a resized capture cannot encode another ring's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuBufferId {
    pub index: u32,
    pub generation: u64,
}

/// Parent-to-helper ring announcement, carried in [`crate::Config::gpu`].
/// `width`/`height` are the capture size; the stream size stays in `Config`.
/// The helper dials `socket`, then receives one fd per frame over it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuRing {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub count: u32,
    pub socket: String,
    /// Render node that allocated the ring. The helper imports on that GPU
    /// first; empty probes every node.
    #[serde(default)]
    pub node: String,
}

impl GpuRing {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == crate::VERSION, "GPU ring version mismatch");
        ensure!(
            self.count > 0 && self.count <= MAX_RING_BUFFERS,
            "invalid GPU ring size"
        );
        ensure!(
            !self.socket.is_empty() && self.socket.len() <= 200,
            "invalid GPU socket path"
        );
        ensure!(
            self.node.is_empty()
                || (self.node.len() <= 64
                    && self
                        .node
                        .strip_prefix("/dev/dri/renderD")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))),
            "invalid GPU render node"
        );
        DmabufDescriptor {
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            modifier: self.modifier,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: self.width * 4,
            }],
        }
        .validate()?;
        Ok(())
    }
}

/// One frame's dmabuf, sent over the socket with its fd. Frames are
/// self-describing so a resized ring needs no re-handshake: the helper
/// imports each `(generation, index)` once and caches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDescriptor {
    pub id: GpuBufferId,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub fourcc: u32,
}

impl FrameDescriptor {
    /// Fixed 40-byte layout: index, generation, width, height, stride,
    /// offset, modifier, fourcc.
    pub fn encode(&self) -> [u8; 40] {
        let mut out = [0; 40];
        out[0..4].copy_from_slice(&self.id.index.to_le_bytes());
        out[4..12].copy_from_slice(&self.id.generation.to_le_bytes());
        out[12..16].copy_from_slice(&self.width.to_le_bytes());
        out[16..20].copy_from_slice(&self.height.to_le_bytes());
        out[20..24].copy_from_slice(&self.stride.to_le_bytes());
        out[24..28].copy_from_slice(&self.offset.to_le_bytes());
        out[28..36].copy_from_slice(&self.modifier.to_le_bytes());
        out[36..40].copy_from_slice(&self.fourcc.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8; 40], count: u32) -> Result<Self> {
        let field = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let wide = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let descriptor = Self {
            id: GpuBufferId {
                index: field(0),
                generation: wide(4),
            },
            width: field(12),
            height: field(16),
            stride: field(20),
            offset: field(24),
            modifier: wide(28),
            fourcc: field(36),
        };
        ensure!(descriptor.id.index < count, "GPU buffer index out of range");
        ensure!(descriptor.id.generation > 0, "GPU frame lacks a generation");
        DmabufDescriptor {
            width: descriptor.width,
            height: descriptor.height,
            fourcc: descriptor.fourcc,
            modifier: descriptor.modifier,
            planes: vec![DmabufPlane {
                offset: descriptor.offset,
                stride: descriptor.stride,
            }],
        }
        .validate()?;
        Ok(descriptor)
    }
}

/// Write a zero-copy frame request: marker + pts + buffer id. Bounded and
/// fixed-size, like the existing request framing.
pub fn write_zero_copy_request(
    writer: &mut impl Write,
    pts: i64,
    id: GpuBufferId,
    force_idr: bool,
) -> Result<()> {
    writer.write_all(&[if force_idr {
        ZERO_COPY_REQUEST_IDR
    } else {
        ZERO_COPY_REQUEST
    }])?;
    writer.write_all(&pts.to_le_bytes())?;
    writer.write_all(&id.index.to_le_bytes())?;
    writer.write_all(&id.generation.to_le_bytes())?;
    Ok(())
}

pub fn read_zero_copy_request(reader: &mut impl Read) -> Result<(i64, GpuBufferId)> {
    let mut pts = [0; 8];
    reader.read_exact(&mut pts)?;
    let mut index = [0; 4];
    reader.read_exact(&mut index)?;
    let mut generation = [0; 8];
    reader.read_exact(&mut generation)?;
    Ok((
        i64::from_le_bytes(pts),
        GpuBufferId {
            index: u32::from_le_bytes(index),
            generation: u64::from_le_bytes(generation),
        },
    ))
}

/// Send one frame's dmabuf: the descriptor as data, its fd as SCM_RIGHTS.
pub fn send_frame_fd(
    socket: &UnixStream,
    fd: BorrowedFd<'_>,
    frame: &FrameDescriptor,
) -> Result<()> {
    let payload = frame.encode();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = rustix::net::SendAncillaryBuffer::new(&mut space);
    ensure!(
        cmsg.push(rustix::net::SendAncillaryMessage::ScmRights(
            std::slice::from_ref(&fd)
        )),
        "fd message too large"
    );
    let sent = rustix::net::sendmsg(
        socket,
        &[std::io::IoSlice::new(&payload)],
        &mut cmsg,
        rustix::net::SendFlags::empty(),
    )?;
    ensure!(sent == payload.len(), "short fd message");
    Ok(())
}

/// Receive one frame's dmabuf. `count` is the announced ring size; anything
/// else fails closed, never encoding a wrong buffer.
pub fn recv_frame_fd(socket: &UnixStream, count: u32) -> Result<(OwnedFd, FrameDescriptor)> {
    let mut payload = [0; 40];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = rustix::net::RecvAncillaryBuffer::new(&mut space);
    let mut slices = [std::io::IoSliceMut::new(&mut payload)];
    let received = rustix::net::recvmsg(
        socket,
        &mut slices,
        &mut cmsg,
        rustix::net::RecvFlags::empty(),
    )?;
    ensure!(received.bytes == payload.len(), "short fd message");
    let mut fds = Vec::new();
    for message in cmsg.drain() {
        if let rustix::net::RecvAncillaryMessage::ScmRights(rights) = message {
            fds.extend(rights);
        }
    }
    ensure!(fds.len() == 1, "fd handoff must carry exactly one fd");
    Ok((
        fds.pop().unwrap(),
        FrameDescriptor::decode(&payload, count)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    fn ring() -> GpuRing {
        GpuRing {
            version: crate::VERSION,
            width: 1920,
            height: 1080,
            fourcc: FOURCC_XRGB8888,
            modifier: MOD_LINEAR,
            count: RING_BUFFERS,
            socket: "/run/user/1000/omabeam/gpu-1.sock".into(),
            node: "/dev/dri/renderD128".into(),
        }
    }

    fn frame() -> FrameDescriptor {
        FrameDescriptor {
            id: GpuBufferId {
                index: 2,
                generation: 9,
            },
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            offset: 0,
            modifier: MOD_LINEAR,
            fourcc: FOURCC_XRGB8888,
        }
    }

    #[test]
    fn ring_and_frame_descriptors_validate() {
        let ring = ring();
        ring.validate().unwrap();
        assert_eq!(
            FrameDescriptor::decode(&frame().encode(), ring.count).unwrap(),
            frame()
        );
        let mut bad = frame().encode();
        bad[0] = 99;
        assert!(FrameDescriptor::decode(&bad, ring.count).is_err());
        let mut bad = ring.clone();
        bad.count = 0;
        assert!(bad.validate().is_err());
        let mut bad = ring.clone();
        bad.fourcc = 0x3231564e;
        assert!(bad.validate().is_err());
        let mut bad = ring.clone();
        bad.width = 1919;
        assert!(bad.validate().is_err());
        let mut bad = ring.clone();
        bad.socket.clear();
        assert!(bad.validate().is_err());
        let mut any_node = ring.clone();
        any_node.node.clear();
        any_node.validate().unwrap();
        for node in [
            "/dev/dri/card0",
            "/dev/dri/renderD",
            "/dev/dri/renderD12x",
            "/etc/passwd",
        ] {
            let mut bad = ring.clone();
            bad.node = node.into();
            assert!(bad.validate().is_err(), "{node}");
        }
    }

    #[test]
    fn zero_copy_request_round_trips() {
        let mut bytes = Vec::new();
        let id = GpuBufferId {
            index: 2,
            generation: 9,
        };
        write_zero_copy_request(&mut bytes, 123_456, id, false).unwrap();
        assert_eq!(bytes[0], ZERO_COPY_REQUEST);
        let mut slice = &bytes[1..];
        assert_eq!(read_zero_copy_request(&mut slice).unwrap(), (123_456, id));
        assert!(slice.is_empty());
        let mut bytes = Vec::new();
        write_zero_copy_request(&mut bytes, 7, id, true).unwrap();
        assert_eq!(bytes[0], ZERO_COPY_REQUEST_IDR);
    }

    #[test]
    fn fd_handoff_round_trips_over_a_socketpair() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        send_frame_fd(&a, file.as_fd(), &frame()).unwrap();
        let (_fd, decoded) = recv_frame_fd(&b, RING_BUFFERS).unwrap();
        assert_eq!(decoded, frame());
    }

    #[test]
    fn fd_handoff_fails_closed_on_size_mismatch() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        send_frame_fd(&a, file.as_fd(), &frame()).unwrap();
        assert!(recv_frame_fd(&b, 2).is_err());
    }
}
