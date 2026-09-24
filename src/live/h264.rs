//! Shared capture conversion and H.264 backends for browser and Cast.
mod hardware;
use super::LiveConfig;
use crate::live::config::EncoderMode;
use anyhow::{Context, Result, ensure};
use omabeam_capture::CapturedFrame;
use openh264::formats::YUVSource;
use openh264::{
    OpenH264API, Timestamp,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
        Profile, RateControlMode, UsageType,
    },
    formats::{RgbSliceU8, RgbaSliceU8, YUVBuffer},
};
use openh264_sys2::{ENCODER_OPTION_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL};
use std::time::Instant;

pub(super) struct RawFrame {
    pub frame: CapturedFrame,
    pub config: LiveConfig,
    pub captured_at: Instant,
}

fn create(config: &LiveConfig, periodic_idr: bool) -> Result<Encoder> {
    // Two seconds at the configured rate. Cast turns this off: a desktop IDR
    // consumes the bitrate budget and the following frames go soft.
    let period = if periodic_idr {
        config.fps.saturating_mul(2).max(1)
    } else {
        0
    };
    Ok(Encoder::with_api_config(
        OpenH264API::from_source(),
        EncoderConfig::new()
            .bitrate(BitRate::from_bps(config.h264_bitrate))
            .max_frame_rate(FrameRate::from_hz(config.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .usage_type(UsageType::ScreenContentRealTime)
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .num_threads(2)
            .scene_change_detect(periodic_idr)
            .intra_frame_period(IntraFramePeriod::from_num_frames(period)),
    )?)
}

pub(super) struct AdaptiveEncoder {
    config: LiveConfig,
    software: Encoder,
    hardware: Option<hardware::Hardware>,
    attempted: bool,
    software_started: bool,
    periodic_idr: bool,
    pub name: String,
    pub note: Option<String>,
}
impl AdaptiveEncoder {
    /// Apply a congestion target without reopening the encoder. A new process
    /// or OpenH264 instance would insert an IDR and break the frames already
    /// in flight. FFmpeg's NVENC reconfigure also forces an IDR, so an open
    /// hardware session keeps the rate it was created with; the stored config
    /// is what a later software fallback starts from.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        if bitrate == self.config.h264_bitrate {
            return Ok(());
        }
        self.config.h264_bitrate = bitrate;
        if self.software_started {
            set_openh264_bitrate(&mut self.software, bitrate)?;
        } else {
            self.software = create(&self.config, self.periodic_idr)?;
        }
        Ok(())
    }
    /// Emit an IDR only for the first frame and when asked: a new viewer, or a
    /// PLI/FIR. Cast and WebRTC both use this. The WebRTC sender coalesces
    /// requests without dropping them, so a lost IDR is repaired by the
    /// browser's next PLI rather than by a periodic keyframe.
    pub fn idr_only_when_requested(&mut self) -> Result<()> {
        self.periodic_idr = false;
        if !self.software_started {
            self.software = create(&self.config, false)?;
        }
        Ok(())
    }
    pub fn new(config: &LiveConfig) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            software: create(config, true)?,
            hardware: None,
            attempted: config.encoder == EncoderMode::Software,
            software_started: false,
            periodic_idr: true,
            name: "OpenH264 software".into(),
            note: None,
        })
    }
    pub fn encode(
        &mut self,
        yuv: &YUVBuffer,
        pts: i64,
        mut force: bool,
    ) -> Result<(Vec<u8>, bool)> {
        if self
            .hardware
            .as_ref()
            .is_some_and(|h| h.dimensions != yuv.dimensions())
        {
            self.hardware = None;
            self.attempted = false;
        }
        let hardware_result = (|| {
            if !self.attempted {
                self.attempted = true;
                let (w, h) = yuv.dimensions();
                let fps = self.config.fps.min(60).max(1);
                self.hardware = Some(hardware::Hardware::new(&omabeam_encoder::Config {
                    version: omabeam_encoder::VERSION,
                    width: w as u32,
                    height: h as u32,
                    fps,
                    bitrate: self.config.h264_bitrate,
                    gop_frames: if self.periodic_idr {
                        0
                    } else {
                        fps.saturating_mul(60)
                    },
                })?);
            }
            match &mut self.hardware {
                Some(hardware) => {
                    let frame = hardware.encode(yuv, pts, force)?;
                    self.name = hardware.name.clone();
                    self.note = None;
                    Ok(Some(frame))
                }
                None => Ok(None),
            }
        })();
        match hardware_result {
            Ok(Some(frame)) => return Ok(frame),
            Ok(None) => {}
            Err(error) => {
                self.hardware = None;
                if self.config.encoder == EncoderMode::Hardware {
                    return Err(error);
                }
                self.note = Some(
                    format!("Hardware unavailable; using software. {error:#}")
                        .chars()
                        .take(600)
                        .collect(),
                );
                self.name = "OpenH264 software".into();
                // A backend switch must restart the prediction sequence with
                // fresh SPS/PPS and IDR, even when the desktop is static.
                force = true;
            }
        }
        if force {
            self.software.force_intra_frame();
        }
        let bitstream = self
            .software
            .encode_at(yuv, Timestamp::from_millis((pts.max(0) / 1000) as u64))?;
        self.software_started = true;
        Ok((bitstream.to_vec(), bitstream.frame_type() == FrameType::IDR))
    }
}

fn set_openh264_bitrate(encoder: &mut Encoder, bitrate: u32) -> Result<()> {
    let mut info = SBitrateInfo {
        iLayer: SPATIAL_LAYER_ALL,
        iBitrate: i32::try_from(bitrate).unwrap_or(i32::MAX),
    };
    // SAFETY: the encoder has already encoded, and this option only replaces
    // the rate-control target. It does not reset the reference pictures.
    let code = unsafe {
        encoder.raw_api().set_option(
            ENCODER_OPTION_BITRATE,
            (&mut info as *mut SBitrateInfo).cast(),
        )
    };
    ensure!(code == 0, "OpenH264 rejected bitrate {bitrate} ({code})");
    Ok(())
}

/// `--check-encoders` sizes when none are given: the old probe size, then
/// 1080p and 4K, where a driver that opens small sessions can still fail.
const PROBE_SIZES: [(u32, u32); 3] = [(640, 360), (1920, 1080), (3840, 2160)];

/// Encode a test card at each size, rounded up to even as a stream would be,
/// with a fresh encoder at the configured rate. `--encoder hardware` fails if
/// any size fails; otherwise each size reports the backend it ended up on.
pub fn probe(config: &LiveConfig, sizes: &[(u32, u32)]) -> Result<serde_json::Value> {
    let sizes = if sizes.is_empty() {
        &PROBE_SIZES[..]
    } else {
        sizes
    };
    // Reject any unusable size before starting an encoder.
    let sizes = sizes
        .iter()
        .map(|&(width, height)| {
            h264_size(width, height).with_context(|| format!("cannot check {width}×{height}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut config = config.clone();
    config.fps = config.fps.min(60);
    let mut results = Vec::new();
    for (width, height) in sizes {
        let yuv = test_card(width, height);
        let mut encoder = AdaptiveEncoder::new(&config)?;
        // Verify first frame, a delta, and a forced IDR at the negotiated profile.
        for (pts, force) in [(0, true), (100_000, false), (200_000, true)] {
            let (bytes, _) = encoder
                .encode(&yuv, pts, force)
                .with_context(|| format!("encoder check failed at {width}×{height}"))?;
            let idr = omabeam_encoder::inspect_h264(&bytes)?;
            ensure!(
                !force || idr,
                "encoder probe did not return a requested IDR at {width}×{height}"
            );
        }
        results.push(serde_json::json!({
            "encoder": encoder.name,
            "hardware": encoder.hardware.is_some(),
            "note": encoder.note,
            "width": yuv.dimensions().0,
            "height": yuv.dimensions().1,
        }));
    }
    Ok(probe_report(results))
}

/// Keep the single-size fields at the top level, taken from the first size
/// that did not use hardware (else the first size), so `hardware` there holds
/// only when every size did.
fn probe_report(sizes: Vec<serde_json::Value>) -> serde_json::Value {
    let mut report = sizes
        .iter()
        .find(|size| size["hardware"] != true)
        .unwrap_or(&sizes[0])
        .clone();
    report["sizes"] = sizes.into();
    report
}

/// A synthetic I420 card (the main crate has no image dependency outside
/// tests). Luma ramps and chroma bands give every size some texture.
fn test_card(width: usize, height: usize) -> YUVBuffer {
    let mut yuv = vec![0; width * height * 3 / 2];
    let (luma, chroma) = yuv.split_at_mut(width * height);
    for (y, row) in luma.chunks_exact_mut(width).enumerate() {
        for (x, value) in row.iter_mut().enumerate() {
            *value = (16 + (x + 3 * y) % 220) as u8;
        }
    }
    for (y, row) in chroma.chunks_exact_mut(width / 2).enumerate() {
        for (x, value) in row.iter_mut().enumerate() {
            *value = (64 + (x / 8 + y / 8) % 128) as u8;
        }
    }
    YUVBuffer::from_vec(yuv, width, height)
}

/// The I420 size for a stream: odd edges are padded by one pixel, never
/// scaled, so native text stays sharp.
fn h264_size(width: u32, height: u32) -> Result<(usize, usize)> {
    let (w, h) = (
        (width as usize).next_multiple_of(2),
        (height as usize).next_multiple_of(2),
    );
    ensure!(
        w >= 16 && h >= 16,
        "H.264 needs at least 16 pixels on each edge; use JPEG for this size"
    );
    ensure!(
        w.max(h) <= 3840 && w.min(h) <= 2160,
        "H.264 supports up to 3840×2160 (or portrait); choose a maximum width or use JPEG"
    );
    Ok((w, h))
}

/// Retain conversion buffers across frames. Opaque, even-sized native captures
/// go straight from RGBA to I420. Other unscaled frames and exact 2:1
/// reductions are written once into `rgba`, padded and composited on black;
/// other ratios still resize through `stream_rgb`.
#[derive(Default)]
pub(super) struct YuvConverter {
    buffer: Option<YUVBuffer>,
    /// Cast's RGB canvas.
    padded: Vec<u8>,
    /// Even-sized RGBA staging for `convert`.
    rgba: Vec<u8>,
}

impl YuvConverter {
    /// Fit the selected source in the fixed negotiated Cast canvas, retaining
    /// aspect ratio and compositing transparency on black. Capture source
    /// resizing cannot silently change the stream's negotiated dimensions.
    pub fn fit(&mut self, frame: &CapturedFrame, width: u32, height: u32) -> Result<&YUVBuffer> {
        ensure!(
            width >= 320
                && height >= 240
                && width <= 1920
                && height <= 1080
                && width % 2 == 0
                && height % 2 == 0,
            "Invalid Cast canvas"
        );
        let image = &frame.image;
        let (sw, sh) = image.dimensions();
        ensure!(sw > 0 && sh > 0, "Empty captured source");
        let (fw, fh) = if u64::from(sw) * u64::from(height) > u64::from(sh) * u64::from(width) {
            (
                width,
                (u64::from(sh) * u64::from(width) / u64::from(sw)).max(1) as u32,
            )
        } else {
            (
                (u64::from(sw) * u64::from(height) / u64::from(sh)).max(1) as u32,
                height,
            )
        };
        let (left, top) = ((width - fw) / 2, (height - fh) / 2);
        self.padded.resize((width * height * 3) as usize, 0);
        self.padded.fill(0);
        for y in 0..fh {
            let sy = (u64::from(y) * u64::from(sh) / u64::from(fh)) as u32;
            for x in 0..fw {
                let sx = (u64::from(x) * u64::from(sw) / u64::from(fw)) as u32;
                let pixel = image.get_pixel(sx, sy).0;
                let at = (((y + top) * width + x + left) * 3) as usize;
                for channel in 0..3 {
                    self.padded[at + channel] =
                        (u16::from(pixel[channel]) * u16::from(pixel[3]) / 255) as u8;
                }
            }
        }
        let dimensions = (width as usize, height as usize);
        if self
            .buffer
            .as_ref()
            .is_none_or(|b| b.dimensions() != dimensions)
        {
            self.buffer = Some(YUVBuffer::new(dimensions.0, dimensions.1));
        }
        let buffer = self.buffer.as_mut().unwrap();
        buffer.read_rgb8(RgbSliceU8::new(&self.padded, dimensions));
        Ok(buffer)
    }
    pub fn convert(&mut self, raw: &RawFrame) -> Result<&YUVBuffer> {
        let (width, height) = raw
            .frame
            .stream_dimensions(raw.config.max_width, raw.config.pixel_mode)?;
        let (w, h) = h264_size(width, height)?;
        if self
            .buffer
            .as_ref()
            .is_none_or(|buffer| buffer.dimensions() != (w, h))
        {
            self.buffer = Some(YUVBuffer::new(w, h));
        }
        let buffer = self.buffer.as_mut().unwrap();
        let image = &raw.frame.image;
        let source = (image.width() as usize, image.height() as usize);
        let pixels = &image.as_raw()[..source.0 * source.1 * 4];
        let size = (width as usize, height as usize);
        if source == size {
            if size == (w, h) && opaque(pixels) {
                buffer.read_rgba8(RgbaSliceU8::new(pixels, (w, h)));
                return Ok(buffer);
            }
            let stride = size.0 * 4;
            // Only rows with translucent pixels pay for compositing.
            stage(&mut self.rgba, (w, h), size, |y, row| {
                let source = &pixels[y * stride..][..stride];
                if opaque(source) {
                    row.copy_from_slice(source);
                } else {
                    composite(source, row);
                }
            });
        } else if source == (size.0 * 2, size.1 * 2) {
            // The common HiDPI logical mode. A box is a little sharper than
            // the triangle filter `resize` (and JPEG) use, and much cheaper.
            let stride = source.0 * 4;
            stage(&mut self.rgba, (w, h), size, |y, row| {
                let rows = &pixels[2 * y * stride..][..2 * stride];
                halve(&rows[..stride], &rows[stride..], row);
                if !opaque(row) {
                    composite_in_place(row);
                }
            });
        } else {
            // Other ratios need `imageops::resize`, which the main crate
            // cannot call outside tests, so they still use `stream_rgb`.
            let rgb = raw
                .frame
                .stream_rgb(raw.config.max_width, raw.config.pixel_mode)?;
            let rgb = rgb.as_raw();
            if size == (w, h) {
                buffer.read_rgb8(RgbSliceU8::new(rgb, (w, h)));
                return Ok(buffer);
            }
            let stride = size.0 * 3;
            stage(&mut self.rgba, (w, h), size, |y, row| {
                let source = rgb[y * stride..][..stride].as_chunks::<3>().0;
                for (out, &[r, g, b]) in row.as_chunks_mut::<4>().0.iter_mut().zip(source) {
                    *out = [r, g, b, 255];
                }
            });
        }
        buffer.read_rgba8(RgbaSliceU8::new(&self.rgba, (w, h)));
        Ok(buffer)
    }
}

/// Write a `width`×`height` image into the even `(w, h)` staging buffer, one
/// row at a time from `fill`, which also composites translucent rows. Odd
/// edges repeat the last column and row.
fn stage(
    rgba: &mut Vec<u8>,
    (w, h): (usize, usize),
    (width, height): (usize, usize),
    mut fill: impl FnMut(usize, &mut [u8]),
) {
    if rgba.len() != w * h * 4 {
        *rgba = vec![0; w * h * 4];
    }
    let stride = w * 4;
    for (y, row) in rgba.chunks_exact_mut(stride).take(height).enumerate() {
        fill(y, &mut row[..width * 4]);
        if width < w {
            row.copy_within((width - 1) * 4..width * 4, width * 4);
        }
    }
    if height < h {
        rgba.copy_within((height - 1) * stride..height * stride, height * stride);
    }
}

/// Whether every RGBA pixel has alpha 255. ANDing aligned words vectorizes,
/// and checking per block still stops early on translucent frames.
fn opaque(rgba: &[u8]) -> bool {
    // SAFETY: every bit pattern is a valid u32.
    let (prefix, words, suffix) = unsafe { rgba.align_to::<u32>() };
    // Pixels start at byte 0, so an unaligned start moves the alpha byte
    // within each word.
    let mut alpha = [0; 4];
    alpha[(3 + 4 - prefix.len() % 4) % 4] = 255;
    let alpha = u32::from_ne_bytes(alpha);
    let bytes_opaque = |start: usize, bytes: &[u8]| {
        bytes
            .iter()
            .enumerate()
            .all(|(i, &byte)| (start + i) % 4 != 3 || byte == 255)
    };
    bytes_opaque(0, prefix)
        && bytes_opaque(rgba.len() - suffix.len(), suffix)
        && words
            .chunks(1024)
            .all(|block| block.iter().fold(alpha, |all, &word| all & word) == alpha)
}

/// One row of an exact 2:1 reduction: each output pixel is the rounded mean
/// of a 2×2 block of straight RGBA, averaged before compositing as `resize`
/// output is. Alternate bytes of each pixel word are summed in 16-bit lanes,
/// which vectorizes across pixels. Inlined into its caller, LLVM vectorized
/// only within each pixel, at a quarter of the speed.
#[inline(never)]
fn halve(top: &[u8], bottom: &[u8], out: &mut [u8]) {
    const LOW: u32 = 0x00ff_00ff;
    let blocks = top.as_chunks::<8>().0.iter().zip(bottom.as_chunks::<8>().0);
    for ((top, bottom), out) in blocks.zip(out.as_chunks_mut::<4>().0) {
        let [a, b, c, d] = [&top[..4], &top[4..], &bottom[..4], &bottom[4..]]
            .map(|pixel| u32::from_ne_bytes(pixel.try_into().unwrap()));
        // Four bytes sum to at most 1020, so no lane carries into the next.
        let even = (a & LOW) + (b & LOW) + (c & LOW) + (d & LOW) + 0x0002_0002;
        let odd =
            ((a >> 8) & LOW) + ((b >> 8) & LOW) + ((c >> 8) & LOW) + ((d >> 8) & LOW) + 0x0002_0002;
        *out = (((even >> 2) & LOW) | ((odd << 6) & !LOW)).to_ne_bytes();
    }
}

/// Straight alpha over black, with `stream_rgb`'s rounding. The result is
/// opaque; writing whole pixels keeps the loops below vectorized (skipping
/// the alpha byte made aarch64 store bytes one at a time).
fn over_black([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    let over = |c: u8| (u16::from(c) * u16::from(a) / 255) as u8;
    [over(r), over(g), over(b), 255]
}

/// Copy a row, compositing it over black in the same pass. Out of line, like
/// `halve`, so inlining cannot change how the loop vectorizes.
#[inline(never)]
fn composite(source: &[u8], out: &mut [u8]) {
    let pixels = out.as_chunks_mut::<4>().0.iter_mut();
    for (out, &pixel) in pixels.zip(source.as_chunks::<4>().0) {
        *out = over_black(pixel);
    }
}

/// `composite` for a row that is already in place.
#[inline(never)]
fn composite_in_place(row: &mut [u8]) {
    for pixel in row.as_chunks_mut::<4>().0 {
        *pixel = over_black(*pixel);
    }
}

#[cfg(test)]
fn yuv(raw: &RawFrame) -> Result<YUVBuffer> {
    let mut converter = YuvConverter::default();
    converter.convert(raw)?;
    Ok(converter.buffer.unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omabeam_capture::PixelMode;
    use openh264::{decoder::Decoder, formats::YUVSource};
    use std::{hint::black_box, time::Duration};

    /// HEAD's `convert`, verbatim, kept as the reference for output and timing.
    fn reference_convert<'a>(
        converter: &'a mut YuvConverter,
        raw: &RawFrame,
    ) -> Result<&'a YUVBuffer> {
        let (width, height) = raw
            .frame
            .stream_dimensions(raw.config.max_width, raw.config.pixel_mode)?;
        let (w, h) = (
            width.next_multiple_of(2) as usize,
            height.next_multiple_of(2) as usize,
        );
        ensure!(
            w >= 16 && h >= 16,
            "H.264 needs at least 16 pixels on each edge; use JPEG for this size"
        );
        ensure!(
            w.max(h) <= 3840 && w.min(h) <= 2160,
            "H.264 supports up to 3840×2160 (or portrait); choose a maximum width or use JPEG"
        );
        if converter
            .buffer
            .as_ref()
            .is_none_or(|buffer| buffer.dimensions() != (w, h))
        {
            converter.buffer = Some(YUVBuffer::new(w, h));
        }
        let buffer = converter.buffer.as_mut().unwrap();
        let image = &raw.frame.image;
        if image.dimensions() == (w as u32, h as u32)
            && (width as usize, height as usize) == (w, h)
            && image.as_raw().chunks_exact(4).all(|p| p[3] == 255)
        {
            buffer.read_rgba8(RgbaSliceU8::new(image.as_raw(), (w, h)));
            return Ok(buffer);
        }
        let rgb = raw
            .frame
            .stream_rgb(raw.config.max_width, raw.config.pixel_mode)?;
        if (w, h) == (width as usize, height as usize) {
            buffer.read_rgb8(RgbSliceU8::new(rgb.as_raw(), (w, h)));
            return Ok(buffer);
        }
        // Replicate odd edges; never shrink native text to an even resolution.
        converter.padded.resize(w * h * 3, 0);
        for y in 0..h {
            for x in 0..w {
                converter.padded[(y * w + x) * 3..(y * w + x + 1) * 3].copy_from_slice(
                    &rgb.get_pixel((x as u32).min(width - 1), (y as u32).min(height - 1))
                        .0,
                );
            }
        }
        buffer.read_rgb8(RgbSliceU8::new(&converter.padded, (w, h)));
        Ok(buffer)
    }

    #[derive(Clone, Copy, Debug)]
    enum Alpha {
        Opaque,
        Half,
        /// Every third row opaque; the others random, including 0 and 255.
        Rows,
        /// Only the last pixel of each odd row is transparent, so a row check
        /// that stops early misses it.
        LastPixel,
    }

    /// Deterministic noise, so a failure reproduces.
    fn noise(seed: u64) -> impl FnMut() -> u8 {
        let mut state = (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        }
    }

    /// Smooth ramps, where a 2×2 box and `resize` agree closely, or noise,
    /// where any misplaced byte shows.
    fn frame(
        (width, height): (u32, u32),
        (logical_width, logical_height): (u32, u32),
        alpha: Alpha,
        smooth: bool,
        seed: u64,
    ) -> CapturedFrame {
        let mut next = noise(seed);
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            let [r, g, b] = if smooth {
                [
                    (40 + x * 150 / width) as u8,
                    (40 + y * 150 / height) as u8,
                    90,
                ]
            } else {
                [next(), next(), next()]
            };
            let a = match alpha {
                Alpha::Opaque => 255,
                Alpha::Half => 127,
                Alpha::Rows if y % 3 == 0 => 255,
                Alpha::Rows => next(),
                Alpha::LastPixel if x == width - 1 && y % 2 == 1 => 0,
                Alpha::LastPixel => 255,
            };
            image::Rgba([r, g, b, a])
        });
        CapturedFrame {
            image,
            logical_width,
            logical_height,
        }
    }

    fn raw(frame: CapturedFrame, config: &LiveConfig) -> RawFrame {
        RawFrame {
            frame,
            config: config.clone(),
            captured_at: Instant::now(),
        }
    }

    fn max_difference(a: &YUVBuffer, b: &YUVBuffer) -> u8 {
        assert_eq!(a.dimensions(), b.dimensions());
        [(a.y(), b.y()), (a.u(), b.u()), (a.v(), b.v())]
            .into_iter()
            .flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| a.abs_diff(*b)))
            .max()
            .unwrap()
    }

    #[test]
    fn cast_canvas_letterboxes_portrait_and_composites_alpha() {
        let frame = CapturedFrame {
            image: image::RgbaImage::from_pixel(64, 128, image::Rgba([255, 0, 0, 128])),
            logical_width: 64,
            logical_height: 128,
        };
        let mut converter = YuvConverter::default();
        let yuv = converter.fit(&frame, 640, 360).unwrap();
        assert_eq!(yuv.dimensions(), (640, 360));
        let black = yuv.y()[180 * 640];
        assert_eq!(yuv.y()[180 * 640 + 229], black);
        assert!(yuv.y()[180 * 640 + 230] > black + 20);
        assert!(yuv.y()[180 * 640 + 409] > black + 20);
        assert_eq!(yuv.y()[180 * 640 + 410], black);
        let opaque =
            YUVBuffer::from_rgb8_source(RgbSliceU8::new(&vec![255; 640 * 360 * 3], (640, 360)));
        assert!(yuv.y()[180 * 640 + 320] < opaque.y()[180 * 640 + 320] / 2);
        assert!(converter.fit(&frame, 319, 240).is_err());
    }

    #[test]
    fn reusable_conversion_preserves_alpha_scaling_and_odd_edges() {
        let logical = LiveConfig::default();
        // Native pixels limited to half their width halve exactly too.
        let native = LiveConfig {
            pixel_mode: PixelMode::Native,
            max_width: Some(32),
            ..LiveConfig::default()
        };
        let mut converter = YuvConverter::default();
        let mut reference = YuvConverter::default();
        // (dimensions, address) of the last YUV buffer and (length, address)
        // of the last staging buffer.
        let mut previous_yuv: Option<((usize, usize), *const u8)> = None;
        let mut previous_staging: Option<(usize, *const u8)> = None;
        // (captured, logical, alpha, config, staged, tolerance against HEAD)
        for (index, (size, logical_size, alpha, config, staged, tolerance)) in [
            // Opaque, unscaled and even: straight from the capture.
            ((32, 18), (32, 18), Alpha::Opaque, &logical, false, 0),
            ((32, 18), (32, 18), Alpha::Opaque, &logical, false, 0),
            // Translucent, partly translucent, or odd: staged and padded.
            ((32, 18), (32, 18), Alpha::Half, &logical, true, 0),
            ((32, 18), (32, 18), Alpha::Rows, &logical, true, 0),
            ((31, 17), (31, 17), Alpha::Opaque, &logical, true, 0),
            ((31, 17), (31, 17), Alpha::Rows, &logical, true, 0),
            // One odd edge at a time.
            ((31, 18), (31, 18), Alpha::Rows, &logical, true, 0),
            ((32, 17), (32, 17), Alpha::Rows, &logical, true, 0),
            ((32, 18), (32, 18), Alpha::LastPixel, &logical, true, 0),
            // Other ratios still resize; only odd results need staging.
            ((32, 18), (16, 16), Alpha::Half, &logical, false, 0),
            ((33, 19), (17, 15), Alpha::Rows, &logical, true, 0),
            // Nearly 2:1 is not 2:1.
            ((65, 36), (32, 18), Alpha::Opaque, &logical, false, 0),
            ((64, 37), (32, 18), Alpha::Opaque, &logical, false, 0),
            // Exact halving uses a 2×2 box instead of the triangle filter.
            ((64, 36), (32, 18), Alpha::Opaque, &logical, true, 3),
            ((64, 36), (32, 18), Alpha::Half, &logical, true, 3),
            ((62, 34), (31, 17), Alpha::Opaque, &logical, true, 3),
            ((64, 36), (64, 36), Alpha::Half, &native, true, 3),
        ]
        .into_iter()
        .enumerate()
        {
            let smooth = tolerance > 0;
            let raw = raw(
                frame(size, logical_size, alpha, smooth, index as u64),
                config,
            );
            let expected = reference_convert(&mut reference, &raw).unwrap();
            converter.rgba.fill(0xa5);
            let actual = converter.convert(&raw).unwrap();
            let difference = max_difference(actual, expected);
            assert!(
                difference <= tolerance,
                "case {index}: YUV differs from HEAD by {difference}"
            );
            let storage = (actual.dimensions(), actual.y().as_ptr());
            let staging = (converter.rgba.len(), converter.rgba.as_ptr());
            let touched = converter.rgba.iter().any(|&byte| byte != 0xa5);
            assert_eq!(touched, staged, "case {index}: staging use");
            if let Some((dimensions, address)) = previous_yuv
                && dimensions == storage.0
            {
                assert_eq!(
                    address, storage.1,
                    "case {index}: same-size YUV reallocated"
                );
            }
            if let Some((len, address)) = previous_staging
                && len == staging.0
            {
                assert_eq!(
                    address, staging.1,
                    "case {index}: same-size staging reallocated"
                );
            }
            (previous_yuv, previous_staging) = (Some(storage), Some(staging));
        }
    }

    #[test]
    fn exact_halving_averages_each_block_then_composites_over_black() {
        let native = LiveConfig {
            pixel_mode: PixelMode::Native,
            max_width: Some(40),
            ..LiveConfig::default()
        };
        let mut converter = YuvConverter::default();
        for (index, (size, logical, config)) in [
            ((64, 36), (32, 18), &LiveConfig::default()),
            ((62, 34), (31, 17), &LiveConfig::default()),
            ((80, 46), (80, 46), &native),
        ]
        .into_iter()
        .enumerate()
        {
            for alpha in [Alpha::Opaque, Alpha::Rows, Alpha::LastPixel] {
                let raw = raw(frame(size, logical, alpha, false, index as u64), config);
                let image = &raw.frame.image;
                let (width, height) = (size.0 / 2, size.1 / 2);
                let expected = image::RgbImage::from_fn(
                    width.next_multiple_of(2),
                    height.next_multiple_of(2),
                    |x, y| {
                        let (x, y) = (x.min(width - 1), y.min(height - 1));
                        let block = [(0, 0), (1, 0), (0, 1), (1, 1)]
                            .map(|(dx, dy)| image.get_pixel(2 * x + dx, 2 * y + dy).0);
                        let mean =
                            |c: usize| (block.iter().map(|p| u32::from(p[c])).sum::<u32>() + 2) / 4;
                        let alpha = mean(3);
                        image::Rgb([0, 1, 2].map(|c| (mean(c) * alpha / 255) as u8))
                    },
                );
                let expected = YUVBuffer::from_rgb8_source(RgbSliceU8::new(
                    expected.as_raw(),
                    (expected.width() as usize, expected.height() as usize),
                ));
                let actual = converter.convert(&raw).unwrap();
                assert_eq!(
                    max_difference(actual, &expected),
                    0,
                    "case {index}, {alpha:?}"
                );
            }
        }
    }

    #[test]
    fn opacity_scan_reads_only_alpha_at_any_alignment() {
        assert!(opaque(&[]));
        for offset in 0..4 {
            for pixels in [1, 2, 7, 2500] {
                // Shift the start so every prefix and suffix length occurs.
                let mut bytes = vec![0; offset + pixels * 4 + 3];
                let rgba = &mut bytes[offset..offset + pixels * 4];
                for pixel in rgba.as_chunks_mut::<4>().0 {
                    *pixel = [0, 128, 254, 255];
                }
                assert!(opaque(rgba), "offset {offset}, {pixels} px");
                for at in [0, pixels / 2, pixels - 1] {
                    rgba[at * 4 + 3] = 254;
                    assert!(!opaque(rgba), "offset {offset}, pixel {at} of {pixels}");
                    rgba[at * 4 + 3] = 255;
                }
            }
        }
    }

    #[test]
    fn software_probe_encodes_each_requested_size() {
        let config = LiveConfig {
            encoder: EncoderMode::Software,
            fps: 30,
            ..Default::default()
        };
        let report = probe(&config, &[(64, 36), (321, 181), (180, 320)]).unwrap();
        let sizes = report["sizes"].as_array().expect("a sizes array");
        let dimensions: Vec<_> = sizes
            .iter()
            .map(|size| (size["width"].as_u64(), size["height"].as_u64()))
            .collect();
        // Odd sizes round up to even, as a stream's padding does.
        assert_eq!(
            dimensions,
            [(64, 36), (322, 182), (180, 320)].map(|(w, h)| (Some(w), Some(h)))
        );
        for size in sizes.iter().chain([&report]) {
            assert_eq!(size["encoder"], "OpenH264 software");
            assert_eq!(size["hardware"], false);
            assert!(size["note"].is_null());
        }
        // The single-size fields stay at the top level.
        assert_eq!(
            (report["width"].as_u64(), report["height"].as_u64()),
            (Some(64), Some(36))
        );
        // Every size meets H.264's limits before any encoder starts.
        for (width, height) in [(14, 360), (3842, 2160), (2162, 3840)] {
            let error = format!(
                "{:#}",
                probe(&config, &[(640, 360), (width, height)]).unwrap_err()
            );
            assert!(error.contains(&format!("{width}×{height}")), "{error}");
        }
    }

    #[test]
    fn hardware_probe_fails_instead_of_falling_back() {
        // Test binaries have no omabeam-encoder helper beside them.
        let config = LiveConfig {
            encoder: EncoderMode::Hardware,
            ..Default::default()
        };
        let error = format!("{:#}", probe(&config, &[(64, 36), (128, 72)]).unwrap_err());
        assert!(
            error.contains("64×36") && error.contains("helper"),
            "{error}"
        );
    }

    #[test]
    fn probe_summary_is_the_first_size_without_hardware() {
        let size = |width: u32, hardware: bool| {
            serde_json::json!({
                "encoder": if hardware { "GPU" } else { "OpenH264 software" },
                "hardware": hardware,
                "note": (!hardware).then_some("fell back"),
                "width": width,
                "height": 2,
            })
        };
        let report = probe_report(vec![size(1, true), size(2, false), size(3, false)]);
        assert_eq!(report["width"], 2);
        assert_eq!(report["hardware"], false);
        assert_eq!(report["note"], "fell back");
        assert_eq!(report["sizes"].as_array().map(Vec::len), Some(3));
        let report = probe_report(vec![size(1, true), size(2, true)]);
        assert_eq!(report["width"], 1);
        assert_eq!(report["encoder"], "GPU");
        assert_eq!(report["hardware"], true);
    }

    #[test]
    #[ignore = "timing report: cargo test --release --lib report_conversion_timings -- --ignored --nocapture"]
    fn report_conversion_timings() {
        let median = |mut times: Vec<Duration>| {
            times.sort();
            times[times.len() / 2].as_secs_f64() * 1000.0
        };
        for (label, size, logical, alpha) in [
            ("1920×1080 opaque", (1920, 1080), (1920, 1080), 255),
            ("2880×1800 to 1440×900", (2880, 1800), (1440, 900), 255),
            ("1279×719 opaque", (1279, 719), (1279, 719), 255),
            // The staged path's worst case: every row composited.
            ("1920×1080 translucent", (1920, 1080), (1920, 1080), 230),
        ] {
            // Screen-like: ramps broken by flat blocks.
            let image = image::RgbaImage::from_fn(size.0, size.1, |x, y| {
                if (x / 64 + y / 48) % 3 == 0 {
                    image::Rgba([236, 236, 236, alpha])
                } else {
                    image::Rgba([x as u8, y as u8, (x + y) as u8, alpha])
                }
            });
            let frame = CapturedFrame {
                image,
                logical_width: logical.0,
                logical_height: logical.1,
            };
            let raw = raw(frame, &LiveConfig::default());
            let (mut before, mut after) = (YuvConverter::default(), YuvConverter::default());
            let (mut old, mut new) = (Vec::new(), Vec::new());
            for _ in 0..31 {
                let started = Instant::now();
                black_box(reference_convert(&mut before, &raw).unwrap());
                old.push(started.elapsed());
                let started = Instant::now();
                black_box(after.convert(&raw).unwrap());
                new.push(started.elapsed());
            }
            if size == logical {
                let expected = reference_convert(&mut before, &raw).unwrap();
                assert_eq!(max_difference(after.convert(&raw).unwrap(), expected), 0);
            }
            println!(
                "{label}: before {:.3} ms, after {:.3} ms (medians of 31)",
                median(old),
                median(new)
            );
        }
    }

    #[test]
    fn software_encoder_emits_a_bitstream_for_every_changed_frame() {
        let config = LiveConfig {
            fps: 60,
            h264_bitrate: 100_000,
            webrtc: true,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let mut encoder = create(&config, true).unwrap();
        for index in 0..24 {
            let raw = RawFrame {
                frame: omabeam_capture::demo_frame(index * 11),
                config: config.clone(),
                captured_at: Instant::now(),
            };
            let yuv = yuv(&raw).unwrap();
            let bitstream = encoder
                .encode_at(&yuv, Timestamp::from_millis(index as u64 * 16))
                .unwrap();
            assert!(!bitstream.to_vec().is_empty(), "frame {index} was skipped");
        }
    }

    #[test]
    fn h264_decodes_odd_edges_keyframe_recovery_and_resolution_changes() {
        let config = LiveConfig {
            webrtc: true,
            ..Default::default()
        };
        let mut encoder = create(&config, true).unwrap();
        let mut decoder = Decoder::new().unwrap();
        for (index, (w, h)) in [(641, 361), (641, 361), (480, 270), (17, 17)]
            .into_iter()
            .enumerate()
        {
            let mut frame = omabeam_capture::demo_frame(index as u32);
            frame.logical_width = w;
            frame.logical_height = h;
            let raw = RawFrame {
                frame,
                config: config.clone(),
                captured_at: Instant::now(),
            };
            let yuv = yuv(&raw).unwrap();
            encoder.force_intra_frame();
            let bitstream = encoder
                .encode_at(&yuv, Timestamp::from_millis(index as u64 * 1000))
                .unwrap();
            assert_eq!(bitstream.frame_type(), FrameType::IDR);
            let bytes = bitstream.to_vec();
            let decoded = decoder
                .decode(&bytes)
                .unwrap()
                .expect("an actual decoded frame");
            assert_eq!(
                decoded.dimensions(),
                (
                    w.next_multiple_of(2) as usize,
                    h.next_multiple_of(2) as usize
                )
            );
        }
    }

    #[test]
    fn oversized_native_images_report_a_fallback_instead_of_silently_downscaling() {
        let mut frame = omabeam_capture::demo_frame(0);
        frame.logical_width = 8000;
        let raw = RawFrame {
            frame,
            config: LiveConfig::default(),
            captured_at: Instant::now(),
        };
        assert!(yuv(&raw).err().unwrap().to_string().contains("3840"));
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn fixture(mode: &str) -> (tempfile::TempDir, YUVBuffer, AdaptiveEncoder) {
        let directory = tempfile::tempdir().unwrap();
        let config = LiveConfig::default();
        let mut frame = omabeam_capture::demo_frame(0);
        frame.logical_width = 64;
        frame.logical_height = 64;
        let pixels = yuv(&RawFrame {
            frame,
            config: config.clone(),
            captured_at: Instant::now(),
        })
        .unwrap();
        let mut software = create(&config, true).unwrap();
        software.force_intra_frame();
        let packet = software.encode(&pixels).unwrap().to_vec();
        std::fs::write(directory.path().join("frame.h264"), packet).unwrap();
        std::fs::write(directory.path().join("mode"), mode).unwrap();
        let helper = directory.path().join("helper");
        std::fs::write(
            &helper,
            r#"#!/usr/bin/env python3
import json, pathlib, struct, sys, time
root = pathlib.Path(__file__).parent
mode = (root / 'mode').read_text()
source, target = sys.stdin.buffer, sys.stdout.buffer
size = struct.unpack('<I', source.read(4))[0]
config = json.loads(source.read(size))
length = config['width'] * config['height'] * 3 // 2
packet = (root / 'frame.h264').read_bytes()
for index in range(2):
    if len(source.read(9 + length)) != 9 + length: sys.exit(0)
    if index == 1:
        if mode == 'stall': time.sleep(30)
        elif mode == 'oversized':
            target.write(struct.pack('<I', 0xffffffff)); target.flush(); time.sleep(30)
        else: sys.exit(1)
    header = json.dumps({'encoder': 'Fixture hardware', 'bytes': len(packet)}).encode()
    target.write(struct.pack('<I', len(header)) + header + packet); target.flush()
"#,
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut encoder = AdaptiveEncoder::new(&config).unwrap();
        encoder.hardware = Some(
            hardware::Hardware::spawn(
                &helper,
                &omabeam_encoder::Config {
                    version: omabeam_encoder::VERSION,
                    width: 64,
                    height: 64,
                    fps: 15,
                    bitrate: config.h264_bitrate,
                    gop_frames: 0,
                },
            )
            .unwrap(),
        );
        encoder.attempted = true;
        (directory, pixels, encoder)
    }

    #[test]
    fn cast_keeps_deltas_past_the_two_second_keyframe_clock() {
        let config = LiveConfig {
            fps: 30,
            h264_bitrate: 2_000_000,
            webrtc: true,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let mut cast = AdaptiveEncoder::new(&config).unwrap();
        cast.idr_only_when_requested().unwrap();
        let mut browser = AdaptiveEncoder::new(&config).unwrap();
        let mut cast_idrs = Vec::new();
        let mut browser_idrs = Vec::new();
        for index in 0..70u32 {
            let frame = RawFrame {
                frame: omabeam_capture::demo_frame(index),
                config: config.clone(),
                captured_at: Instant::now(),
            };
            let pixels = yuv(&frame).unwrap();
            let force = index == 0;
            let pts = i64::from(index) * 33_000;
            if cast.encode(&pixels, pts, force).unwrap().1 {
                cast_idrs.push(index);
            }
            if browser.encode(&pixels, pts, force).unwrap().1 {
                browser_idrs.push(index);
            }
        }
        assert_eq!(cast_idrs, vec![0], "cast inserted {cast_idrs:?}");
        assert!(
            browser_idrs.iter().any(|index| *index > 0),
            "browser no longer refreshes on its own: {browser_idrs:?}"
        );
    }

    #[test]
    fn bitrate_update_keeps_the_open_software_prediction_chain() {
        let config = LiveConfig {
            fps: 30,
            h264_bitrate: 4_000_000,
            webrtc: true,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let raw = RawFrame {
            frame: omabeam_capture::demo_frame(1),
            config: config.clone(),
            captured_at: Instant::now(),
        };
        let pixels = yuv(&raw).unwrap();
        let mut encoder = AdaptiveEncoder::new(&config).unwrap();
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        let (idr, idr_flag) = encoder.encode(&pixels, 0, true).unwrap();
        assert!(idr_flag && omabeam_encoder::inspect_h264(&idr).unwrap());
        decoder.decode(&idr).unwrap().unwrap();
        let (delta, delta_flag) = encoder.encode(&pixels, 100_000, false).unwrap();
        assert!(!delta_flag && !omabeam_encoder::inspect_h264(&delta).unwrap());
        decoder.decode(&delta).unwrap().unwrap();
        encoder.set_bitrate(1_000_000).unwrap();
        let (next, next_flag) = encoder.encode(&pixels, 200_000, false).unwrap();
        assert!(
            !next_flag && !omabeam_encoder::inspect_h264(&next).unwrap(),
            "a bitrate update must not insert an IDR"
        );
        assert!(decoder.decode(&next).unwrap().is_some());
        assert_eq!(encoder.name, "OpenH264 software");
    }

    #[test]
    fn helper_failure_recovers_with_a_decodable_software_idr_and_does_not_retry() {
        for mode in ["crash", "stall", "oversized"] {
            let (_directory, pixels, mut encoder) = fixture(mode);
            assert!(encoder.encode(&pixels, 0, true).unwrap().1);
            assert_eq!(encoder.name, "Fixture hardware");
            let started = Instant::now();
            let (packet, idr) = encoder.encode(&pixels, 100_000, false).unwrap();
            assert!(started.elapsed() < Duration::from_secs(3), "{mode}");
            assert!(idr && omabeam_encoder::inspect_h264(&packet).unwrap());
            let mut decoder = openh264::decoder::Decoder::new().unwrap();
            assert_eq!(
                decoder.decode(&packet).unwrap().unwrap().dimensions(),
                (64, 64)
            );
            assert_eq!(encoder.name, "OpenH264 software");
            assert!(encoder.note.is_some() && encoder.hardware.is_none() && encoder.attempted);
            encoder.set_bitrate(1_000_000).unwrap();
            assert!(encoder.note.is_some() && encoder.hardware.is_none() && encoder.attempted);
            encoder.encode(&pixels, 200_000, false).unwrap();
            assert_eq!(encoder.name, "OpenH264 software");
        }
    }

    #[test]
    fn explicit_hardware_mode_reports_failure_instead_of_silently_using_software() {
        let (_directory, pixels, mut encoder) = fixture("crash");
        encoder.config.encoder = EncoderMode::Hardware;
        encoder.encode(&pixels, 0, true).unwrap();
        assert!(encoder.encode(&pixels, 100_000, false).is_err());
    }
}
