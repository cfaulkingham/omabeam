//! GBM dmabuf capture for the zero-copy GPU encode path.
//!
//! The session captures into a ring of dmabufs; the encoder helper imports
//! the same buffers, so live H.264 frames never touch the CPU. The ring is
//! shared with published frames through [`GpuFrame`]: a slot stays busy until
//! the last frame referencing it drops, so capture can never overwrite a
//! buffer the encoder still reads. Anything that needs CPU pixels (JPEG,
//! screenshots) maps the buffer on demand instead. Where that map or the
//! encoder fails, the frame stops its session's dmabuf capture
//! ([`GpuFrame::stop_gpu_capture`]) and later frames use shared memory.
use crate::pixels::BufferSpec;
use anyhow::{Context, Result, ensure};
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Why a capture session gave up on dmabufs, shared with every frame it
/// publishes. Set once, by whichever consumer could not use a dmabuf; the
/// session then captures shared memory until it ends.
#[derive(Clone, Default)]
pub(crate) struct GpuStop(Arc<Mutex<Option<String>>>);

impl GpuStop {
    fn stop(&self, reason: String) {
        let mut stopped = self.0.lock().unwrap();
        if stopped.is_none() {
            *stopped = Some(reason.chars().take(300).collect());
        }
    }

    pub(crate) fn reason(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Decode a Linux dev_t into its major/minor numbers (glibc encoding).
fn dev_major_minor(dev: u64) -> (u32, u32) {
    let major = (((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000)) as u32;
    let minor = ((dev & 0xff) | ((dev >> 12) & 0xffff_ff00)) as u32;
    (major, minor)
}

/// Match the compositor's dmabuf device to one of our render nodes through
/// sysfs. The compositor may name a primary node while capture allocates on
/// render nodes for the same GPU, so raw dev_t equality is never used.
/// Returns `None` when the device cannot be resolved; the caller then falls
/// back to node order.
pub(crate) fn node_for_device(dev: &[u8]) -> Option<PathBuf> {
    let raw: [u8; 8] = dev.try_into().ok()?;
    let (major, minor) = dev_major_minor(u64::from_le_bytes(raw));
    let sysfs = std::fs::canonicalize(format!("/sys/dev/char/{major}:{minor}")).ok()?;
    let name = sysfs.file_name()?.to_string_lossy();
    if let Some(rest) = name.strip_prefix("renderD")
        && !rest.is_empty()
        && rest.bytes().all(|b| b.is_ascii_digit())
    {
        let node = PathBuf::from("/dev/dri").join(name.as_ref());
        if node.exists() {
            return Some(node);
        }
    }
    // A primary node: find the render node behind the same PCI device.
    let device = std::fs::canonicalize(sysfs.join("device")).ok()?;
    render_nodes().into_iter().find(|node| {
        node.file_name()
            .and_then(|n| {
                std::fs::canonicalize(format!("/sys/class/drm/{}", n.to_string_lossy())).ok()
            })
            .and_then(|drm| std::fs::canonicalize(drm.join("device")).ok())
            .is_some_and(|candidate| candidate == device)
    })
}

/// DRM render nodes, lowest first. Integrated graphics usually own renderD128.
pub fn render_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix("renderD")
                .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .map(|_| entry.path())
        })
        .collect();
    nodes.sort();
    nodes
}

fn prefer_node(nodes: &mut Vec<PathBuf>, preferred: &std::path::Path) {
    if let Some(position) = nodes.iter().position(|node| node == preferred) {
        let node = nodes.remove(position);
        nodes.insert(0, node);
    }
}

/// True when a render node opens and initializes GBM: the capture session can
/// offer dmabuf buffers. Cheap enough to run once per live share.
pub fn gpu_available() -> bool {
    render_nodes().iter().any(|node| {
        File::open(node)
            .ok()
            .and_then(|file| gbm::Device::new(file).ok())
            .is_some()
    })
}

struct RingState {
    busy: Vec<bool>,
    generation: u64,
}

struct Shared {
    // Owns the DRM fd: libgbm resolves buffer fds through it, so dropping it
    // while buffers live breaks every later export.
    #[allow(dead_code)]
    device: gbm::Device<File>,
    // Locked for readback mapping: some drivers only map with read/write
    // transfers, which needs exclusive access. Held briefly; never nested.
    buffers: Mutex<Vec<gbm::BufferObject<()>>>,
    /// The render node that allocated the buffers; the helper imports there.
    node: PathBuf,
    format: wayland_client::protocol::wl_shm::Format,
    modifier: u64,
    width: u32,
    height: u32,
    state: Mutex<RingState>,
    stop: GpuStop,
}

/// A ring of GBM capture buffers. Published frames share the `Arc`, so a
/// ring rebuilt under them (resize, retarget) stays readable until the last
/// frame using it drops.
pub(crate) struct GpuRing {
    shared: Arc<Shared>,
}

impl GpuRing {
    /// Allocate `count` XRGB8888 buffers, preferring a linear layout so a
    /// fallback readback is a plain row copy. Tries every render node; the
    /// first that allocates the whole ring wins. `generation` distinguishes
    /// this ring from earlier ones for the encoder helper.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        width: u32,
        height: u32,
        modifier: u64,
        count: u32,
        generation: u64,
        preferred: Option<&Path>,
        stop: &GpuStop,
    ) -> Result<Self> {
        ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "odd dmabuf size"
        );
        let mut nodes = render_nodes();
        if modifier != omabeam_encoder::dmabuf::MOD_LINEAR {
            // A tiled layout belongs to one GPU (see `pick_layout`).
            let device = preferred.context("a tiled dmabuf needs the compositor's render node")?;
            nodes.retain(|node| node == device);
        } else {
            // The compositor's device first; an explicit
            // `OMABEAM_RENDER_NODE` still wins over the feedback.
            if let Some(preferred) = preferred {
                prefer_node(&mut nodes, preferred);
            }
            if let Some(env) = std::env::var_os("OMABEAM_RENDER_NODE").map(PathBuf::from) {
                prefer_node(&mut nodes, &env);
            }
        }
        ensure!(!nodes.is_empty(), "no DRM render node for dmabuf capture");
        let mut failure = None;
        for node in &nodes {
            match Self::open_on(node, width, height, modifier, count, generation, stop) {
                Ok(ring) => return Ok(ring),
                Err(error) => failure = Some(anyhow::anyhow!("{node:?}: {error:#}")),
            }
        }
        Err(failure.unwrap_or_else(|| anyhow::anyhow!("no DRM render node for dmabuf capture")))
    }

    /// Test-only allocation without compositor feedback: linear when the
    /// driver allows it, else whatever layout it picks.
    fn open_for_test(width: u32, height: u32, count: u32) -> Result<Self> {
        Self::open(
            width,
            height,
            omabeam_encoder::dmabuf::MOD_LINEAR,
            count,
            1,
            None,
            &GpuStop::default(),
        )
        .or_else(|_| {
            let nodes = render_nodes();
            ensure!(!nodes.is_empty(), "no DRM render node for dmabuf capture");
            let mut failure = None;
            for node in &nodes {
                let file = File::open(node)?;
                let device = gbm::Device::new(file)?;
                let mut buffers = Vec::new();
                for _ in 0..count {
                    match device.create_buffer_object::<()>(
                        width,
                        height,
                        drm_fourcc::DrmFourcc::Xrgb8888,
                        gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING,
                    ) {
                        Ok(buffer) => buffers.push(buffer),
                        Err(error) => {
                            failure = Some(anyhow::anyhow!("{error:#}"));
                            break;
                        }
                    }
                }
                if buffers.len() == count as usize {
                    let modifier = buffers[0].modifier().into();
                    return Ok(Self {
                        shared: Arc::new(Shared {
                            device,
                            buffers: Mutex::new(buffers),
                            node: node.clone(),
                            format: wayland_client::protocol::wl_shm::Format::Xrgb8888,
                            modifier,
                            width,
                            height,
                            state: Mutex::new(RingState {
                                busy: vec![false; count as usize],
                                generation: 1,
                            }),
                            stop: GpuStop::default(),
                        }),
                    });
                }
            }
            Err(failure.unwrap_or_else(|| anyhow::anyhow!("GBM buffer allocation failed")))
        })
    }

    fn open_on(
        node: &Path,
        width: u32,
        height: u32,
        modifier: u64,
        count: u32,
        generation: u64,
        stop: &GpuStop,
    ) -> Result<Self> {
        use gbm::BufferObjectFlags as Flags;
        let file = File::open(node).with_context(|| format!("cannot open {node:?}"))?;
        let device = gbm::Device::new(file).context("cannot initialize GBM")?;
        let format = drm_fourcc::DrmFourcc::Xrgb8888;
        // Exactly the advertised layout: GBM must not substitute a modifier
        // the compositor was never asked about.
        let modifiers = vec![drm_fourcc::DrmModifier::from(modifier)];
        let with_modifiers = |flags| {
            device.create_buffer_object_with_modifiers2::<()>(
                width,
                height,
                format,
                modifiers.clone().into_iter(),
                flags,
            )
        };
        let plain = |flags| device.create_buffer_object::<()>(width, height, format, flags);
        // Explicit modifiers first so the layout matches the compositor
        // feedback; plain allocation lets the driver pick when it refuses.
        // NVIDIA refuses RENDERING on linear buffers but allocates them
        // without it, and the compositor still renders into them.
        let mut buffers = Vec::new();
        for _ in 0..count {
            let buffer = with_modifiers(Flags::RENDERING)
                .or_else(|_| with_modifiers(Flags::empty()))
                .or_else(|_| plain(Flags::RENDERING | Flags::LINEAR))
                .or_else(|_| plain(Flags::LINEAR))
                .context("GBM buffer allocation failed")?;
            buffers.push(buffer);
        }
        // The ring shares one layout: mixed modifiers would need per-buffer
        // wl_buffer parameters and helper imports. Fail closed when the
        // driver ignores the requested layout: only an advertised modifier
        // may be attached.
        let actual: u64 = buffers[0].modifier().into();
        ensure!(
            buffers
                .iter()
                .all(|b| b.modifier() == buffers[0].modifier()),
            "GBM allocated mixed dmabuf modifiers"
        );
        ensure!(
            actual == modifier,
            "GBM allocated an unrequested dmabuf layout"
        );
        let modifier = actual;
        Ok(Self {
            shared: Arc::new(Shared {
                device,
                buffers: Mutex::new(buffers),
                node: node.to_owned(),
                format: wayland_client::protocol::wl_shm::Format::Xrgb8888,
                modifier,
                width,
                height,
                state: Mutex::new(RingState {
                    busy: vec![false; count as usize],
                    generation,
                }),
                stop: stop.clone(),
            }),
        })
    }

    pub(crate) fn width(&self) -> u32 {
        self.shared.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.shared.height
    }

    pub(crate) fn modifier(&self) -> u64 {
        self.shared.modifier
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.shared.state.lock().unwrap().generation
    }

    /// New fds for one buffer, for a wl_buffer or the helper handoff. Each
    /// call dups; the GBM object keeps its own.
    pub(crate) fn buffer_fds(&self, index: usize) -> Result<Vec<OwnedFd>> {
        let buffers = self.shared.buffers.lock().unwrap();
        let buffer = buffers.get(index).context("dmabuf index out of range")?;
        // XRGB8888 is single-plane: one fd describes the whole buffer.
        Ok(vec![buffer.fd().context("cannot export dmabuf fd")?])
    }

    pub(crate) fn stride(&self, index: usize) -> Result<u32> {
        let buffers = self.shared.buffers.lock().unwrap();
        let buffer = buffers.get(index).context("dmabuf index out of range")?;
        Ok(buffer.stride_for_plane(0))
    }

    pub(crate) fn offset(&self, index: usize) -> Result<u32> {
        let buffers = self.shared.buffers.lock().unwrap();
        let buffer = buffers.get(index).context("dmabuf index out of range")?;
        Ok(buffer.offset(0))
    }

    /// Claim a free slot for the next capture. `None` means every buffer is
    /// still with the encoder; the caller skips the frame and retries.
    pub(crate) fn claim(&self) -> Option<usize> {
        let mut state = self.shared.state.lock().unwrap();
        let index = state.busy.iter().position(|busy| !busy)?;
        state.busy[index] = true;
        Some(index)
    }

    /// Release a claim the capture thread still owns: a cancelled frame or a
    /// publish that never happened. Published frames release on drop.
    pub(crate) fn unclaim(&self, index: usize) {
        let mut state = self.shared.state.lock().unwrap();
        if state.busy.get(index) == Some(&true) {
            state.busy[index] = false;
        }
    }

    pub(crate) fn matches(&self, width: u32, height: u32, modifier: u64) -> bool {
        self.shared.width == width
            && self.shared.height == height
            && self.shared.modifier == modifier
    }

    fn release(shared: &Shared, index: usize, generation: u64) {
        let mut state = shared.state.lock().unwrap();
        if state.generation == generation {
            state.busy[index] = false;
        }
    }

    /// Publish a captured buffer: the slot stays busy until the frame drops.
    /// Dups the buffer's fd for the helper handoff.
    pub(crate) fn frame(&self, index: usize) -> Result<GpuFrame> {
        let state = self.shared.state.lock().unwrap();
        ensure!(
            state.busy.get(index) == Some(&true),
            "publishing a free dmabuf slot"
        );
        let buffers = self.shared.buffers.lock().unwrap();
        let buffer = buffers.get(index).context("dmabuf index out of range")?;
        let stride = buffer.stride_for_plane(0);
        let offset = buffer.offset(0);
        drop(buffers);
        let fd = self
            .buffer_fds(index)?
            .pop()
            .context("dmabuf without an fd")?;
        Ok(GpuFrame {
            inner: Arc::new(Frame {
                index: index as u32,
                generation: state.generation,
                width: self.shared.width,
                height: self.shared.height,
                stride,
                offset,
                modifier: self.shared.modifier,
                fd: Arc::new(fd),
                ring: Arc::clone(&self.shared),
            }),
        })
    }
}

struct Frame {
    index: u32,
    generation: u64,
    width: u32,
    height: u32,
    stride: u32,
    offset: u32,
    modifier: u64,
    fd: Arc<OwnedFd>,
    ring: Arc<Shared>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        GpuRing::release(&self.ring, self.index as usize, self.generation);
    }
}

/// A captured dmabuf, safe to send to the encoder thread. The slot releases
/// when the last clone drops. CPU access maps the buffer on demand and only
/// for paths that truly need pixels (JPEG, screenshots).
#[derive(Clone)]
pub struct GpuFrame {
    inner: Arc<Frame>,
}

impl std::fmt::Debug for GpuFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuFrame")
            .field("index", &self.inner.index)
            .field("generation", &self.inner.generation)
            .field("width", &self.inner.width)
            .field("height", &self.inner.height)
            .finish_non_exhaustive()
    }
}

impl GpuFrame {
    pub fn buffer_index(&self) -> u32 {
        self.inner.index
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub fn pixel_dimensions(&self) -> (u32, u32) {
        (self.inner.width, self.inner.height)
    }

    pub fn handoff_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.inner.fd.as_fd()
    }

    /// The render node that allocated the buffer, `/dev/dri/renderD*`.
    pub fn render_node(&self) -> &Path {
        &self.inner.ring.node
    }

    /// Move the capture session that produced this frame to shared memory
    /// for the rest of its life: for consumers that cannot use its dmabufs.
    /// The first reason wins and becomes the session's GPU note.
    pub fn stop_gpu_capture(&self, reason: String) {
        self.inner.ring.stop.stop(reason);
    }

    /// Why this frame's session stopped dmabuf capture, if it has.
    pub fn gpu_capture_stopped(&self) -> Option<String> {
        self.inner.ring.stop.reason()
    }

    pub fn frame_descriptor(&self) -> omabeam_encoder::dmabuf::FrameDescriptor {
        omabeam_encoder::dmabuf::FrameDescriptor {
            id: omabeam_encoder::dmabuf::GpuBufferId {
                index: self.inner.index,
                generation: self.inner.generation,
            },
            width: self.inner.width,
            height: self.inner.height,
            stride: self.inner.stride,
            offset: self.inner.offset,
            modifier: self.inner.modifier,
            fourcc: omabeam_encoder::dmabuf::FOURCC_XRGB8888,
        }
    }

    /// Map the buffer and copy its rows: the JPEG/screenshot fallback. The
    /// H.264 path never calls this. A driver that refuses the map (NVIDIA)
    /// stops the session's dmabuf capture, so later frames carry pixels.
    pub(crate) fn readback(&self) -> Result<(Vec<u8>, BufferSpec)> {
        self.map_rows().inspect_err(|error| {
            self.stop_gpu_capture(format!("dmabuf readback unavailable: {error:#}"));
        })
    }

    fn map_rows(&self) -> Result<(Vec<u8>, BufferSpec)> {
        let shared = &self.inner.ring;
        let mut buffers = shared.buffers.lock().unwrap();
        let buffer = buffers
            .get_mut(self.inner.index as usize)
            .context("dmabuf index out of range")?;
        let (width, height) = (self.inner.width, self.inner.height);
        // Read-only first; some drivers (virtio) only serve read/write
        // transfers. Nothing is written either way.
        let bytes = buffer
            .map(0, 0, width, height, |mapped| mapped.buffer().to_vec())
            .or_else(|_| buffer.map_mut(0, 0, width, height, |mapped| mapped.buffer().to_vec()))
            .map_err(|error| match error.raw_os_error() {
                // NVIDIA's GBM refuses without setting errno.
                Some(0) => anyhow::anyhow!("the GPU driver does not map dmabufs for the CPU"),
                _ => anyhow::anyhow!("cannot map dmabuf for readback: {error:#}"),
            })?;
        Ok((
            bytes,
            BufferSpec {
                width: self.inner.width,
                height: self.inner.height,
                stride: self.inner.stride,
                format: shared.format,
            },
        ))
    }

    #[cfg(test)]
    pub(crate) fn slot_is_busy(&self) -> bool {
        self.inner.ring.state.lock().unwrap().busy[self.inner.index as usize]
    }
}

/// A real GBM dmabuf for exercising the zero-copy handoff without a
/// compositor. `None` where no render node allocates; callers skip, like
/// the encoder hardware tests.
pub fn demo_gpu_frame() -> Option<GpuFrame> {
    let ring = GpuRing::open_for_test(640, 360, 2).ok()?;
    let slot = ring.claim()?;
    ring.frame(slot).ok()
}

/// A linear dmabuf for the readback fallback: only a mappable layout can
/// stand in for the linear-only capture ring. `None` where the driver
/// refuses linear or its CPU map, as on NVIDIA; callers skip.
pub fn demo_linear_gpu_frame() -> Option<GpuFrame> {
    let stop = GpuStop::default();
    let ring = GpuRing::open(
        640,
        360,
        omabeam_encoder::dmabuf::MOD_LINEAR,
        2,
        1,
        None,
        &stop,
    )
    .ok()?;
    let slot = ring.claim()?;
    let frame = ring.frame(slot).ok()?;
    frame.map_rows().ok()?;
    Some(frame)
}

/// Pick the dmabuf layout from the compositor's `DmabufFormat` feedback,
/// skipping layouts it already failed to capture into (`rejected`).
///
/// Linear XRGB8888 first: Mesa compositors render into it, and Mesa maps it
/// for JPEG readback. NVIDIA's EGL cannot render into linear buffers, so
/// Hyprland fails those captures (reason `Unknown`); after that the
/// compositor's own first choice is used, which on NVIDIA is the block-linear
/// layout of its swapchain (verified with Hyprland 0.56.2). A tiled layout
/// only imports on the GPU that allocated it, so it needs `on_device`: the
/// ring then allocates on the compositor's render node alone. Allocated on
/// another GPU, the import fails with a fatal protocol error. Tiled-only
/// feedback with no linear attempt stays on shared memory.
///
/// An `INVALID` offer constrains nothing, so it resolves to linear.
///
/// `OMABEAM_DMABUF_ANY_MODIFIER=1` takes the compositor's first usable
/// layout from the start, for verifying layouts the default reaches late.
pub(crate) fn pick_layout(
    formats: &[(u32, Vec<u64>)],
    rejected: &[u64],
    on_device: bool,
) -> Option<(u32, u64)> {
    let any = std::env::var_os("OMABEAM_DMABUF_ANY_MODIFIER").is_some_and(|v| v == "1");
    pick_layout_with(formats, rejected, on_device, any)
}

fn pick_layout_with(
    formats: &[(u32, Vec<u64>)],
    rejected: &[u64],
    on_device: bool,
    any_modifier: bool,
) -> Option<(u32, u64)> {
    use omabeam_encoder::dmabuf::{FOURCC_XRGB8888, MOD_INVALID, MOD_LINEAR};
    let (_, modifiers) = formats
        .iter()
        .find(|(fourcc, _)| *fourcc == FOURCC_XRGB8888)?;
    let mut offered = modifiers.iter().map(|&modifier| match modifier {
        MOD_INVALID => MOD_LINEAR,
        modifier => modifier,
    });
    let usable =
        |modifier: &u64| !rejected.contains(modifier) && (*modifier == MOD_LINEAR || on_device);
    let modifier = if !any_modifier && !rejected.contains(&MOD_LINEAR) {
        offered.find(|&modifier| modifier == MOD_LINEAR)?
    } else {
        offered.find(usable)?
    };
    Some((FOURCC_XRGB8888, modifier))
}

/// A ring plus its Wayland buffers, one proxy per slot. Rebuilt when the
/// capture constraints change size; each build takes a fresh generation so
/// the encoder helper can tell rings apart.
pub(crate) struct GpuSession {
    pub ring: GpuRing,
    pub buffers: Vec<wayland_client::protocol::wl_buffer::WlBuffer>,
    next_generation: u64,
    stop: GpuStop,
}

impl Drop for GpuSession {
    fn drop(&mut self) {
        for buffer in self.buffers.drain(..) {
            buffer.destroy();
        }
    }
}

impl GpuSession {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        dmabuf: &wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        width: u32,
        height: u32,
        modifier: u64,
        count: u32,
        preferred: Option<&Path>,
        stop: &GpuStop,
        qh: &wayland_client::QueueHandle<crate::connection::State>,
    ) -> Result<Self> {
        let mut session = Self {
            ring: GpuRing::open(width, height, modifier, count, 1, preferred, stop)?,
            buffers: Vec::new(),
            next_generation: 2,
            stop: stop.clone(),
        };
        session.create_buffers(dmabuf, qh)?;
        Ok(session)
    }

    /// Rebuild unless the ring already matches. The caller cancels its
    /// in-flight dmabuf frame first; published frames keep the old ring alive
    /// through their own references.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure(
        &mut self,
        dmabuf: &wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        width: u32,
        height: u32,
        modifier: u64,
        count: u32,
        preferred: Option<&Path>,
        qh: &wayland_client::QueueHandle<crate::connection::State>,
    ) -> Result<()> {
        if self.ring.matches(width, height, modifier) {
            return Ok(());
        }
        for buffer in self.buffers.drain(..) {
            buffer.destroy();
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.ring = GpuRing::open(
            width, height, modifier, count, generation, preferred, &self.stop,
        )?;
        self.create_buffers(dmabuf, qh)
    }

    fn create_buffers(
        &mut self,
        dmabuf: &wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        qh: &wayland_client::QueueHandle<crate::connection::State>,
    ) -> Result<()> {
        let count = self.ring.shared.buffers.lock().unwrap().len();
        for index in 0..count {
            let fds = self.ring.buffer_fds(index)?;
            let stride = self.ring.stride(index)?;
            let offset = self.ring.offset(index)?;
            let params = dmabuf.create_params(qh, ());
            params.add(
                fds[0].as_fd(),
                0,
                offset,
                stride,
                (self.ring.modifier() >> 32) as u32,
                self.ring.modifier() as u32,
            );
            let buffer = params.create_immed(
                self.ring.width() as i32,
                self.ring.height() as i32,
                omabeam_encoder::dmabuf::FOURCC_XRGB8888,
                wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags::empty(),
                qh,
                (),
            );
            params.destroy();
            self.buffers.push(buffer);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs wherever GBM allocates (a DRM render node) and checks nothing
    /// where it does not, like the encoder hardware tests.
    #[test]
    fn ring_allocates_and_releases_slots() {
        let ring = match GpuRing::open_for_test(640, 360, 2) {
            Ok(ring) => ring,
            Err(error) => {
                eprintln!("skipped: {error:#}");
                return;
            }
        };
        assert_eq!((ring.width(), ring.height()), (640, 360));
        assert_eq!(ring.generation(), 1);
        let first = ring.claim().unwrap();
        let frame = ring.frame(first).unwrap();
        assert_eq!(frame.pixel_dimensions(), (640, 360));
        assert!(
            render_nodes()
                .iter()
                .any(|node| node == frame.render_node())
        );
        let descriptor = frame.frame_descriptor();
        assert_eq!(descriptor.id.index, first as u32);
        assert_eq!(descriptor.id.generation, 1);
        omabeam_encoder::dmabuf::FrameDescriptor::decode(&descriptor.encode(), 2).unwrap();
        assert!(frame.slot_is_busy());
        assert!(ring.claim().is_some());
        assert_eq!(ring.claim(), None);
        drop(frame);
        assert_eq!(ring.claim(), Some(first));
    }

    #[test]
    fn dev_t_decodes_to_major_minor() {
        assert_eq!(dev_major_minor(0x0000_0000_0000_e281), (226, 129));
        assert_eq!(dev_major_minor(0x0000_0000_0000_e280), (226, 128));
        assert_eq!(dev_major_minor(0x0000_0000_0000_0801), (8, 1));
    }

    #[test]
    fn device_matching_rejects_garbage() {
        assert_eq!(node_for_device(&[]), None);
        assert_eq!(node_for_device(&[1, 2, 3]), None);
        assert_eq!(node_for_device(&[0xff; 8]), None);
    }

    /// Hyprland's XR24 feedback on NVIDIA: block-linear first, then linear.
    const NVIDIA_TILED: u64 = 0x0300_0000_0060_6014;

    #[test]
    fn layout_starts_linear_xrgb() {
        use omabeam_encoder::dmabuf::{FOURCC_ARGB8888, FOURCC_XRGB8888, MOD_LINEAR};
        let pick = |formats: &[(u32, Vec<u64>)]| pick_layout_with(formats, &[], true, false);
        assert_eq!(
            pick(&[(FOURCC_XRGB8888, vec![NVIDIA_TILED, MOD_LINEAR])]),
            Some((FOURCC_XRGB8888, MOD_LINEAR))
        );
        // Tiled-only feedback stays on shared memory (see `pick_layout`).
        assert_eq!(pick(&[(FOURCC_XRGB8888, vec![NVIDIA_TILED])]), None);
        assert_eq!(pick(&[(FOURCC_ARGB8888, vec![MOD_LINEAR])]), None);
        assert_eq!(pick(&[(FOURCC_XRGB8888, vec![])]), None);
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn a_rejected_linear_layout_falls_back_to_the_compositors_choice_on_its_gpu() {
        use omabeam_encoder::dmabuf::{FOURCC_XRGB8888, MOD_LINEAR};
        let feedback = [(FOURCC_XRGB8888, vec![NVIDIA_TILED, MOD_LINEAR])];
        assert_eq!(
            pick_layout_with(&feedback, &[MOD_LINEAR], true, false),
            Some((FOURCC_XRGB8888, NVIDIA_TILED))
        );
        // Off the compositor's device a tiled buffer would not import.
        assert_eq!(
            pick_layout_with(&feedback, &[MOD_LINEAR], false, false),
            None
        );
        assert_eq!(
            pick_layout_with(&feedback, &[MOD_LINEAR, NVIDIA_TILED], true, false),
            None
        );
    }

    #[test]
    fn any_modifier_takes_the_first_usable_layout() {
        use omabeam_encoder::dmabuf::{FOURCC_XRGB8888, MOD_LINEAR};
        let feedback = [(FOURCC_XRGB8888, vec![NVIDIA_TILED, MOD_LINEAR])];
        assert_eq!(
            pick_layout_with(&feedback, &[], true, true),
            Some((FOURCC_XRGB8888, NVIDIA_TILED))
        );
        assert_eq!(
            pick_layout_with(&feedback, &[], false, true),
            Some((FOURCC_XRGB8888, MOD_LINEAR))
        );
        assert_eq!(
            pick_layout_with(&feedback, &[NVIDIA_TILED], true, true),
            Some((FOURCC_XRGB8888, MOD_LINEAR))
        );
        assert_eq!(
            pick_layout_with(&[(FOURCC_XRGB8888, vec![])], &[], true, true),
            None
        );
        assert_eq!(pick_layout_with(&[], &[], true, true), None);
    }

    #[test]
    fn a_tiled_ring_allocates_only_on_the_compositors_node() {
        let error = GpuRing::open(640, 360, NVIDIA_TILED, 2, 1, None, &GpuStop::default())
            .err()
            .expect("no compositor node");
        assert!(
            format!("{error:#}").contains("compositor's render node"),
            "{error:#}"
        );
    }

    /// Either the linear buffer maps, or (NVIDIA) the refusal stops dmabuf
    /// capture for every frame of the session, the next ring included.
    #[test]
    fn readback_maps_a_linear_buffer_or_stops_gpu_capture() {
        let stop = GpuStop::default();
        let open = || {
            GpuRing::open(
                640,
                360,
                omabeam_encoder::dmabuf::MOD_LINEAR,
                2,
                1,
                None,
                &stop,
            )
        };
        let ring = match open() {
            Ok(ring) => ring,
            Err(error) => {
                eprintln!("skipped: no linear GBM allocation: {error:#}");
                return;
            }
        };
        let frame = ring.frame(ring.claim().unwrap()).unwrap();
        assert_eq!(frame.gpu_capture_stopped(), None);
        match frame.readback() {
            Ok((bytes, spec)) => {
                assert_eq!((spec.width, spec.height), (640, 360));
                assert!(spec.stride >= 640 * 4);
                assert_eq!(bytes.len(), spec.stride as usize * 360);
                assert_eq!(stop.reason(), None);
            }
            Err(error) => {
                let reason = frame.gpu_capture_stopped().unwrap_or_default();
                assert!(
                    reason.contains("readback unavailable"),
                    "{error:#}: {reason}"
                );
                let next = open().unwrap();
                let later = next.frame(next.claim().unwrap()).unwrap();
                assert_eq!(later.gpu_capture_stopped(), Some(reason));
            }
        }
    }

    #[test]
    fn the_first_stop_reason_wins_and_is_bounded() {
        let stop = GpuStop::default();
        stop.stop("x".repeat(1000));
        stop.stop("second".into());
        assert_eq!(stop.reason(), Some("x".repeat(300)));
    }

    #[test]
    fn invalid_resolves_to_linear_in_both_modes() {
        use omabeam_encoder::dmabuf::{FOURCC_XRGB8888, MOD_INVALID, MOD_LINEAR};
        for any_modifier in [false, true] {
            assert_eq!(
                pick_layout_with(
                    &[(FOURCC_XRGB8888, vec![MOD_INVALID])],
                    &[],
                    false,
                    any_modifier
                ),
                Some((FOURCC_XRGB8888, MOD_LINEAR)),
                "any_modifier={any_modifier}"
            );
        }
    }
}
