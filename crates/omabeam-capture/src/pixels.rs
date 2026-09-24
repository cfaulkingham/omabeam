use crate::Rect;
use anyhow::{Result, bail, ensure};
use image::{ImageEncoder, Rgba, RgbaImage, imageops};
use std::ops::Range;
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

/// How decoding treats translucent pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AlphaMode {
    /// Straight (un-premultiplied) color and the source alpha, rounded as
    /// before. PNG screenshots need this.
    #[default]
    Straight,
    /// The premultiplied color over black with alpha 255: what the JPEG and
    /// H.264 paths composite for unscaled frames anyway (within ±1), without
    /// dividing by alpha only to multiply it back. A PNG of such a frame is
    /// flattened.
    Opaque,
}

/// Converts native-endian wl_shm packed pixels to straight-alpha RGBA.
/// Padding is ignored and Y inversion is applied before the output transform.
#[cfg_attr(not(test), expect(dead_code))] // Capture reuses images via decode_into.
pub(crate) fn decode(
    bytes: &[u8],
    spec: BufferSpec,
    inverted: bool,
    transform: Transform,
) -> Result<RgbaImage> {
    let mut image = RgbaImage::new(0, 0);
    decode_into(
        bytes,
        spec,
        inverted,
        transform,
        AlphaMode::Straight,
        &mut image,
    )?;
    Ok(image)
}

/// `decode` into a caller-owned image that is reallocated only when the
/// output dimensions change, so steady capture reuses one allocation. Every
/// pixel is replaced; on error `dst` is untouched.
pub(crate) fn decode_into(
    bytes: &[u8],
    spec: BufferSpec,
    inverted: bool,
    transform: Transform,
    alpha: AlphaMode,
    dst: &mut RgbaImage,
) -> Result<()> {
    let packed = Packed::new(bytes, spec, inverted, transform, alpha)?;
    let (width, height) = packed.size();
    if dst.dimensions() != (width, height) {
        *dst = RgbaImage::new(width, height);
    }
    packed.convert_rows(0..spec.height, dst);
    Ok(())
}

/// Converts only buffer rows `rows` into `dst`, for damage-limited updates.
/// Rows index the buffer as stored, top row first (ext-image-copy-capture
/// damage uses these coordinates); Y inversion and `transform` place their
/// pixels exactly where `decode_into` would. Pixels from other rows are
/// untouched.
///
/// Preconditions:
/// - `dst` holds a full `decode_into` of this output made with the same
///   `spec`, `inverted`, `transform` and `alpha`; after any change to those,
///   decode in full first. Only the dimensions are checked.
/// - `rows` lies within `0..spec.height` (checked; an empty range is a no-op).
/// - `bytes` spans the whole buffer (checked), though only `rows` are read.
///
/// On error `dst` is untouched.
pub(crate) fn decode_rows_into(
    bytes: &[u8],
    spec: BufferSpec,
    inverted: bool,
    transform: Transform,
    alpha: AlphaMode,
    rows: Range<u32>,
    dst: &mut RgbaImage,
) -> Result<()> {
    let packed = Packed::new(bytes, spec, inverted, transform, alpha)?;
    ensure!(
        dst.dimensions() == packed.size(),
        "decoded image does not match the capture buffer"
    );
    ensure!(
        rows.start <= rows.end && rows.end <= spec.height,
        "rows {rows:?} are outside the capture buffer"
    );
    packed.convert_rows(rows, dst);
    Ok(())
}

/// Same result as `crop` of a full `decode_into` (`logical` is the output's
/// layout rectangle, `region` is output-relative), converting only the
/// region's pixels. Only the Normal transform saves work; rotated or flipped
/// outputs are decoded in full and cropped.
#[cfg_attr(not(test), expect(dead_code))] // Not yet used outside tests.
pub(crate) fn decode_rect(
    bytes: &[u8],
    spec: BufferSpec,
    inverted: bool,
    transform: Transform,
    alpha: AlphaMode,
    logical: Rect,
    region: Rect,
) -> Result<CapturedFrame> {
    if transform != Transform::Normal {
        // A rotated or mirrored region is not a span of each buffer row.
        let mut image = RgbaImage::new(0, 0);
        decode_into(bytes, spec, inverted, transform, alpha, &mut image)?;
        return crop(&image, logical, region);
    }
    let packed = Packed::new(bytes, spec, inverted, transform, alpha)?;
    let (rect, clipped) = pixel_rect((spec.width, spec.height), logical, region)?;
    let mut image = RgbaImage::new(rect.width, rect.height);
    let span = rect.x as usize * packed.bpp..(rect.x + rect.width) as usize * packed.bpp;
    let rows = image.chunks_exact_mut(rect.width as usize * 4);
    for (y, out) in (rect.y..rect.y + rect.height).zip(rows) {
        let source_y = if inverted { spec.height - 1 - y } else { y };
        (packed.kernel)(&packed.row(source_y)[span.clone()], out);
    }
    Ok(CapturedFrame {
        image,
        logical_width: clipped.width,
        logical_height: clipped.height,
    })
}

/// A validated buffer, with its row kernel and layout chosen once per frame.
struct Packed<'a> {
    bytes: &'a [u8],
    spec: BufferSpec,
    bpp: usize,
    kernel: RowKernel,
    layout: Layout,
    /// Buffer rows land last to first: Y inversion combined with the layout.
    flip_rows: bool,
}

impl<'a> Packed<'a> {
    fn new(
        bytes: &'a [u8],
        spec: BufferSpec,
        inverted: bool,
        transform: Transform,
        alpha: AlphaMode,
    ) -> Result<Self> {
        ensure!(bytes.len() >= spec.byte_len()?, "truncated capture buffer");
        let layout = Layout::of(transform)?;
        Ok(Self {
            bytes,
            spec,
            // byte_len has rejected unsupported formats.
            bpp: bytes_per_pixel(spec.format).unwrap() as usize,
            kernel: row_kernel(spec.format, alpha)?,
            layout,
            flip_rows: layout.mirror != inverted,
        })
    }

    /// Output dimensions: rows that become columns swap them.
    fn size(&self) -> (u32, u32) {
        if self.layout.columns {
            (self.spec.height, self.spec.width)
        } else {
            (self.spec.width, self.spec.height)
        }
    }

    /// Buffer row `y`'s pixels, without padding.
    fn row(&self, y: u32) -> &'a [u8] {
        let start = y as usize * self.spec.stride as usize;
        &self.bytes[start..start + self.spec.width as usize * self.bpp]
    }

    /// Converts buffer rows into `dst`, which must have the output dimensions.
    fn convert_rows(&self, rows: Range<u32>, dst: &mut RgbaImage) {
        let (width, height) = (self.spec.width as usize, self.spec.height as usize);
        let out = &mut (**dst)[..width * height * 4];
        // Rows that become columns are converted here, then scattered.
        let mut line = vec![0; if self.layout.columns { width * 4 } else { 0 }];
        for y in rows {
            let src = self.row(y);
            let y = y as usize;
            let target = if self.flip_rows { height - 1 - y } else { y };
            if !self.layout.columns {
                let row = &mut out[target * width * 4..][..width * 4];
                (self.kernel)(src, row);
                if self.layout.reverse {
                    row.as_chunks_mut::<4>().0.reverse();
                }
                continue;
            }
            (self.kernel)(src, &mut line);
            // Output rows are `height` pixels wide.
            let column = out.as_chunks_mut::<4>().0[target..]
                .iter_mut()
                .step_by(height);
            let pixels = line.as_chunks::<4>().0;
            if self.layout.reverse {
                column.zip(pixels.iter().rev()).for_each(|(d, s)| *d = *s);
            } else {
                column.zip(pixels).for_each(|(d, s)| *d = *s);
            }
        }
    }
}

/// Where buffer rows land in the output image. wl_output transforms describe
/// the transform applied to the buffer; undoing the rotation (the protocol
/// uses counter-clockwise angles) and then the reflection sends each buffer
/// row to an output row or column, in either direction.
#[derive(Clone, Copy)]
struct Layout {
    /// Rows become columns (90° and 270°).
    columns: bool,
    /// The last buffer row lands first, before Y inversion.
    mirror: bool,
    /// A row's last pixel lands first.
    reverse: bool,
}

impl Layout {
    fn of(transform: Transform) -> Result<Self> {
        let (columns, mirror, reverse) = match transform {
            Transform::Normal => (false, false, false),
            Transform::_90 => (true, true, false),
            Transform::_180 => (false, true, true),
            Transform::_270 => (true, false, true),
            Transform::Flipped => (false, false, true),
            Transform::Flipped90 => (true, false, false),
            Transform::Flipped180 => (false, true, false),
            Transform::Flipped270 => (true, true, true),
            _ => bail!("unsupported capture transform"),
        };
        Ok(Self {
            columns,
            mirror,
            reverse,
        })
    }
}

/// Converts one row: `src` holds exactly the row's packed pixels, `dst` four
/// bytes per pixel.
type RowKernel = fn(&[u8], &mut [u8]);

/// Picks the row kernel once per frame, so pixel loops carry no format
/// `match` or bounds checks and can vectorize. Formats without alpha convert
/// identically in both modes.
fn row_kernel(format: Format, alpha: AlphaMode) -> Result<RowKernel> {
    use AlphaMode::{Opaque, Straight};
    let kernel: RowKernel = match (format, alpha) {
        (Format::Xrgb8888, _) => |s, d| convert(s, d, xrgb8888),
        (Format::Argb8888, Straight) => |s, d| convert(s, d, |p| unpremultiply(argb8888(p))),
        (Format::Argb8888, Opaque) => |s, d| convert(s, d, |p| over_black(argb8888(p))),
        (Format::Xbgr8888, _) => |s, d| convert(s, d, xbgr8888),
        (Format::Abgr8888, Straight) => |s, d| convert(s, d, |p| unpremultiply(abgr8888(p))),
        (Format::Abgr8888, Opaque) => |s, d| convert(s, d, |p| over_black(abgr8888(p))),
        (Format::Xrgb2101010, _) => |s, d| convert(s, d, xrgb2101010),
        (Format::Argb2101010, Straight) => |s, d| convert(s, d, |p| unpremultiply(argb2101010(p))),
        (Format::Argb2101010, Opaque) => |s, d| convert(s, d, |p| over_black(argb2101010(p))),
        (Format::Xbgr2101010, _) => |s, d| convert(s, d, xbgr2101010),
        (Format::Abgr2101010, Straight) => |s, d| convert(s, d, |p| unpremultiply(abgr2101010(p))),
        (Format::Abgr2101010, Opaque) => |s, d| convert(s, d, |p| over_black(abgr2101010(p))),
        (Format::Rgb888, _) => |s, d| convert(s, d, rgb888),
        (Format::Bgr888, _) => |s, d| convert(s, d, bgr888),
        (Format::Rgb565, _) => |s, d| convert(s, d, rgb565),
        (Format::Bgr565, _) => |s, d| convert(s, d, bgr565),
        _ => bail!("unsupported capture pixel format: {format:?}"),
    };
    Ok(kernel)
}

fn convert<const N: usize>(src: &[u8], dst: &mut [u8], pixel: impl Fn([u8; N]) -> [u8; 4]) {
    // The zip below would silently stop at the shorter row.
    debug_assert_eq!(src.len() / N, dst.len() / 4, "pixel counts differ");
    for (s, d) in src
        .as_chunks::<N>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0)
    {
        *d = pixel(*s);
    }
}

// Pixel readers return the stored, premultiplied [r, g, b, a]. wl_shm formats
// are little-endian words; reading them native-endian keeps the byte order
// decode has always used.

fn argb8888(p: [u8; 4]) -> [u8; 4] {
    let v = u32::from_ne_bytes(p);
    [(v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as u8]
}

fn xrgb8888(p: [u8; 4]) -> [u8; 4] {
    let [r, g, b, _] = argb8888(p);
    [r, g, b, 255]
}

fn abgr8888(p: [u8; 4]) -> [u8; 4] {
    let v = u32::from_ne_bytes(p);
    [v as u8, (v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8]
}

fn xbgr8888(p: [u8; 4]) -> [u8; 4] {
    let [r, g, b, _] = abgr8888(p);
    [r, g, b, 255]
}

fn argb2101010(p: [u8; 4]) -> [u8; 4] {
    let v = u32::from_ne_bytes(p);
    [ten(v >> 20), ten(v >> 10), ten(v), two(v >> 30)]
}

fn xrgb2101010(p: [u8; 4]) -> [u8; 4] {
    let [r, g, b, _] = argb2101010(p);
    [r, g, b, 255]
}

fn abgr2101010(p: [u8; 4]) -> [u8; 4] {
    let v = u32::from_ne_bytes(p);
    [ten(v), ten(v >> 10), ten(v >> 20), two(v >> 30)]
}

fn xbgr2101010(p: [u8; 4]) -> [u8; 4] {
    let [r, g, b, _] = abgr2101010(p);
    [r, g, b, 255]
}

/// The low 10 bits, rounded to 8.
fn ten(v: u32) -> u8 {
    (((v & 1023) * 255 + 511) / 1023) as u8
}

/// The low 2 bits of alpha, scaled to 8.
fn two(v: u32) -> u8 {
    ((v & 3) * 85) as u8
}

fn rgb565(p: [u8; 2]) -> [u8; 4] {
    let v = u16::from_ne_bytes(p);
    [
        (((v >> 11) & 31) * 255 / 31) as u8,
        (((v >> 5) & 63) * 255 / 63) as u8,
        ((v & 31) * 255 / 31) as u8,
        255,
    ]
}

fn bgr565(p: [u8; 2]) -> [u8; 4] {
    let [r, g, b, a] = rgb565(p);
    [b, g, r, a]
}

/// `from_ne_bytes` for 24-bit pixels: red is the high byte of RGB888.
fn u24([a, b, c]: [u8; 3]) -> u32 {
    if cfg!(target_endian = "little") {
        u32::from_le_bytes([a, b, c, 0])
    } else {
        u32::from_be_bytes([0, a, b, c])
    }
}

fn rgb888(p: [u8; 3]) -> [u8; 4] {
    let v = u24(p);
    [(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]
}

fn bgr888(p: [u8; 3]) -> [u8; 4] {
    let v = u24(p);
    [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
}

/// Straight alpha, rounded as PNG screenshots always have been.
fn unpremultiply([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    if a == 0 || a == 255 {
        return [r, g, b, a];
    }
    let straight = |v: u8| ((u32::from(v) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8;
    [straight(r), straight(g), straight(b), a]
}

/// The premultiplied color over black. Clamping to alpha matches compositing
/// the clamped straight color when invalid color exceeds alpha.
fn over_black([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    [r.min(a), g.min(a), b.min(a), 255]
}

/// Whether every pixel has alpha 255. ANDing whole pixels keeps the scan
/// vectorized; testing per block still stops early on translucent frames.
fn is_opaque(image: &RgbaImage) -> bool {
    let len = image.width() as usize * image.height() as usize * 4;
    image.as_raw()[..len].chunks(4096).all(|block| {
        let pixels = block.as_chunks::<4>().0.iter();
        let all = pixels.fold(u32::MAX, |all, p| all & u32::from_ne_bytes(*p));
        all.to_ne_bytes()[3] == 255
    })
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
    /// Keeps transparency only for frames decoded with `AlphaMode::Straight`.
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
        let (width, height) = self.stream_dimensions(max_width, mode)?;
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality);
        if self.image.dimensions() == (width, height) && is_opaque(&self.image) {
            // The encoder drops alpha, so opaque pixels need no RGB copy.
            encoder.encode_image(&self.image)?;
        } else {
            encoder.encode_image(&self.stream_rgb(max_width, mode)?)?;
        }
        Ok((bytes, width, height))
    }

    pub fn stream_dimensions(&self, max_width: Option<u32>, mode: PixelMode) -> Result<(u32, u32)> {
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
        Ok((width, height))
    }

    /// Shared scaling and alpha compositing for JPEG and H.264 encoders.
    pub fn stream_rgb(&self, max_width: Option<u32>, mode: PixelMode) -> Result<image::RgbImage> {
        let (width, height) = self.stream_dimensions(max_width, mode)?;
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
        // Rows are contiguous, so one slice pass covers them all.
        let mut rgb = vec![0; width as usize * height as usize * 3];
        let rgba = image.as_raw()[..rgb.len() / 3 * 4].as_chunks::<4>().0;
        for (out, &[r, g, b, a]) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(rgba) {
            let over_black = |c: u8| ((u16::from(c) * u16::from(a)) / 255) as u8;
            *out = [over_black(r), over_black(g), over_black(b)];
        }
        Ok(image::RgbImage::from_raw(width, height, rgb).expect("sized for the image"))
    }
}

/// Image pixels a region touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The pixels of an image of `size`, covering an output's `logical`
/// rectangle, that output-relative `region` touches. Rounds outward so
/// fractional scales keep partly covered pixels, and also returns the region
/// clipped to the output.
pub(crate) fn pixel_rect(
    (width, height): (u32, u32),
    logical: Rect,
    region: Rect,
) -> Result<(PixelRect, Rect)> {
    let bounds = Rect {
        x: 0,
        y: 0,
        ..logical
    };
    let clipped = region
        .intersect(bounds)
        .ok_or_else(|| anyhow::anyhow!("selected region is outside its output"))?;
    let sx = width as f64 / logical.width as f64;
    let sy = height as f64 / logical.height as f64;
    let x = (clipped.x as f64 * sx).floor() as u32;
    let y = (clipped.y as f64 * sy).floor() as u32;
    let right = (clipped.right() as f64 * sx).ceil().min(width as f64) as u32;
    let bottom = (clipped.bottom() as f64 * sy).ceil().min(height as f64) as u32;
    let rect = PixelRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    };
    Ok((rect, clipped))
}

pub(crate) fn crop(image: &RgbaImage, logical: Rect, region: Rect) -> Result<CapturedFrame> {
    let (rect, clipped) = pixel_rect(image.dimensions(), logical, region)?;
    Ok(CapturedFrame {
        image: imageops::crop_imm(image, rect.x, rect.y, rect.width, rect.height).to_image(),
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
    use std::{borrow::Cow, hint::black_box, time::Instant};

    // The per-pixel implementations the kernels replaced, kept verbatim as
    // references: Straight output and every JPEG must match them.

    fn reference_decode(
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
                        let (r, g, b) =
                            if matches!(spec.format, Format::Abgr8888 | Format::Xbgr8888) {
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
                        let ten =
                            |shift: u32| ((((v >> shift) & 1023u32) * 255 + 511) / 1023) as u8;
                        let (r, b) =
                            if matches!(spec.format, Format::Xbgr2101010 | Format::Abgr2101010) {
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
                        let reverse =
                            (spec.format == Format::Rgb888) == cfg!(target_endian = "little");
                        if reverse {
                            (p[2], p[1], p[0], 255)
                        } else {
                            (p[0], p[1], p[2], 255)
                        }
                    }
                    _ => bail!("unsupported capture pixel format"),
                };
                if a > 0 && a < 255 {
                    let straight = |v: u8| {
                        ((u32::from(v) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
                    };
                    (r, g, b) = (straight(r), straight(g), straight(b));
                }
                image.put_pixel(x, y, Rgba([r, g, b, a]));
            }
        }
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

    fn reference_stream_rgb(
        frame: &CapturedFrame,
        max_width: Option<u32>,
        mode: PixelMode,
    ) -> Result<image::RgbImage> {
        let (width, height) = frame.stream_dimensions(max_width, mode)?;
        let image = if frame.image.dimensions() == (width, height) {
            Cow::Borrowed(&frame.image)
        } else {
            Cow::Owned(imageops::resize(
                &frame.image,
                width,
                height,
                imageops::FilterType::Triangle,
            ))
        };
        let rgb = image::RgbImage::from_fn(image.width(), image.height(), |x, y| {
            let p = image.get_pixel(x, y).0;
            image::Rgb([
                ((p[0] as u16 * p[3] as u16) / 255) as u8,
                ((p[1] as u16 * p[3] as u16) / 255) as u8,
                ((p[2] as u16 * p[3] as u16) / 255) as u8,
            ])
        });
        Ok(rgb)
    }

    fn reference_jpeg(
        frame: &CapturedFrame,
        quality: u8,
        max_width: Option<u32>,
        mode: PixelMode,
    ) -> Result<(Vec<u8>, u32, u32)> {
        ensure!((1..=95).contains(&quality), "invalid JPEG quality");
        let rgb = reference_stream_rgb(frame, max_width, mode)?;
        let (width, height) = rgb.dimensions();
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(&rgb)?;
        Ok((bytes, width, height))
    }

    const FORMATS: [Format; 12] = [
        Format::Argb8888,
        Format::Xrgb8888,
        Format::Abgr8888,
        Format::Xbgr8888,
        Format::Xrgb2101010,
        Format::Argb2101010,
        Format::Xbgr2101010,
        Format::Abgr2101010,
        Format::Rgb888,
        Format::Bgr888,
        Format::Rgb565,
        Format::Bgr565,
    ];
    const TRANSFORMS: [Transform; 8] = [
        Transform::Normal,
        Transform::_90,
        Transform::_180,
        Transform::_270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];
    /// Transparent, the rounding extremes, half, and opaque.
    const ALPHAS: [u8; 5] = [0, 1, 127, 254, 255];

    /// Deterministic xorshift bytes, so padding and ignored bits are not zero.
    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect()
    }

    /// Random colors and padding; alpha cycles through ALPHAS (all four
    /// values for 2-bit alpha).
    fn buffer(
        format: Format,
        width: u32,
        height: u32,
        padding: u32,
        seed: u64,
    ) -> (Vec<u8>, BufferSpec) {
        let bpp = bytes_per_pixel(format).unwrap();
        let spec = BufferSpec {
            width,
            height,
            stride: width * bpp + padding,
            format,
        };
        let mut bytes = noise(seed, spec.byte_len().unwrap());
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) as usize;
                let (keep, alpha) = match format {
                    Format::Argb8888 | Format::Abgr8888 => {
                        (0x00ff_ffff, u32::from(ALPHAS[i % ALPHAS.len()]) << 24)
                    }
                    Format::Argb2101010 | Format::Abgr2101010 => {
                        (0x3fff_ffff, (i as u32 % 4) << 30)
                    }
                    _ => continue,
                };
                let at = (y * spec.stride + x * bpp) as usize;
                let pixel = &mut bytes[at..at + 4];
                let v = u32::from_ne_bytes(pixel.try_into().unwrap()) & keep | alpha;
                pixel.copy_from_slice(&v.to_ne_bytes());
            }
        }
        (bytes, spec)
    }

    /// Every channel value against every alpha a format can express (a spread
    /// for 24-bit color), 256 pixels per row.
    fn value_sweep(format: Format) -> (Vec<u8>, BufferSpec) {
        let bytes: Vec<u8> = match format {
            Format::Argb8888 | Format::Xrgb8888 | Format::Abgr8888 | Format::Xbgr8888 => (0..1u32
                << 16)
                .flat_map(|i| {
                    let (c, a) = (i & 255, i >> 8);
                    (a << 24 | c << 16 | (255 - c) << 8 | (c ^ 0x5a)).to_ne_bytes()
                })
                .collect(),
            Format::Rgb565 | Format::Bgr565 => (0..=u16::MAX).flat_map(u16::to_ne_bytes).collect(),
            Format::Rgb888 | Format::Bgr888 => (0..1u32 << 16)
                .flat_map(|i| [i as u8, (i >> 8) as u8, (i as u8).wrapping_mul(37)])
                .collect(),
            _ => (0..1u32 << 12)
                .flat_map(|i| {
                    let (c, a) = (i & 1023, i >> 10);
                    (a << 30 | c << 20 | (1023 - c) << 10 | (c ^ 0x155)).to_ne_bytes()
                })
                .collect(),
        };
        let row = 256 * bytes_per_pixel(format).unwrap() as usize;
        let spec = BufferSpec::packed(256, (bytes.len() / row) as u32, format).unwrap();
        (bytes, spec)
    }

    /// The same bits with alpha ignored: the stored, premultiplied color.
    fn color_only(format: Format) -> Format {
        match format {
            Format::Argb8888 => Format::Xrgb8888,
            Format::Abgr8888 => Format::Xbgr8888,
            Format::Argb2101010 => Format::Xrgb2101010,
            Format::Abgr2101010 => Format::Xbgr2101010,
            other => other,
        }
    }

    fn decoded(
        bytes: &[u8],
        spec: BufferSpec,
        inverted: bool,
        transform: Transform,
        alpha: AlphaMode,
    ) -> RgbaImage {
        let mut image = RgbaImage::new(0, 0);
        decode_into(bytes, spec, inverted, transform, alpha, &mut image).unwrap();
        image
    }

    #[track_caller]
    fn assert_same(actual: &RgbaImage, expected: &RgbaImage, context: &str) {
        assert_eq!(actual.dimensions(), expected.dimensions(), "{context}");
        let (w, _) = actual.dimensions();
        let diff = actual
            .pixels()
            .zip(expected.pixels())
            .position(|(a, e)| a != e);
        if let Some(i) = diff {
            let (x, y) = (i as u32 % w, i as u32 / w);
            panic!(
                "{context}: pixel ({x}, {y}) is {:?}, expected {:?}",
                actual.get_pixel(x, y),
                expected.get_pixel(x, y)
            );
        }
    }

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

    #[test]
    fn straight_decode_matches_the_reference_for_every_layout() {
        for format in FORMATS {
            // Odd widths leave a vector tail; odd padding misaligns rows.
            for (width, height) in [(5, 3), (37, 4)] {
                for padding in [0, 5] {
                    let (bytes, spec) =
                        buffer(format, width, height, padding, u64::from(width + padding));
                    for inverted in [false, true] {
                        for transform in TRANSFORMS {
                            let context = format!(
                                "{format:?} {width}x{height}+{padding} inverted={inverted} {transform:?}"
                            );
                            let expected =
                                reference_decode(&bytes, spec, inverted, transform).unwrap();
                            let old_api = decode(&bytes, spec, inverted, transform).unwrap();
                            assert_same(&old_api, &expected, &context);
                            let straight =
                                decoded(&bytes, spec, inverted, transform, AlphaMode::Straight);
                            assert_same(&straight, &expected, &context);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn straight_decode_matches_the_reference_for_every_pixel_value() {
        for format in FORMATS {
            let (bytes, spec) = value_sweep(format);
            let expected = reference_decode(&bytes, spec, false, Transform::Normal).unwrap();
            let actual = decoded(&bytes, spec, false, Transform::Normal, AlphaMode::Straight);
            assert_same(&actual, &expected, &format!("{format:?}"));
        }
    }

    #[test]
    fn opaque_decode_is_premultiplied_color_over_black() {
        for format in FORMATS {
            let ten_bit = matches!(
                format,
                Format::Argb2101010
                    | Format::Abgr2101010
                    | Format::Xrgb2101010
                    | Format::Xbgr2101010
            );
            for (bytes, spec) in [value_sweep(format), buffer(format, 37, 4, 5, 9)] {
                let opaque = decoded(&bytes, spec, false, Transform::Normal, AlphaMode::Opaque);
                let color_spec = BufferSpec {
                    format: color_only(format),
                    ..spec
                };
                let color = reference_decode(&bytes, color_spec, false, Transform::Normal).unwrap();
                let straight = reference_decode(&bytes, spec, false, Transform::Normal).unwrap();
                // What the JPEG and H.264 paths made of the straight decode.
                let composited = reference_stream_rgb(
                    &CapturedFrame {
                        image: straight.clone(),
                        logical_width: spec.width,
                        logical_height: spec.height,
                    },
                    None,
                    PixelMode::Native,
                )
                .unwrap();
                let pixels = opaque.pixels().zip(color.pixels()).zip(straight.pixels());
                for (((o, c), s), old) in pixels.zip(composited.pixels()) {
                    let a = s[3];
                    let expected = [c[0].min(a), c[1].min(a), c[2].min(a), 255];
                    assert_eq!(o.0, expected, "{format:?} color {c:?} alpha {a}");
                    for channel in 0..3 {
                        let diff = o[channel].abs_diff(old[channel]);
                        assert!(diff <= 1, "{format:?} {o:?} vs composited {old:?}");
                        assert!(diff == 0 || !ten_bit, "{format:?} {o:?} vs {old:?}");
                    }
                }
            }
        }
    }

    /// Which buffer row each output pixel comes from, per the reference.
    fn source_rows(width: u32, height: u32, inverted: bool, transform: Transform) -> RgbaImage {
        let bytes: Vec<u8> = (0..height)
            .flat_map(|y| (0..width).flat_map(move |_| (y << 16).to_ne_bytes()))
            .collect();
        let spec = BufferSpec::packed(width, height, Format::Xrgb8888).unwrap();
        reference_decode(&bytes, spec, inverted, transform).unwrap()
    }

    #[test]
    fn decode_rows_into_converts_only_the_requested_buffer_rows() {
        let (width, height) = (7, 6);
        let untouched = Rgba([1, 2, 3, 4]);
        for format in FORMATS {
            let (bytes, spec) = buffer(format, width, height, 5, 3);
            for alpha in [AlphaMode::Straight, AlphaMode::Opaque] {
                for inverted in [false, true] {
                    for transform in TRANSFORMS {
                        let full = decoded(&bytes, spec, inverted, transform, alpha);
                        let origin = source_rows(width, height, inverted, transform);
                        for rows in [0..0, 0..1, 2..4, height - 1..height, 0..height] {
                            let context = format!(
                                "{format:?} {alpha:?} inverted={inverted} {transform:?} rows {rows:?}"
                            );
                            let mut image =
                                RgbaImage::from_pixel(full.width(), full.height(), untouched);
                            let storage = image.as_raw().as_ptr();
                            decode_rows_into(
                                &bytes,
                                spec,
                                inverted,
                                transform,
                                alpha,
                                rows.clone(),
                                &mut image,
                            )
                            .unwrap();
                            assert_eq!(image.as_raw().as_ptr(), storage, "{context}");
                            let expected =
                                RgbaImage::from_fn(full.width(), full.height(), |x, y| {
                                    let row = u32::from(origin.get_pixel(x, y)[0]);
                                    if rows.contains(&row) {
                                        *full.get_pixel(x, y)
                                    } else {
                                        untouched
                                    }
                                });
                            assert_same(&image, &expected, &context);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn decode_rows_into_rejects_mismatched_images_and_rows() {
        let (bytes, spec) = buffer(Format::Xrgb8888, 7, 6, 0, 4);
        for (transform, (width, height), rows) in [
            (Transform::Normal, (6, 7), 0..6),
            (Transform::_90, (7, 6), 0..6),
            (Transform::Normal, (7, 6), 0..7),
            (
                Transform::Normal,
                (7, 6),
                std::ops::Range { start: 4, end: 2 },
            ),
        ] {
            let mut image = RgbaImage::from_pixel(width, height, Rgba([1, 2, 3, 4]));
            let before = image.clone();
            let result = decode_rows_into(
                &bytes,
                spec,
                false,
                transform,
                AlphaMode::Opaque,
                rows,
                &mut image,
            );
            assert!(result.is_err());
            assert_eq!(image, before);
        }
        let mut image = RgbaImage::new(7, 6);
        let truncated = &bytes[..bytes.len() - 1];
        let result = decode_rows_into(
            truncated,
            spec,
            false,
            Transform::Normal,
            AlphaMode::Opaque,
            0..1,
            &mut image,
        );
        assert!(result.is_err());
    }

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn decode_rect_matches_cropping_a_full_decode() {
        let outputs = [
            // 1.25 at a negative layout origin, as in the crop test.
            (250, 125, rect(-200, 50, 200, 100)),
            // About 1.5 with odd sizes.
            (151, 85, rect(0, 0, 101, 57)),
            // Fewer pixels than logical units.
            (100, 60, rect(1920, 0, 150, 90)),
        ];
        for (width, height, logical) in outputs {
            let (right, bottom) = (logical.width as i32, logical.height as i32);
            let regions = [
                rect(20, 10, 40, 20),
                rect(0, 0, logical.width, logical.height),
                // Clipped at the top left and at the bottom right.
                rect(-10, -5, 31, 17),
                rect(right - 7, bottom - 3, 50, 50),
                rect(3, 7, 1, 1),
            ];
            let formats = [
                Format::Xrgb8888,
                Format::Argb8888,
                Format::Bgr888,
                Format::Abgr2101010,
            ];
            for format in formats {
                let (bytes, spec) = buffer(format, width, height, 5, 11);
                for alpha in [AlphaMode::Straight, AlphaMode::Opaque] {
                    for inverted in [false, true] {
                        // Only Normal has its own path; the others must still agree.
                        let transforms = [Transform::Normal, Transform::_90, Transform::Flipped180];
                        for transform in transforms {
                            let full = decoded(&bytes, spec, inverted, transform, alpha);
                            for region in regions {
                                let context = format!(
                                    "{width}x{height} {format:?} {alpha:?} inverted={inverted} {transform:?} {region:?}"
                                );
                                let expected = crop(&full, logical, region).unwrap();
                                let actual = decode_rect(
                                    &bytes, spec, inverted, transform, alpha, logical, region,
                                )
                                .unwrap();
                                assert_same(&actual.image, &expected.image, &context);
                                assert_eq!(
                                    (actual.logical_width, actual.logical_height),
                                    (expected.logical_width, expected.logical_height),
                                    "{context}"
                                );
                            }
                        }
                    }
                }
            }
            let (bytes, spec) = buffer(Format::Xrgb8888, width, height, 0, 12);
            let outside = rect(right, 0, 10, 10);
            let normal = Transform::Normal;
            let straight = AlphaMode::Straight;
            let result = decode_rect(&bytes, spec, false, normal, straight, logical, outside);
            assert!(result.is_err());
        }
    }

    #[test]
    fn decode_into_reuses_the_destination_allocation() {
        let (first, spec) = buffer(Format::Argb8888, 9, 5, 3, 1);
        let (second, _) = buffer(Format::Argb8888, 9, 5, 3, 2);
        let mut image = RgbaImage::new(0, 0);
        decode_into(
            &first,
            spec,
            false,
            Transform::Normal,
            AlphaMode::Straight,
            &mut image,
        )
        .unwrap();
        let storage = image.as_raw().as_ptr();
        // Same output size, even in another orientation: storage kept, every pixel replaced.
        for (inverted, transform) in [
            (false, Transform::Normal),
            (true, Transform::Flipped180),
            (true, Transform::_180),
        ] {
            decode_into(
                &second,
                spec,
                inverted,
                transform,
                AlphaMode::Straight,
                &mut image,
            )
            .unwrap();
            assert_eq!(image.as_raw().as_ptr(), storage);
            let expected = reference_decode(&second, spec, inverted, transform).unwrap();
            assert_same(&image, &expected, &format!("{transform:?}"));
        }
        // Rotation swaps the output dimensions.
        decode_into(
            &second,
            spec,
            false,
            Transform::_90,
            AlphaMode::Opaque,
            &mut image,
        )
        .unwrap();
        assert_eq!(image.dimensions(), (5, 9));
        // A rejected buffer leaves the image alone.
        let before = image.clone();
        let truncated = &second[..second.len() - 1];
        let result = decode_into(
            truncated,
            spec,
            false,
            Transform::_90,
            AlphaMode::Opaque,
            &mut image,
        );
        assert!(result.is_err());
        assert_eq!(image, before);
    }

    /// Random colors; `alpha` gives each pixel's alpha by index.
    fn noise_frame(
        (width, height): (u32, u32),
        (logical_width, logical_height): (u32, u32),
        alpha: impl Fn(usize) -> u8,
    ) -> CapturedFrame {
        let mut raw = noise(u64::from(width * height), (width * height * 4) as usize);
        for (i, pixel) in raw.chunks_exact_mut(4).enumerate() {
            pixel[3] = alpha(i);
        }
        CapturedFrame {
            image: RgbaImage::from_raw(width, height, raw).unwrap(),
            logical_width,
            logical_height,
        }
    }

    #[test]
    fn stream_rgb_matches_the_per_pixel_reference() {
        for (size, logical) in [
            ((37, 21), (37, 21)),
            ((37, 21), (25, 14)),
            ((64, 36), (32, 18)),
        ] {
            let frame = noise_frame(size, logical, |i| ALPHAS[i % ALPHAS.len()]);
            for mode in [PixelMode::Logical, PixelMode::Native] {
                for limit in [None, Some(1), Some(20), Some(1000)] {
                    assert_eq!(
                        frame.stream_rgb(limit, mode).unwrap(),
                        reference_stream_rgb(&frame, limit, mode).unwrap(),
                        "{size:?} {logical:?} {mode:?} {limit:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn opaque_jpeg_frames_skip_the_rgb_copy_with_identical_output() {
        // Unscaled native and logical frames take the direct path; the scaled
        // cases keep the RGB copy.
        for (size, logical) in [
            ((64, 36), (64, 36)),
            ((37, 21), (37, 21)),
            ((64, 36), (32, 18)),
        ] {
            let frame = noise_frame(size, logical, |_| 255);
            for mode in [PixelMode::Logical, PixelMode::Native] {
                for limit in [None, Some(20), Some(1000)] {
                    for quality in [1, 55, 95] {
                        assert_eq!(
                            frame.jpeg_with_mode(quality, limit, mode).unwrap(),
                            reference_jpeg(&frame, quality, limit, mode).unwrap(),
                            "{size:?} {logical:?} {mode:?} {limit:?} q{quality}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn translucent_jpeg_frames_still_composite_over_black() {
        let frame = CapturedFrame {
            image: RgbaImage::from_fn(64, 32, |x, _| {
                Rgba([255, 255, 255, if x < 32 { 255 } else { 64 }])
            }),
            logical_width: 64,
            logical_height: 32,
        };
        for mode in [PixelMode::Logical, PixelMode::Native] {
            let encoded = frame.jpeg_with_mode(90, None, mode).unwrap();
            assert_eq!(encoded, reference_jpeg(&frame, 90, None, mode).unwrap());
            let decoded = image::load_from_memory(&encoded.0).unwrap().to_rgb8();
            assert!(decoded.get_pixel(8, 16)[0] > 245);
            assert!(decoded.get_pixel(56, 16)[0].abs_diff(64) <= 4);
        }
    }

    #[test]
    #[should_panic(expected = "pixel counts differ")]
    fn row_kernels_reject_mismatched_row_lengths() {
        // Two source pixels into room for one would silently drop a pixel.
        let kernel = row_kernel(Format::Xrgb8888, AlphaMode::Straight).unwrap();
        kernel(&[0; 8], &mut [0; 4]);
    }

    #[test]
    fn opacity_check_finds_any_translucent_pixel() {
        let opaque = RgbaImage::from_pixel(37, 29, Rgba([9, 9, 9, 255]));
        assert!(is_opaque(&opaque));
        for (x, y) in [(0, 0), (18, 14), (36, 28)] {
            let mut image = opaque.clone();
            image.put_pixel(x, y, Rgba([9, 9, 9, 254]));
            assert!(!is_opaque(&image), "({x}, {y})");
        }
    }

    #[test]
    #[ignore = "timing report: cargo test --release -p omabeam-capture -- --ignored --nocapture"]
    fn report_4k_decode_and_jpeg_timings() {
        let (width, height) = (3840u32, 2160u32);
        // Screen-like content: gradients broken up by flat blocks.
        let bytes: Vec<u8> = (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| {
                    let flat = (x / 64 + y / 32) % 5 == 0;
                    let rgb = if flat {
                        0x20_2020
                    } else {
                        (x % 256) << 16 | (y % 256) << 8 | ((x ^ y) % 256)
                    };
                    (0xff00_0000 | rgb).to_ne_bytes()
                })
            })
            .collect();
        let pixels = f64::from(width * height);
        let time = |label: &str, runs: usize, run: &mut dyn FnMut()| {
            run();
            let mut each: Vec<f64> = (0..runs)
                .map(|_| {
                    let started = Instant::now();
                    run();
                    started.elapsed().as_secs_f64()
                })
                .collect();
            each.sort_by(f64::total_cmp);
            let (min, median) = (each[0], each[runs / 2]);
            println!(
                "{label:<44} min {:>7.2} ms  median {:>7.2} ms  {:>6.0} Mpx/s",
                min * 1e3,
                median * 1e3,
                pixels / median / 1e6
            );
        };
        let normal = Transform::Normal;
        let mut image = RgbaImage::new(0, 0);
        for format in [Format::Xrgb8888, Format::Argb8888] {
            let spec = BufferSpec::packed(width, height, format).unwrap();
            time(&format!("{format:?} decode, reference"), 15, &mut || {
                black_box(reference_decode(&bytes, spec, false, normal).unwrap());
            });
            let label = format!("{format:?} decode (Straight, new image)");
            time(&label, 31, &mut || {
                black_box(decode(&bytes, spec, false, normal).unwrap());
            });
            for alpha in [AlphaMode::Straight, AlphaMode::Opaque] {
                let label = format!("{format:?} decode_into ({alpha:?}, reused)");
                time(&label, 31, &mut || {
                    decode_into(&bytes, spec, false, normal, alpha, &mut image).unwrap();
                    black_box(&image);
                });
            }
        }
        let frame = CapturedFrame {
            image,
            logical_width: width,
            logical_height: height,
        };
        let native = PixelMode::Native;
        time("stream_rgb, reference", 15, &mut || {
            black_box(reference_stream_rgb(&frame, None, native).unwrap());
        });
        time("stream_rgb", 31, &mut || {
            black_box(frame.stream_rgb(None, native).unwrap());
        });
        time("JPEG q55, reference (RGB copy)", 9, &mut || {
            black_box(reference_jpeg(&frame, 55, None, native).unwrap());
        });
        time("JPEG q55 (opaque, direct RGBA)", 9, &mut || {
            black_box(frame.jpeg_with_mode(55, None, native).unwrap());
        });
    }
}
