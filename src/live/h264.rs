//! Shared capture conversion and H.264 backends for browser and Cast.
mod hardware;
use super::LiveConfig;
use crate::live::config::EncoderMode;
use anyhow::{Result, ensure};
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
use std::time::Instant;

pub(super) struct RawFrame {
    pub frame: CapturedFrame,
    pub config: LiveConfig,
    pub captured_at: Instant,
}

fn create(config: &LiveConfig) -> Result<Encoder> {
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
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.fps * 2)),
    )?)
}

pub(super) struct AdaptiveEncoder {
    config: LiveConfig,
    software: Encoder,
    hardware: Option<hardware::Hardware>,
    attempted: bool,
    pub name: String,
    pub note: Option<String>,
}
impl AdaptiveEncoder {
    /// Reset the prediction chain when applying a new congestion-control
    /// target. Reopening also covers hardware backends without runtime rate
    /// controls; callers rate-limit these changes and force a fresh IDR.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        if bitrate == self.config.h264_bitrate {
            return Ok(());
        }
        let mut config = self.config.clone();
        config.h264_bitrate = bitrate;
        let mut next = Self::new(&config)?;
        if self.attempted && self.hardware.is_none() && config.encoder == EncoderMode::Auto {
            // A congestion update must not repeatedly probe a failed driver.
            // Keep the same per-session software fallback policy as WebRTC.
            next.attempted = true;
            next.note = self.note.clone();
        }
        *self = next;
        Ok(())
    }
    pub fn new(config: &LiveConfig) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            software: create(config)?,
            hardware: None,
            attempted: config.encoder == EncoderMode::Software,
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
                self.hardware = Some(hardware::Hardware::new(&omabeam_encoder::Config {
                    version: omabeam_encoder::VERSION,
                    width: w as u32,
                    height: h as u32,
                    fps: self.config.fps.min(60),
                    bitrate: self.config.h264_bitrate,
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
        Ok((bitstream.to_vec(), bitstream.frame_type() == FrameType::IDR))
    }
}

pub fn probe(config: &LiveConfig) -> Result<serde_json::Value> {
    let mut config = config.clone();
    config.fps = config.fps.min(60);
    let raw = RawFrame {
        frame: omabeam_capture::demo_frame(0),
        config: config.clone(),
        captured_at: Instant::now(),
    };
    let mut converter = YuvConverter::default();
    let yuv = converter.convert(&raw)?;
    let mut encoder = AdaptiveEncoder::new(&config)?;
    // Verify first frame, a delta, and a forced IDR at the negotiated profile.
    for (pts, force) in [(0, true), (100_000, false), (200_000, true)] {
        let (bytes, _) = encoder.encode(yuv, pts, force)?;
        let idr = omabeam_encoder::inspect_h264(&bytes)?;
        ensure!(
            !force || idr,
            "encoder probe did not return a requested IDR"
        );
    }
    Ok(
        serde_json::json!({ "encoder": encoder.name, "hardware": encoder.hardware.is_some(),
        "note": encoder.note, "width": yuv.dimensions().0, "height": yuv.dimensions().1 }),
    )
}

/// Retain conversion buffers across frames. Opaque native captures go directly
/// from RGBA to I420, without allocating/compositing an intermediate RGB image.
#[derive(Default)]
pub(super) struct YuvConverter {
    buffer: Option<YUVBuffer>,
    padded: Vec<u8>,
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
        if self
            .buffer
            .as_ref()
            .is_none_or(|buffer| buffer.dimensions() != (w, h))
        {
            self.buffer = Some(YUVBuffer::new(w, h));
        }
        let buffer = self.buffer.as_mut().unwrap();
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
        self.padded.resize(w * h * 3, 0);
        for y in 0..h {
            for x in 0..w {
                self.padded[(y * w + x) * 3..(y * w + x + 1) * 3].copy_from_slice(
                    &rgb.get_pixel((x as u32).min(width - 1), (y as u32).min(height - 1))
                        .0,
                );
            }
        }
        buffer.read_rgb8(RgbSliceU8::new(&self.padded, (w, h)));
        Ok(buffer)
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
    use openh264::{decoder::Decoder, formats::YUVSource};

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
        let mut converter = YuvConverter::default();
        let mut previous_storage = None;
        for (w, h, alpha, scaled) in [
            (32, 18, 255, false),
            (32, 18, 255, false),
            (32, 18, 127, false),
            (31, 17, 255, false),
            (32, 18, 255, true),
        ] {
            let frame = CapturedFrame {
                image: image::RgbaImage::from_fn(w, h, |x, y| {
                    image::Rgba([(x * 7) as u8, (y * 11) as u8, 90, alpha])
                }),
                logical_width: if scaled { 16 } else { w },
                logical_height: if scaled { 16 } else { h },
            };
            let raw = RawFrame {
                frame,
                config: LiveConfig::default(),
                captured_at: Instant::now(),
            };
            let rgb = raw.frame.stream_rgb(None, raw.config.pixel_mode).unwrap();
            let (ew, eh) = (
                rgb.width().next_multiple_of(2),
                rgb.height().next_multiple_of(2),
            );
            let padded = image::RgbImage::from_fn(ew, eh, |x, y| {
                *rgb.get_pixel(x.min(rgb.width() - 1), y.min(rgb.height() - 1))
            });
            let expected = YUVBuffer::from_rgb8_source(RgbSliceU8::new(
                padded.as_raw(),
                (ew as usize, eh as usize),
            ));
            let actual = converter.convert(&raw).unwrap();
            assert_eq!(actual.dimensions(), expected.dimensions());
            for (a, b) in [
                (actual.y(), expected.y()),
                (actual.u(), expected.u()),
                (actual.v(), expected.v()),
            ] {
                assert!(
                    a.iter().zip(b).all(|(a, b)| a.abs_diff(*b) <= 1),
                    "color conversion changed"
                );
            }
            if let Some((dimensions, address)) = previous_storage {
                if dimensions == actual.dimensions() {
                    assert_eq!(
                        address,
                        actual.y().as_ptr(),
                        "same-size conversion reallocated"
                    );
                }
            }
            previous_storage = Some((actual.dimensions(), actual.y().as_ptr()));
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
        let mut encoder = create(&config).unwrap();
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
        let mut encoder = create(&config).unwrap();
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
        let mut software = create(&config).unwrap();
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
                },
            )
            .unwrap(),
        );
        encoder.attempted = true;
        (directory, pixels, encoder)
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
