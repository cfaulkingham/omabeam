use anyhow::{Context, Result, ensure};
use ffmpeg_next::{self as av, codec, ffi, format::Pixel, frame, picture};
use omabeam_encoder::Config;
use std::{ffi::CString, ptr};

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
    config: Config,
}
impl Hardware {
    pub fn open(config: &Config, candidate: &Candidate) -> Result<Self> {
        config.frame_len()?;
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
        encoder.set_gop(config.fps * 2);
        encoder.set_max_b_frames(0);
        encoder.set_flags(codec::Flags::LOW_DELAY);
        encoder.set_format(Pixel::NV12);
        let mut options = av::Dictionary::new();
        options.set("profile", "baseline");
        let mut frames = None;
        match candidate {
            Candidate::Nvenc => {
                options.set("preset", "p1");
                options.set("tune", "ull");
                options.set("rc", "cbr");
                options.set("rc-lookahead", "0");
                options.set("zerolatency", "1");
                options.set("delay", "0");
                options.set("forced-idr", "1");
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
                    context.sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
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
        cpu.set_format(Pixel::NV12);
        cpu.set_width(config.width);
        cpu.set_height(config.height);
        unsafe {
            checked(ffi::av_frame_get_buffer(cpu.as_mut_ptr(), 32))?;
        }
        Ok(Self {
            label: candidate.label(),
            encoder,
            frames,
            cpu,
            config: config.clone(),
        })
    }

    pub fn encode(&mut self, i420: &[u8], pts: i64, force: bool) -> Result<Vec<u8>> {
        ensure!(
            i420.len() == self.config.frame_len()?,
            "wrong raw frame size"
        );
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        let cpu = &mut self.cpu;
        unsafe {
            // Reuse storage when the codec released it; copy on write if a
            // driver still retains a reference to the previous frame.
            checked(ffi::av_frame_make_writable(cpu.as_mut_ptr()))?;
        }
        let y_stride = cpu.stride(0);
        for y in 0..h {
            cpu.data_mut(0)[y * y_stride..y * y_stride + w]
                .copy_from_slice(&i420[y * w..(y + 1) * w]);
        }
        let uv_stride = cpu.stride(1);
        let chroma = w * h / 4;
        for y in 0..h / 2 {
            let row = &mut cpu.data_mut(1)[y * uv_stride..y * uv_stride + w];
            for x in 0..w / 2 {
                row[x * 2] = i420[w * h + y * w / 2 + x];
                row[x * 2 + 1] = i420[w * h + chroma + y * w / 2 + x];
            }
        }
        let mut gpu = frame::Video::empty();
        let frame = if let Some(pool) = &self.frames {
            unsafe {
                checked(ffi::av_hwframe_get_buffer(pool.0, gpu.as_mut_ptr(), 0))?;
                checked(ffi::av_hwframe_transfer_data(
                    gpu.as_mut_ptr(),
                    cpu.as_ptr(),
                    0,
                ))?;
            }
            &mut gpu
        } else {
            cpu
        };
        frame.set_pts(Some(pts));
        frame.set_kind(if force {
            picture::Type::I
        } else {
            picture::Type::None
        });
        self.encoder.send_frame(&frame)?;
        let mut packet = av::Packet::empty();
        // Low-delay operation must return this frame, including the first
        // static desktop frame. Delayed/reordered backends are rejected.
        self.encoder
            .receive_packet(&mut packet)
            .context("encoder delayed or failed to encode the frame")?;
        ensure!(
            packet.pts() == Some(pts),
            "hardware returned a delayed frame"
        );
        Ok(packet.data().context("empty encoded packet")?.to_vec())
    }
}
