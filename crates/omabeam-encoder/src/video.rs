use anyhow::{Context, Result, ensure};
use ffmpeg_next::{self as av, codec, ffi, format::Pixel, frame, picture};
use omabeam_encoder::{Config, interleave_uv, read_plane};
use std::{ffi::CString, io::Read, ptr};

#[derive(Debug, Clone)]
pub enum Candidate {
    Nvenc,
    Vaapi(String),
    VideoToolbox,
}
impl Candidate {
    pub fn label(&self) -> String {
        match self {
            Self::Nvenc => "NVIDIA NVENC".into(),
            Self::Vaapi(device) => format!("VA-API ({device})"),
            Self::VideoToolbox => "Apple VideoToolbox".into(),
        }
    }
    fn codec(&self) -> &str {
        match self {
            Self::Nvenc => "h264_nvenc",
            Self::Vaapi(_) => "h264_vaapi",
            Self::VideoToolbox => "h264_videotoolbox",
        }
    }
    /// CPU frame formats to try, best first. NVENC takes the planar I420 the
    /// parent sends, with no interleave, and falls back to NV12 if it fails.
    /// VideoToolbox delays planar frames, and VA-API uploads NV12.
    pub fn formats(&self) -> &'static [Pixel] {
        match self {
            Self::Nvenc => &[Pixel::YUV420P, Pixel::NV12],
            Self::Vaapi(_) | Self::VideoToolbox => &[Pixel::NV12],
        }
    }
}

fn nvenc_maxrate(bitrate: u32) -> u32 {
    bitrate.saturating_mul(2)
}

pub fn candidates() -> Vec<Candidate> {
    // Probe encoders and devices, rather than trusting a PCI vendor name or a
    // compiled-in codec list. Multiple render nodes include hybrid laptops.
    match std::env::consts::OS {
        "linux" => {
            let mut devices: Vec<_> = std::fs::read_dir("/dev/dri")
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.strip_prefix("renderD")
                        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                        .map(|_| entry.path().to_string_lossy().into_owned())
                })
                .collect();
            devices.sort();
            std::iter::once(Candidate::Nvenc)
                .chain(devices.into_iter().take(8).map(Candidate::Vaapi))
                .collect()
        }
        "macos" => vec![Candidate::VideoToolbox],
        _ => Vec::new(),
    }
}

struct BufferRef(*mut ffi::AVBufferRef);
impl Drop for BufferRef {
    fn drop(&mut self) {
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}
fn checked(code: i32) -> Result<()> {
    if code < 0 {
        Err(av::Error::from(code).into())
    } else {
        Ok(())
    }
}

pub struct Hardware {
    pub label: String,
    encoder: codec::encoder::video::Encoder,
    frames: Option<BufferRef>,
    cpu: frame::Video,
    /// NV12 only: the planar U and V rows, staged for the interleave.
    chroma: Vec<u8>,
    packet: av::Packet,
    config: Config,
}
impl Hardware {
    pub fn open(config: &Config, candidate: &Candidate, format: Pixel) -> Result<Self> {
        config.frame_len()?;
        ensure!(
            matches!(format, Pixel::YUV420P | Pixel::NV12),
            "unsupported frame format"
        );
        let codec = av::encoder::find_by_name(candidate.codec())
            .context("encoder is not in this FFmpeg build")?;
        let mut encoder = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        encoder.set_width(config.width);
        encoder.set_height(config.height);
        encoder.set_time_base((1, 1_000_000));
        encoder.set_frame_rate(Some((config.fps as i32, 1)));
        encoder.set_bit_rate(config.bitrate as usize);
        encoder.set_gop(config.gop_frames());
        encoder.set_max_b_frames(0);
        encoder.set_flags(codec::Flags::LOW_DELAY);
        encoder.set_format(format);
        let mut options = av::Dictionary::new();
        options.set("profile", "baseline");
        let mut frames = None;
        match candidate {
            Candidate::Nvenc => {
                options.set("preset", "p1");
                options.set("tune", "ull");
                options.set("rc", "vbr");
                options.set("maxrate", &nvenc_maxrate(config.bitrate).to_string());
                options.set("rc-lookahead", "0");
                options.set("zerolatency", "1");
                options.set("delay", "0");
                options.set("forced-idr", "1");
                options.set("no-scenecut", "1");
                options.set("gpu", "-1");
            }
            Candidate::VideoToolbox => {
                options.set("profile", "constrained_baseline");
                options.set("realtime", "1");
                options.set("allow_sw", "0");
            }
            Candidate::Vaapi(path) => {
                options.set("profile", "constrained_baseline");
                options.set("async_depth", "1");
                options.set("idr_interval", "0");
                encoder.set_format(Pixel::VAAPI);
                let path = CString::new(path.as_str())?;
                let mut device = BufferRef(ptr::null_mut());
                // Each pointer is created/refcounted by libavutil. BufferRef
                // releases our references; AVCodecContext owns its own ref.
                unsafe {
                    checked(ffi::av_hwdevice_ctx_create(
                        &mut device.0,
                        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                        path.as_ptr(),
                        ptr::null_mut(),
                        0,
                    ))?;
                    let pool = BufferRef(ffi::av_hwframe_ctx_alloc(device.0));
                    ensure!(!pool.0.is_null(), "cannot allocate VA-API frame pool");
                    let context = &mut *((*pool.0).data as *mut ffi::AVHWFramesContext);
                    context.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                    context.sw_format = format.into();
                    context.width = config.width as i32;
                    context.height = config.height as i32;
                    context.initial_pool_size = 4;
                    checked(ffi::av_hwframe_ctx_init(pool.0))?;
                    (*encoder.as_mut_ptr()).hw_frames_ctx = ffi::av_buffer_ref(pool.0);
                    ensure!(
                        !(*encoder.as_ptr()).hw_frames_ctx.is_null(),
                        "cannot retain VA-API frame pool"
                    );
                    frames = Some(pool);
                }
            }
        }
        let encoder = encoder
            .open_as_with(codec, options)
            .context("cannot open hardware encoder")?;
        let mut cpu = frame::Video::empty();
        cpu.set_format(format);
        cpu.set_width(config.width);
        cpu.set_height(config.height);
        unsafe {
            checked(ffi::av_frame_get_buffer(cpu.as_mut_ptr(), 32))?;
        }
        let chroma = if format == Pixel::NV12 {
            vec![0; config.width as usize * config.height as usize / 2]
        } else {
            Vec::new()
        };
        Ok(Self {
            label: candidate.label(),
            encoder,
            frames,
            cpu,
            chroma,
            packet: av::Packet::empty(),
            config: config.clone(),
        })
    }

    /// Fill the reused frame with the next I420 frame from `input`. Planar
    /// rows land straight in the frame; NV12 stages only the chroma, for the
    /// interleave.
    pub fn read_frame(&mut self, input: &mut impl Read) -> Result<()> {
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        let cpu = &mut self.cpu;
        unsafe {
            // Reuse storage when the codec released it; copy on write if a
            // driver still retains a reference to the previous frame.
            checked(ffi::av_frame_make_writable(cpu.as_mut_ptr()))?;
        }
        let stride = cpu.stride(0);
        read_plane(input, cpu.data_mut(0), w, stride, h)?;
        if cpu.format() == Pixel::NV12 {
            input.read_exact(&mut self.chroma)?;
            let (u, v) = self.chroma.split_at(w * h / 4);
            let stride = cpu.stride(1);
            interleave_uv(u, v, cpu.data_mut(1), w / 2, stride, h / 2)?;
        } else {
            for plane in [1, 2] {
                let stride = cpu.stride(plane);
                read_plane(input, cpu.data_mut(plane), w / 2, stride, h / 2)?;
            }
        }
        Ok(())
    }

    /// Encode the frame last read; `packet` returns it until the next call.
    pub fn encode(&mut self, pts: i64, force: bool) -> Result<()> {
        let mut gpu;
        let frame = if let Some(pool) = &self.frames {
            gpu = frame::Video::empty();
            unsafe {
                checked(ffi::av_hwframe_get_buffer(pool.0, gpu.as_mut_ptr(), 0))?;
                checked(ffi::av_hwframe_transfer_data(
                    gpu.as_mut_ptr(),
                    self.cpu.as_ptr(),
                    0,
                ))?;
            }
            &mut gpu
        } else {
            &mut self.cpu
        };
        frame.set_pts(Some(pts));
        frame.set_kind(if force {
            picture::Type::I
        } else {
            picture::Type::None
        });
        self.encoder.send_frame(frame)?;
        // Low-delay operation must return this frame, including the first
        // static desktop frame. Delayed/reordered backends are rejected.
        self.encoder
            .receive_packet(&mut self.packet)
            .context("encoder delayed or failed to encode the frame")?;
        ensure!(
            self.packet.pts() == Some(pts),
            "hardware returned a delayed frame"
        );
        ensure!(
            self.packet.data().is_some_and(|data| !data.is_empty()),
            "empty encoded packet"
        );
        Ok(())
    }

    pub fn packet(&self) -> &[u8] {
        self.packet.data().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_backend_tries_its_frame_formats_in_order() {
        assert_eq!(Candidate::Nvenc.formats(), [Pixel::YUV420P, Pixel::NV12]);
        assert_eq!(
            Candidate::Vaapi("/dev/dri/renderD128".into()).formats(),
            [Pixel::NV12]
        );
        // VideoToolbox delays planar frames, so its probe would only fall back.
        assert_eq!(Candidate::VideoToolbox.formats(), [Pixel::NV12]);
    }

    #[test]
    fn nvenc_maxrate_is_twice_the_target() {
        assert_eq!(nvenc_maxrate(4_000_000), 8_000_000);
        assert_eq!(nvenc_maxrate(16_000_000), 32_000_000);
        assert_eq!(nvenc_maxrate(u32::MAX / 2 + 1), u32::MAX);
    }

    fn decode(packet: &[u8]) -> frame::Video {
        let codec = av::decoder::find(codec::Id::H264).unwrap();
        let mut decoder = codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()
            .unwrap();
        decoder.send_packet(&av::Packet::copy(packet)).unwrap();
        decoder.send_eof().unwrap();
        let mut decoded = frame::Video::empty();
        decoder.receive_frame(&mut decoded).unwrap();
        decoded
    }

    /// Mean absolute difference between a decoded plane and the one sent.
    fn difference(decoded: &frame::Video, plane: usize, sent: &[u8], width: usize) -> f64 {
        let rows = sent.len() / width;
        let total: u64 = decoded
            .data(plane)
            .chunks(decoded.stride(plane))
            .zip(sent.chunks(width))
            .map(|(row, sent)| {
                row[..width]
                    .iter()
                    .zip(sent)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
                    .sum::<u64>()
            })
            .sum();
        total as f64 / (width * rows) as f64
    }

    /// Runs wherever a hardware encoder opens (VideoToolbox on macOS, NVENC or
    /// VA-API on Linux) and checks nothing where none does.
    #[test]
    fn every_backend_format_that_opens_encodes_the_frame_it_was_sent() {
        av::init().unwrap();
        let (mut verified, mut skipped) = (Vec::new(), Vec::new());
        // 1366 pads every row of every plane; 320 × 240 pads none.
        for (width, height) in [(1366, 768), (320, 240)] {
            let config = Config {
                version: omabeam_encoder::VERSION,
                width,
                height,
                fps: 30,
                bitrate: 20_000_000,
                gop_frames: 0,
            };
            let (w, h) = (width as usize, height as usize);
            // A luma ramp and flat, distinct chroma: a swapped or misaligned
            // plane shows.
            let mut i420: Vec<u8> = (0..w * h).map(|i| (16 + i % w * 200 / w) as u8).collect();
            i420.resize(w * h * 5 / 4, 90);
            i420.resize(w * h * 3 / 2, 170);
            let (y, chroma) = i420.split_at(w * h);
            let (u, v) = chroma.split_at(w * h / 4);
            for candidate in candidates() {
                for &format in candidate.formats() {
                    let name = format!("{} {format:?} {width}x{height}", candidate.label());
                    let mut device = match Hardware::open(&config, &candidate, format) {
                        Ok(device) => device,
                        Err(error) => {
                            skipped.push(format!("{name}: {error:#}"));
                            continue;
                        }
                    };
                    device.read_frame(&mut i420.as_slice()).unwrap();
                    if let Err(error) = device.encode(0, true) {
                        skipped.push(format!("{name}: {error:#}"));
                        continue;
                    }
                    let decoded = decode(device.packet());
                    assert_eq!((decoded.width(), decoded.height()), (width, height));
                    for (plane, sent, plane_width) in [(0, y, w), (1, u, w / 2), (2, v, w / 2)] {
                        let error = difference(&decoded, plane, sent, plane_width);
                        assert!(error < 3.0, "{name}: plane {plane} is off by {error:.1}");
                    }
                    verified.push(name);
                }
            }
        }
        eprintln!("verified: {verified:#?}\nskipped: {skipped:#?}");
    }
}
