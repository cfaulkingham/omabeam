use crate::Rect;
use anyhow::{Result, bail, ensure};
use image::{ImageEncoder, Rgba, RgbaImage, imageops};
use wayland_client::protocol::{wl_output::Transform, wl_shm::Format};

const MAX_BUFFER_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BufferSpec {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: Format,
}

impl BufferSpec {
    pub fn packed(width: u32, height: u32, format: Format) -> Result<Self> {
        let bpp = bytes_per_pixel(format)
            .ok_or_else(|| anyhow::anyhow!("unsupported capture pixel format: {format:?}"))?;
        let stride = width
            .checked_mul(bpp)
            .ok_or_else(|| anyhow::anyhow!("capture width overflow"))?;
        let spec = Self {
            width,
            height,
            stride,
            format,
        };
        spec.byte_len()?;
        Ok(spec)
    }

    pub fn byte_len(self) -> Result<usize> {
        let bpp = bytes_per_pixel(self.format).ok_or_else(|| {
            anyhow::anyhow!("unsupported capture pixel format: {:?}", self.format)
        })?;
        ensure!(
            self.width > 0 && self.height > 0 && self.width <= 32768 && self.height <= 32768,
            "invalid capture dimensions"
        );
        ensure!(
            self.stride >= self.width * bpp && self.stride <= i32::MAX as u32,
            "invalid capture stride"
        );
        let size = (self.stride as usize)
            .checked_mul(self.height as usize)
            .ok_or_else(|| anyhow::anyhow!("capture buffer size overflow"))?;
        ensure!(size <= MAX_BUFFER_BYTES, "capture buffer exceeds 512 MiB");
        Ok(size)
    }
}

pub(crate) fn bytes_per_pixel(format: Format) -> Option<u32> {
    match format {
        Format::Argb8888
        | Format::Xrgb8888
        | Format::Abgr8888
        | Format::Xbgr8888
        | Format::Xrgb2101010
        | Format::Argb2101010
        | Format::Xbgr2101010
        | Format::Abgr2101010 => Some(4),
        Format::Rgb888 | Format::Bgr888 => Some(3),
        Format::Rgb565 | Format::Bgr565 => Some(2),
        _ => None,
    }
}

/// Converts native-endian wl_shm packed pixels to straight-alpha RGBA.
/// Padding is ignored and Y inversion is applied before the output transform.
pub(crate) fn decode(
    bytes: &[u8],
    spec: BufferSpec,
    inverted: bool,
    transform: Transform,
) -> Result<RgbaImage> {
    ensure!(bytes.len() >= spec.byte_len()?, "truncated capture buffer");
    let bpp = bytes_per_pixel(spec.format).unwrap() as usize;
    let mut image = RgbaImage::new(spec.width, spec.height);
    for y in 0..spec.height {
        let source_y = if inverted { spec.height - 1 - y } else { y };
        let row = &bytes[source_y as usize * spec.stride as usize..];
        for x in 0..spec.width {
            let p = &row[x as usize * bpp..][..bpp];
            let (mut r, mut g, mut b, a) = match spec.format {
                Format::Argb8888 | Format::Xrgb8888 | Format::Abgr8888 | Format::Xbgr8888 => {
                    let v = u32::from_ne_bytes(p.try_into().unwrap());
                    let rgb = ((v >> 16) as u8, (v >> 8) as u8, v as u8);
                    let (r, g, b) = if matches!(spec.format, Format::Abgr8888 | Format::Xbgr8888) {
                        (rgb.2, rgb.1, rgb.0)
                    } else {
                        rgb
                    };
                    (
                        r,
                        g,
                        b,
                        if matches!(spec.format, Format::Argb8888 | Format::Abgr8888) {
                            (v >> 24) as u8
                        } else {
                            255
                        },
                    )
                }
                Format::Xrgb2101010
                | Format::Argb2101010
                | Format::Xbgr2101010
                | Format::Abgr2101010 => {
                    let v = u32::from_ne_bytes(p.try_into().unwrap());
                    let ten = |shift: u32| ((((v >> shift) & 1023u32) * 255 + 511) / 1023) as u8;
                    let (r, b) = if matches!(spec.format, Format::Xbgr2101010 | Format::Abgr2101010)
                    {
                        (ten(0), ten(20))
                    } else {
                        (ten(20), ten(0))
                    };
                    (
                        r,
                        ten(10),
                        b,
                        if matches!(spec.format, Format::Argb2101010 | Format::Abgr2101010) {
                            ((v >> 30) * 85) as u8
                        } else {
                            255
                        },
                    )
                }
                Format::Rgb565 | Format::Bgr565 => {
                    let v = u16::from_ne_bytes(p.try_into().unwrap());
                    let r = (((v >> 11) & 31) * 255 / 31) as u8;
                    let g = (((v >> 5) & 63) * 255 / 63) as u8;
                    let b = ((v & 31) * 255 / 31) as u8;
                    if spec.format == Format::Bgr565 {
                        (b, g, r, 255)
                    } else {
                        (r, g, b, 255)
                    }
                }
                Format::Rgb888 | Format::Bgr888 => {
                    let reverse = (spec.format == Format::Rgb888) == cfg!(target_endian = "little");
                    if reverse {
                        (p[2], p[1], p[0], 255)
                    } else {
                        (p[0], p[1], p[2], 255)
                    }
                }
                _ => bail!("unsupported capture pixel format"),
            };
            if a > 0 && a < 255 {
                let straight =
                    |v: u8| ((u32::from(v) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8;
                (r, g, b) = (straight(r), straight(g), straight(b));
            }
            image.put_pixel(x, y, Rgba([r, g, b, a]));
        }
    }
    // wl_output transforms describe the transform applied to the buffer.
    // Undo rotation (the protocol uses counter-clockwise angles), then reflection.
    let rotated = match transform {
        Transform::Normal | Transform::Flipped => image,
        Transform::_90 | Transform::Flipped90 => imageops::rotate90(&image),
        Transform::_180 | Transform::Flipped180 => imageops::rotate180(&image),
        Transform::_270 | Transform::Flipped270 => imageops::rotate270(&image),
        _ => bail!("unsupported capture transform"),
    };
    Ok(
        if matches!(
            transform,
            Transform::Flipped
                | Transform::Flipped90
                | Transform::Flipped180
                | Transform::Flipped270
        ) {
            imageops::flip_horizontal(&rotated)
        } else {
            rotated
        },
    )
}

/// Choose the pixel grid before applying the streaming width limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PixelMode {
    #[default]
    Logical,
    Native,
}

/// Pixels at capture resolution, with logical dimensions for scale-1 streaming.
#[derive(Debug)]
pub struct CapturedFrame {
    pub image: RgbaImage,
    pub logical_width: u32,
    pub logical_height: u32,
}

impl CapturedFrame {
    pub fn png(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes).write_image(
            self.image.as_raw(),
            self.image.width(),
            self.image.height(),
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(bytes)
    }

    pub fn jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        Ok(self.jpeg_scaled(quality, None)?.0)
    }

    /// JPEG at logical resolution, optionally limited in width. Returns the
    /// actual encoded dimensions for stream diagnostics.
    pub fn jpeg_scaled(&self, quality: u8, max_width: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
        self.jpeg_with_mode(quality, max_width, PixelMode::Logical)
    }

    /// Native mode preserves captured pixels. The width limit applies to the
    /// selected mode, never upscales it, and leaves PNG screenshots unchanged.
    pub fn jpeg_with_mode(
        &self,
        quality: u8,
        max_width: Option<u32>,
        mode: PixelMode,
    ) -> Result<(Vec<u8>, u32, u32)> {
        ensure!((1..=95).contains(&quality), "invalid JPEG quality");
        let (base_width, base_height) = match mode {
            PixelMode::Logical => (self.logical_width, self.logical_height),
            PixelMode::Native => self.image.dimensions(),
        };
        ensure!(base_width > 0 && base_height > 0, "empty stream image");
        ensure!(
            max_width.is_none_or(|w| w > 0),
            "maximum width must be positive"
        );
        let width = max_width.map_or(base_width, |w| w.min(base_width));
        let height =
            (u64::from(base_height) * u64::from(width) / u64::from(base_width)).max(1) as u32;
        let image = if self.image.dimensions() == (width, height) {
            std::borrow::Cow::Borrowed(&self.image)
        } else {
            std::borrow::Cow::Owned(imageops::resize(
                &self.image,
                width,
                height,
                imageops::FilterType::Triangle,
            ))
        };
        // Composite transparent window pixels over black before removing alpha.
        let rgb = image::RgbImage::from_fn(image.width(), image.height(), |x, y| {
            let p = image.get_pixel(x, y).0;
            image::Rgb([
                ((p[0] as u16 * p[3] as u16) / 255) as u8,
                ((p[1] as u16 * p[3] as u16) / 255) as u8,
                ((p[2] as u16 * p[3] as u16) / 255) as u8,
            ])
        });
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(&rgb)?;
        Ok((bytes, width, height))
    }
}

pub(crate) fn crop(image: &RgbaImage, logical: Rect, region: Rect) -> Result<CapturedFrame> {
    let bounds = Rect {
        x: 0,
        y: 0,
        ..logical
    };
    let clipped = region
        .intersect(bounds)
        .ok_or_else(|| anyhow::anyhow!("selected region is outside its output"))?;
    let sx = image.width() as f64 / logical.width as f64;
    let sy = image.height() as f64 / logical.height as f64;
    let x = (clipped.x as f64 * sx).floor() as u32;
    let y = (clipped.y as f64 * sy).floor() as u32;
    let right = (clipped.right() as f64 * sx)
        .ceil()
        .min(image.width() as f64) as u32;
    let bottom = (clipped.bottom() as f64 * sy)
        .ceil()
        .min(image.height() as f64) as u32;
    Ok(CapturedFrame {
        image: imageops::crop_imm(image, x, y, right - x, bottom - y).to_image(),
        logical_width: clipped.width,
        logical_height: clipped.height,
    })
}

pub(crate) fn compose(tiles: &[(&RgbaImage, Rect)], region: Rect) -> Result<CapturedFrame> {
    ensure!(
        region.width > 0 && region.height > 0,
        "empty capture region"
    );
    let scale = tiles
        .iter()
        .map(|(image, rect)| image.width() as f64 / rect.width as f64)
        .fold(1.0, f64::max);
    let width = (region.width as f64 * scale).ceil() as u32;
    let height = (region.height as f64 * scale).ceil() as u32;
    BufferSpec::packed(width, height, Format::Xrgb8888)?;
    let mut canvas = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 255]));
    for (image, logical) in tiles {
        let Some(overlap) = region.intersect(*logical) else {
            continue;
        };
        let local = Rect {
            x: overlap.x - logical.x,
            y: overlap.y - logical.y,
            ..overlap
        };
        let cropped = crop(image, *logical, local)?;
        let x = ((i64::from(overlap.x) - i64::from(region.x)) as f64 * scale).round() as u32;
        let y = ((i64::from(overlap.y) - i64::from(region.y)) as f64 * scale).round() as u32;
        let right = ((overlap.right() - i64::from(region.x)) as f64 * scale).round() as u32;
        let bottom = ((overlap.bottom() - i64::from(region.y)) as f64 * scale).round() as u32;
        let tile = imageops::resize(
            &cropped.image,
            right - x,
            bottom - y,
            imageops::FilterType::Triangle,
        );
        imageops::overlay(&mut canvas, &tile, x as i64, y as i64);
    }
    Ok(CapturedFrame {
        image: canvas,
        logical_width: region.width,
        logical_height: region.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_streams_keep_hidpi_detail_and_width_limits_use_the_selected_grid() {
        // Include fractional scaling and portrait dimensions. The encoded
        // dimensions must agree with the decoded JPEG, not just metadata.
        for (width, height, logical_width, logical_height) in [
            (640, 360, 320, 180),
            (641, 361, 427, 241),
            (360, 640, 180, 320),
        ] {
            let frame = CapturedFrame {
                image: RgbaImage::from_fn(width, height, |x, _| {
                    let gray = if x % 2 == 0 { 0 } else { 255 };
                    Rgba([gray, gray, gray, 255])
                }),
                logical_width,
                logical_height,
            };
            for mode in [PixelMode::Logical, PixelMode::Native] {
                let (base_w, base_h) = match mode {
                    PixelMode::Logical => (logical_width, logical_height),
                    PixelMode::Native => (width, height),
                };
                for limit in [None, Some(1), Some(480), Some(1000)] {
                    let (bytes, w, h) = frame.jpeg_with_mode(95, limit, mode).unwrap();
                    let decoded = image::load_from_memory(&bytes).unwrap().to_rgb8();
                    assert_eq!(decoded.dimensions(), (w, h));
                    assert_eq!(w, limit.map_or(base_w, |cap| cap.min(base_w)));
                    assert_eq!(h, (base_h * w / base_w).max(1));
                    if mode == PixelMode::Native && limit.is_none() {
                        // One-pixel lines survive in native mode.
                        assert!(decoded.get_pixel(0, 0)[0] < 20);
                        assert!(decoded.get_pixel(1, 0)[0] > 235);
                    }
                }
            }
            assert_eq!(
                image::load_from_memory(&frame.png().unwrap())
                    .unwrap()
                    .width(),
                width
            );
        }
    }

    #[test]
    fn jpeg_width_limit_preserves_aspect_and_decodes_at_quality_extremes() {
        let frame = crate::demo_frame(0);
        for width in [1, 17, 320, 1000] {
            for quality in [1, 55, 95] {
                let (bytes, w, h) = frame.jpeg_scaled(quality, Some(width)).unwrap();
                let decoded = image::load_from_memory(&bytes).unwrap();
                assert_eq!((decoded.width(), decoded.height()), (w, h));
                assert_eq!(w, width.min(640));
                assert_eq!(h, (360 * w / 640).max(1));
            }
        }
        assert!(frame.jpeg_scaled(0, None).is_err());
        assert!(frame.jpeg_scaled(55, Some(0)).is_err());
    }

    #[test]
    fn handles_stride_channel_order_and_inversion() {
        let spec = BufferSpec {
            width: 1,
            height: 2,
            stride: 8,
            format: Format::Xrgb8888,
        };
        let bytes: Vec<_> = [0x00ff0000u32, 0xdeadbeef, 0x000000ff, 0xdeadbeef]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect();
        let image = decode(&bytes, spec, true, Transform::Normal).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert_eq!(image.get_pixel(0, 1).0, [255, 0, 0, 255]);
    }
    #[test]
    fn handles_alpha_and_ten_bit_color() {
        let image = decode(
            &0x80800000u32.to_ne_bytes(),
            BufferSpec::packed(1, 1, Format::Argb8888).unwrap(),
            false,
            Transform::Normal,
        )
        .unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 128]);
        let image = decode(
            &1023u32.to_ne_bytes(),
            BufferSpec::packed(1, 1, Format::Xbgr2101010).unwrap(),
            false,
            Transform::Normal,
        )
        .unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }
    #[test]
    fn crops_fractional_scale_output_relative_coordinates() {
        let image = RgbaImage::from_fn(250, 125, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let frame = crop(
            &image,
            Rect {
                x: -200,
                y: 50,
                width: 200,
                height: 100,
            },
            Rect {
                x: 20,
                y: 10,
                width: 40,
                height: 20,
            },
        )
        .unwrap();
        assert_eq!(frame.image.dimensions(), (50, 26));
        assert_eq!(frame.image.get_pixel(0, 0).0, [25, 12, 0, 255]);
        let jpeg = image::load_from_memory(&frame.jpeg(55).unwrap()).unwrap();
        assert_eq!((jpeg.width(), jpeg.height()), (40, 20));
        assert_eq!(
            image::load_from_memory(&frame.png().unwrap())
                .unwrap()
                .width(),
            50
        );
    }
    #[test]
    fn composes_monitors_at_negative_origins_and_different_scales() {
        let a = RgbaImage::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let b = RgbaImage::from_pixel(200, 200, Rgba([0, 0, 255, 255]));
        let frame = compose(
            &[
                (
                    &a,
                    Rect {
                        x: -100,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                ),
                (
                    &b,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                ),
            ],
            Rect {
                x: -25,
                y: 20,
                width: 50,
                height: 40,
            },
        )
        .unwrap();
        assert_eq!(frame.image.dimensions(), (100, 80));
        assert_eq!(frame.image.get_pixel(49, 0).0, [255, 0, 0, 255]);
        assert_eq!(frame.image.get_pixel(50, 0).0, [0, 0, 255, 255]);
    }
    #[test]
    fn rejects_invalid_or_oversized_buffers() {
        assert!(BufferSpec::packed(u32::MAX, 2, Format::Xrgb8888).is_err());
        assert!(BufferSpec::packed(20000, 20000, Format::Xrgb8888).is_err());
        let spec = BufferSpec {
            width: 10,
            height: 10,
            stride: 1,
            format: Format::Xrgb8888,
        };
        assert!(decode(&[], spec, false, Transform::Normal).is_err());
    }
    #[test]
    fn undoes_all_eight_output_transforms() {
        let original = RgbaImage::from_fn(3, 2, |x, y| Rgba([(x + 3 * y) as u8, 0, 0, 255]));
        for transform in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let flipped = matches!(
                transform,
                Transform::Flipped
                    | Transform::Flipped90
                    | Transform::Flipped180
                    | Transform::Flipped270
            );
            let image = if flipped {
                imageops::flip_horizontal(&original)
            } else {
                original.clone()
            };
            let image = match transform {
                Transform::_90 | Transform::Flipped90 => imageops::rotate270(&image),
                Transform::_180 | Transform::Flipped180 => imageops::rotate180(&image),
                Transform::_270 | Transform::Flipped270 => imageops::rotate90(&image),
                _ => image,
            };
            let bytes: Vec<u8> = image
                .pixels()
                .flat_map(|p| (0xff000000u32 | ((p[0] as u32) << 16)).to_ne_bytes())
                .collect();
            let actual = decode(
                &bytes,
                BufferSpec::packed(image.width(), image.height(), Format::Xrgb8888).unwrap(),
                false,
                transform,
            )
            .unwrap();
            assert_eq!(actual, original, "{transform:?}");
        }
    }
}
