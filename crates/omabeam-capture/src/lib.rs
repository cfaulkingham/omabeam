#![cfg_attr(feature = "simd", feature(portable_simd))]

//! Native Wayland capture and selection. Each session owns its connection and
//! event queue and can run on a background thread independently of the GUI.

mod capture;
mod connection;
#[cfg(feature = "dmabuf")]
mod dmabuf;
mod pixels;
mod selector;

#[cfg(test)]
mod protocol_tests;

pub use capture::{CaptureOptions, CaptureSession, StillCapturer};
#[cfg(feature = "dmabuf")]
pub use dmabuf::{GpuFrame, demo_gpu_frame, demo_linear_gpu_frame, gpu_available};
#[cfg(not(feature = "dmabuf"))]
pub use dmabuf_stub::{GpuFrame, gpu_available};
pub use pixels::{AlphaMode, CapturedFrame, PixelMode};
pub use selector::pick_region;

/// A rectangle in logical pixels. Regions are output-relative; DesktopRect
/// uses compositor layout coordinates, including negative monitor positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub(crate) fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }
    pub(crate) fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    pub(crate) fn intersect(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > i64::from(x) && bottom > i64::from(y)).then(|| Self {
            x,
            y,
            width: (right - i64::from(x)) as u32,
            height: (bottom - i64::from(y)) as u32,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub output: String,
    pub rect: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    Output(String),
    Toplevel(String),
    Region(Region),
    DesktopRect(Rect),
}

/// Synthetic image for exercising the complete streaming path without a desktop.
pub fn demo_frame(frame: u32) -> CapturedFrame {
    let image = image::RgbaImage::from_fn(640, 360, |x, y| {
        let moving = (x.wrapping_add(frame.wrapping_mul(7)) % 640) < 100;
        image::Rgba([
            if moving { 122 } else { (x % 256) as u8 },
            (y % 256) as u8,
            180,
            255,
        ])
    });
    CapturedFrame {
        image,
        logical_width: 640,
        logical_height: 360,
        gpu: None,
    }
}

#[cfg(not(feature = "dmabuf"))]
mod dmabuf_stub {
    //! Stand-in so `CapturedFrame.gpu` exists in every build. The private
    //! field keeps it unconstructible: these methods are unreachable.
    #[derive(Debug, Clone)]
    pub struct GpuFrame {
        _never: (),
    }

    impl GpuFrame {
        pub fn pixel_dimensions(&self) -> (u32, u32) {
            unreachable!("GPU capture is not compiled in")
        }

        pub(crate) fn readback(&self) -> anyhow::Result<(Vec<u8>, crate::pixels::BufferSpec)> {
            unreachable!("GPU capture is not compiled in")
        }

        pub fn gpu_capture_stopped(&self) -> Option<String> {
            unreachable!("GPU capture is not compiled in")
        }
    }

    pub fn gpu_available() -> bool {
        false
    }
}
