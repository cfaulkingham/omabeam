//! Zero-copy GPU encode: dmabufs in, H.264 packets out.
//!
//! The app captures into dmabufs and hands one fd per frame over a Unix
//! socket. Each frame maps into the GPU that owns it, a GPU filter converts
//! XRGB to the encoder's NV12 (resizing to the stream on the way), and the
//! hardware encoder consumes the result without pixels crossing the CPU. Any
//! failure here fails the helper, and the parent falls back to the upload
//! path with CPU pixels.
//!
//! CUDA cannot import a dmabuf, so on NVIDIA the buffer maps into Vulkan on
//! the same GPU, `scale_vulkan` converts it into exportable memory, and that
//! memory transfers to CUDA for NVENC. Elsewhere the buffer maps straight
//! into a VA surface and `scale_vaapi` converts it on the video engine.
//!
//! The upload path in [`video`](crate::video) keeps the safe ffmpeg-next API;
//! this module uses raw sys calls throughout because hardware frame mapping
//! and filter graphs with hardware contexts do not fit the safe wrappers.
//! Every unsafe block names its contract. Encoder settings mirror
//! [`video`](crate::video) exactly; keep the two in sync.
use crate::video::checked;
use anyhow::{Context, Result, ensure};
use ffmpeg_next::sys as ffi;
use omabeam_encoder::Config;
use omabeam_encoder::dmabuf::{FOURCC_XRGB8888, FrameDescriptor, GpuBufferId, GpuRing};
use std::ffi::{CString, c_int};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr::{self, null_mut};
use std::time::Duration;

/// Owned `AVBufferRef`: a hardware device, a frames pool, a DRM descriptor.
/// Null on failure paths; the pointer is otherwise a live owned reference.
struct Buf(*mut ffi::AVBufferRef);
impl Buf {
    /// A new reference for C fields that take ownership of one.
    fn reference(&self) -> Result<*mut ffi::AVBufferRef> {
        // SAFETY: self is a live reference; the copy is owned by the caller.
        let copy = unsafe { ffi::av_buffer_ref(self.0) };
        ensure!(!copy.is_null(), "cannot reference a hardware context");
        Ok(copy)
    }
}
impl Drop for Buf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: live owned reference or null, checked above.
            unsafe { ffi::av_buffer_unref(&mut self.0) };
        }
    }
}
/// Owned `AVFrame`, buffers included.
struct Frame(*mut ffi::AVFrame);
impl Frame {
    fn alloc() -> Result<Self> {
        // SAFETY: no input; null on OOM is checked.
        let frame = unsafe { ffi::av_frame_alloc() };
        ensure!(!frame.is_null(), "cannot allocate video frame");
        Ok(Self(frame))
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: allocated above; av_frame_free nulls the pointer.
        unsafe { ffi::av_frame_free(&mut self.0) };
    }
}

/// Owned `AVCodecContext`.
struct Codec(*mut ffi::AVCodecContext);
impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: allocated by open_codec; avcodec_free_context nulls it.
        unsafe { ffi::avcodec_free_context(&mut self.0) };
    }
}

/// Owned filter graph: freeing the graph frees its contexts.
struct Graph {
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
}
impl Drop for Graph {
    fn drop(&mut self) {
        if !self.graph.is_null() {
            // SAFETY: allocated below; frees the contexts with it.
            unsafe { ffi::avfilter_graph_free(&mut self.graph) };
        }
    }
}

/// Frees an `AVDRMFrameDescriptor` built by [`drm_frame`]: closes its fds,
/// then the descriptor itself.
unsafe extern "C" fn drm_free(_opaque: *mut std::ffi::c_void, data: *mut u8) {
    // SAFETY: called by FFmpeg with the descriptor below; nb_objects is 1.
    unsafe {
        let descriptor = data as *mut ffi::AVDRMFrameDescriptor;
        for index in 0..(*descriptor).nb_objects.min(4) {
            let fd = (*descriptor).objects[index as usize].fd;
            if fd >= 0 {
                drop(OwnedFd::from_raw_fd(fd));
            }
        }
        ffi::av_free(data as *mut std::ffi::c_void);
    }
}

/// Wrap one dmabuf as a DRM_PRIME frame. The descriptor is FFmpeg-owned from
/// here: the fd closes when the last frame reference drops, so early errors
/// and mapped frames both stay balanced.
fn drm_frame(fd: OwnedFd, desc: &FrameDescriptor) -> Result<Frame> {
    use std::os::fd::AsFd;
    ensure!(
        desc.fourcc == FOURCC_XRGB8888,
        "zero-copy needs XRGB8888 pixels"
    );
    // The real object size, valid for linear and tiled layouts alike.
    let size = rustix::fs::fstat(fd.as_fd())
        .context("cannot stat dmabuf")?
        .st_size as usize;
    // SAFETY: all pointers are checked; the descriptor is zeroed, fully
    // described, and handed to exactly one AVBufferRef with drm_free.
    unsafe {
        let raw = ffi::av_malloc(size_of::<ffi::AVDRMFrameDescriptor>())
            as *mut ffi::AVDRMFrameDescriptor;
        ensure!(!raw.is_null(), "cannot allocate DRM descriptor");
        ptr::write_bytes(raw, 0, 1);
        (*raw).nb_objects = 1;
        (*raw).objects[0].fd = fd.into_raw_fd();
        (*raw).objects[0].size = size;
        (*raw).objects[0].format_modifier = desc.modifier;
        (*raw).nb_layers = 1;
        (*raw).layers[0].format = desc.fourcc;
        (*raw).layers[0].nb_planes = 1;
        (*raw).layers[0].planes[0].object_index = 0;
        (*raw).layers[0].planes[0].pitch = desc.stride as isize;
        (*raw).layers[0].planes[0].offset = desc.offset as isize;
        let owned = ffi::av_buffer_create(
            raw as *mut u8,
            size_of::<ffi::AVDRMFrameDescriptor>(),
            Some(drm_free),
            null_mut(),
            0,
        );
        if owned.is_null() {
            drm_free(null_mut(), raw as *mut u8);
            anyhow::bail!("cannot allocate DRM descriptor buffer");
        }
        let frame = Frame::alloc()?;
        (*frame.0).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
        (*frame.0).width = desc.width as c_int;
        (*frame.0).height = desc.height as c_int;
        (*frame.0).data[0] = raw as *mut u8;
        (*frame.0).buf[0] = owned;
        Ok(frame)
    }
}

/// A backend and the render node it imports on.
#[derive(Debug, Clone)]
enum BackendKind {
    Nvenc(String),
    Vaapi(String),
}
impl BackendKind {
    fn label(&self) -> String {
        match self {
            Self::Nvenc(node) => format!("NVIDIA NVENC zero-copy ({node})"),
            Self::Vaapi(node) => format!("VA-API zero-copy ({node})"),
        }
    }
    fn encoder(&self) -> &str {
        match self {
            Self::Nvenc(_) => "h264_nvenc",
            Self::Vaapi(_) => "h264_vaapi",
        }
    }
    /// The hardware frames a dmabuf maps into.
    fn map_format(&self) -> ffi::AVPixelFormat {
        match self {
            Self::Nvenc(_) => ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
            Self::Vaapi(_) => ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
        }
    }
    fn encode_format(&self) -> ffi::AVPixelFormat {
        match self {
            Self::Nvenc(_) => ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
            Self::Vaapi(_) => ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
        }
    }
    /// The GPU step to stream size. Colors come out as limited-range BT.601,
    /// the CPU converter's matrix, so a fallback keeps them. NVENC converts
    /// RGB to exactly that itself; `scale_vulkan` only resizes (and copies
    /// into memory CUDA can import), because in FFmpeg 9 its RGB to YUV
    /// conversion swaps red and blue and refuses unscaled frames.
    fn scale_filter(&self, stream: (u32, u32)) -> (&'static str, String) {
        let (w, h) = stream;
        match self {
            Self::Nvenc(_) => ("scale_vulkan", format!("w={w}:h={h}:format=bgr0")),
            Self::Vaapi(_) => (
                "scale_vaapi",
                format!("w={w}:h={h}:format=nv12:out_range=limited:out_color_matrix=bt470bg"),
            ),
        }
    }
}

fn render_nodes() -> Vec<String> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
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
    nodes.sort();
    nodes
}

/// The kernel driver behind a render node, from sysfs.
fn driver(node: &str) -> Option<String> {
    let name = Path::new(node).file_name()?;
    let link =
        std::fs::read_link(Path::new("/sys/class/drm").join(name).join("device/driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// Probe order: the ring's own render node first, then the others. NVIDIA
/// nodes take the Vulkan route to NVENC; every other driver takes VA-API.
fn candidates(preferred: &str) -> Vec<BackendKind> {
    let mut nodes = render_nodes();
    if let Some(at) = nodes.iter().position(|node| node == preferred) {
        let node = nodes.remove(at);
        nodes.insert(0, node);
    }
    nodes
        .into_iter()
        .take(8)
        .map(|node| match driver(&node).as_deref() {
            Some("nvidia") => BackendKind::Nvenc(node),
            _ => BackendKind::Vaapi(node),
        })
        .collect()
}

fn hwdevice(device_type: ffi::AVHWDeviceType, device: &str) -> Result<Buf> {
    let device = CString::new(device)?;
    let mut context = null_mut();
    // SAFETY: out-pointer is valid; the device string outlives the call.
    checked(unsafe {
        ffi::av_hwdevice_ctx_create(&mut context, device_type, device.as_ptr(), null_mut(), 0)
    })?;
    ensure!(!context.is_null(), "hardware device creation returned null");
    Ok(Buf(context))
}

/// A device of another API on the same GPU as `source`.
fn derived(device_type: ffi::AVHWDeviceType, source: &Buf) -> Result<Buf> {
    let mut context = null_mut();
    // SAFETY: source is a live device reference; the out-pointer is valid.
    checked(unsafe {
        ffi::av_hwdevice_ctx_create_derived(&mut context, device_type, source.0, 0)
    })?;
    ensure!(!context.is_null(), "derived device creation returned null");
    Ok(Buf(context))
}

/// A hardware frames context. Map targets need no preallocated surfaces;
/// encoder inputs keep a few, like the upload path.
fn frames_pool(
    device: &Buf,
    (width, height): (u32, u32),
    format: ffi::AVPixelFormat,
    sw_format: ffi::AVPixelFormat,
    initial: c_int,
) -> Result<Buf> {
    // SAFETY: device is a live reference; the pool is checked, then its
    // context is filled before init.
    unsafe {
        let pool = Buf(ffi::av_hwframe_ctx_alloc(device.0));
        ensure!(!pool.0.is_null(), "cannot allocate hardware frame pool");
        let context = &mut *((*pool.0).data as *mut ffi::AVHWFramesContext);
        context.width = width as c_int;
        context.height = height as c_int;
        context.format = format;
        context.sw_format = sw_format;
        context.initial_pool_size = initial;
        checked(ffi::av_hwframe_ctx_init(pool.0))?;
        Ok(pool)
    }
}

/// buffersrc on the mapped frames, the GPU conversion, then buffersink.
/// Converts XRGB to NV12 and the capture size to the stream size in one GPU
/// step; at equal sizes it still converts.
fn scale_graph(
    kind: &BackendKind,
    import: &Buf,
    capture: (u32, u32),
    stream: (u32, u32),
) -> Result<Graph> {
    // SAFETY: every pointer is checked before use; the graph owns its
    // contexts and frees them on every early return through Drop.
    unsafe {
        let graph = ffi::avfilter_graph_alloc();
        ensure!(!graph.is_null(), "cannot allocate filter graph");
        let mut graph = Graph {
            graph,
            src: null_mut(),
            sink: null_mut(),
        };
        let buffersrc = ffi::avfilter_get_by_name(c"buffer".as_ptr());
        ensure!(!buffersrc.is_null(), "buffer source filter is missing");
        // A hardware source needs its frames context before init, so it is
        // allocated, given its parameters, and only then initialized.
        graph.src = ffi::avfilter_graph_alloc_filter(graph.graph, buffersrc, c"in".as_ptr());
        ensure!(!graph.src.is_null(), "cannot allocate buffer source");
        let parameters = ffi::av_buffersrc_parameters_alloc();
        ensure!(!parameters.is_null(), "cannot allocate source parameters");
        (*parameters).format = kind.map_format() as c_int;
        (*parameters).width = capture.0 as c_int;
        (*parameters).height = capture.1 as c_int;
        (*parameters).time_base = ffi::AVRational {
            num: 1,
            den: 1_000_000,
        };
        (*parameters).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
        (*parameters).color_space = ffi::AVColorSpace::AVCOL_SPC_SMPTE170M;
        // Borrowed: the source takes its own reference.
        (*parameters).hw_frames_ctx = import.0;
        let set = ffi::av_buffersrc_parameters_set(graph.src, parameters);
        ffi::av_free(parameters as *mut std::ffi::c_void);
        checked(set)?;
        checked(ffi::avfilter_init_str(graph.src, ptr::null()))?;
        let (name, args) = kind.scale_filter(stream);
        let scale = ffi::avfilter_get_by_name(CString::new(name)?.as_ptr());
        ensure!(
            !scale.is_null(),
            format!("{name} is not in this FFmpeg build")
        );
        let mut scale_context = null_mut();
        let args = CString::new(args)?;
        checked(ffi::avfilter_graph_create_filter(
            &mut scale_context,
            scale,
            c"scale".as_ptr(),
            args.as_ptr(),
            null_mut(),
            graph.graph,
        ))?;
        let buffersink = ffi::avfilter_get_by_name(c"buffersink".as_ptr());
        ensure!(!buffersink.is_null(), "buffer sink filter is missing");
        checked(ffi::avfilter_graph_create_filter(
            &mut graph.sink,
            buffersink,
            c"out".as_ptr(),
            ptr::null(),
            null_mut(),
            graph.graph,
        ))?;
        checked(ffi::avfilter_link(graph.src, 0, scale_context, 0))?;
        checked(ffi::avfilter_link(scale_context, 0, graph.sink, 0))?;
        checked(ffi::avfilter_graph_config(graph.graph, null_mut()))?;
        Ok(graph)
    }
}

fn nvenc_maxrate(bitrate: u32) -> u32 {
    bitrate.saturating_mul(2)
}

/// Open the hardware encoder with the upload path's settings: same profile,
/// rate control, GOP, and low-delay flags, so the two paths are
/// interchangeable behind the parent's fallback.
fn open_codec(
    kind: &BackendKind,
    config: &Config,
    device: &Buf,
    frames: *mut ffi::AVBufferRef,
) -> Result<Codec> {
    let name = CString::new(kind.encoder())?;
    // SAFETY: checked pointers throughout; the dictionary is freed after the
    // open consumes it, and the codec owns its context references.
    unsafe {
        let codec = ffi::avcodec_find_encoder_by_name(name.as_ptr());
        ensure!(
            !codec.is_null(),
            format!("{} is not in this FFmpeg build", kind.encoder())
        );
        let context = ffi::avcodec_alloc_context3(codec);
        ensure!(!context.is_null(), "cannot allocate encoder context");
        let context = Codec(context);
        (*context.0).width = config.width as c_int;
        (*context.0).height = config.height as c_int;
        (*context.0).pix_fmt = kind.encode_format();
        (*context.0).time_base = ffi::AVRational {
            num: 1,
            den: 1_000_000,
        };
        (*context.0).framerate = ffi::AVRational {
            num: config.fps as c_int,
            den: 1,
        };
        (*context.0).bit_rate = config.bitrate as i64;
        (*context.0).gop_size = config.gop_frames() as c_int;
        (*context.0).max_b_frames = 0;
        (*context.0).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as c_int;
        (*context.0).hw_device_ctx = device.reference()?;
        let frames = ffi::av_buffer_ref(frames);
        ensure!(!frames.is_null(), "cannot reference encoder frame pool");
        (*context.0).hw_frames_ctx = frames;
        // Per-backend options, exactly like the upload path.
        let maxrate = nvenc_maxrate(config.bitrate).to_string();
        let pairs: &[(&str, &str)] = match kind {
            BackendKind::Nvenc(_) => &[
                ("profile", "baseline"),
                ("preset", "p1"),
                ("tune", "ull"),
                ("rc", "vbr"),
                ("maxrate", &maxrate),
                ("rc-lookahead", "0"),
                ("zerolatency", "1"),
                ("delay", "0"),
                ("forced-idr", "1"),
                ("no-scenecut", "1"),
            ],
            BackendKind::Vaapi(_) => &[
                ("profile", "constrained_baseline"),
                ("async_depth", "1"),
                ("idr_interval", "0"),
            ],
        };
        let mut dictionary = null_mut();
        for (key, value) in pairs {
            let key = CString::new(*key)?;
            let value = CString::new(*value)?;
            let set = ffi::av_dict_set(&mut dictionary, key.as_ptr(), value.as_ptr(), 0);
            if set < 0 {
                ffi::av_dict_free(&mut dictionary);
                checked(set)?;
            }
        }
        let opened = ffi::avcodec_open2(context.0, codec, &mut dictionary);
        ffi::av_dict_free(&mut dictionary);
        checked(opened)?;
        Ok(context)
    }
}

struct Backend {
    label: String,
    kind: BackendKind,
    /// Frames context the dmabufs map into: Vulkan or VA-API, BGR0 at
    /// capture size.
    import: Buf,
    graph: Graph,
    /// NVENC only: CUDA frames the converted Vulkan frames transfer into.
    upload: Option<Buf>,
    codec: Codec,
    _devices: Vec<Buf>,
}

impl Backend {
    fn open(kind: &BackendKind, config: &Config, capture: (u32, u32)) -> Result<Self> {
        use ffi::AVHWDeviceType::*;
        use ffi::AVPixelFormat::*;
        let stream = (config.width, config.height);
        let (mut devices, encode_device) = match kind {
            BackendKind::Nvenc(node) => {
                let drm = hwdevice(AV_HWDEVICE_TYPE_DRM, node)?;
                let vulkan = derived(AV_HWDEVICE_TYPE_VULKAN, &drm).context("Vulkan")?;
                let cuda = derived(AV_HWDEVICE_TYPE_CUDA, &vulkan).context("CUDA")?;
                (vec![drm, vulkan], cuda)
            }
            BackendKind::Vaapi(node) => (Vec::new(), hwdevice(AV_HWDEVICE_TYPE_VAAPI, node)?),
        };
        let map_device = devices.last().unwrap_or(&encode_device);
        let import = frames_pool(map_device, capture, kind.map_format(), AV_PIX_FMT_BGR0, 0)?;
        let graph = scale_graph(kind, &import, capture, stream)?;
        let upload = match kind {
            BackendKind::Nvenc(_) => Some(frames_pool(
                &encode_device,
                stream,
                AV_PIX_FMT_CUDA,
                AV_PIX_FMT_BGR0,
                4,
            )?),
            BackendKind::Vaapi(_) => None,
        };
        let frames = match &upload {
            Some(pool) => pool.0,
            // SAFETY: the configured graph's sink; borrowed, and referenced
            // by open_codec before the graph could drop.
            None => unsafe { ffi::av_buffersink_get_hw_frames_ctx(graph.sink) },
        };
        ensure!(!frames.is_null(), "GPU filter produced no hardware frames");
        let codec = open_codec(kind, config, &encode_device, frames)?;
        devices.push(encode_device);
        Ok(Self {
            label: kind.label(),
            kind: kind.clone(),
            import,
            graph,
            upload,
            codec,
            _devices: devices,
        })
    }

    /// Map one dmabuf, convert it on the GPU, and encode it.
    fn encode_one(
        &mut self,
        fd: OwnedFd,
        desc: &FrameDescriptor,
        pts: i64,
        force: bool,
    ) -> Result<Vec<u8>> {
        let drm = drm_frame(fd, desc)?;
        // SAFETY: all frames are allocated above; the pools and graph are
        // live; each call's return is checked before its outputs are used.
        // The mapping holds its own reference to the DRM frame and its fd.
        unsafe {
            let mapped = Frame::alloc()?;
            (*mapped.0).format = self.kind.map_format() as c_int;
            (*mapped.0).hw_frames_ctx = self.import.reference()?;
            checked(ffi::av_hwframe_map(
                mapped.0,
                drm.0,
                ffi::AV_HWFRAME_MAP_READ as c_int,
            ))?;
            drop(drm);
            (*mapped.0).pts = pts;
            // Desktop RGB is full range; the filter's output range and
            // matrix are fixed in `scale_filter`.
            (*mapped.0).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*mapped.0).colorspace = ffi::AVColorSpace::AVCOL_SPC_SMPTE170M;
            checked(ffi::av_buffersrc_add_frame_flags(
                self.graph.src,
                mapped.0,
                0,
            ))?;
            let converted = Frame::alloc()?;
            checked(ffi::av_buffersink_get_frame(self.graph.sink, converted.0))?;
            let input = match &self.upload {
                Some(pool) => {
                    let cuda = Frame::alloc()?;
                    checked(ffi::av_hwframe_get_buffer(pool.0, cuda.0, 0))?;
                    checked(ffi::av_hwframe_transfer_data(cuda.0, converted.0, 0))?;
                    cuda
                }
                None => converted,
            };
            (*input.0).pts = pts;
            (*input.0).pict_type = if force {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            checked(ffi::avcodec_send_frame(self.codec.0, input.0))?;
            let mut packet = ffi::av_packet_alloc();
            ensure!(!packet.is_null(), "cannot allocate packet");
            let received = ffi::avcodec_receive_packet(self.codec.0, packet);
            if received < 0 {
                ffi::av_packet_free(&mut packet);
                checked(received)?;
                unreachable!("a negative packet status always errors");
            }
            let delayed = (*packet).pts != pts;
            let bytes =
                std::slice::from_raw_parts((*packet).data, (*packet).size as usize).to_vec();
            ffi::av_packet_free(&mut packet);
            ensure!(!delayed, "hardware returned a delayed frame");
            ensure!(!bytes.is_empty(), "empty encoded packet");
            Ok(bytes)
        }
    }
}

/// A zero-copy encoder session: one socket to the parent, one winning
/// backend. The first frame probes every backend; later frames reuse the
/// winner. Any error ends the helper so the parent falls back.
pub struct ZeroCopy {
    config: Config,
    ring: GpuRing,
    socket: UnixStream,
    backend: Option<Backend>,
    label: Option<String>,
}

impl ZeroCopy {
    pub fn open(config: &Config, ring: &GpuRing, socket: UnixStream) -> Result<Self> {
        config.frame_len()?;
        ring.validate()?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            config: config.clone(),
            ring: ring.clone(),
            socket,
            backend: None,
            label: None,
        })
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Encode the frame the parent announced on the pipe. The socket hands
    /// over its fd; the two channels must agree, or the session fails closed.
    pub fn encode(&mut self, pts: i64, id: GpuBufferId, force: bool) -> Result<(Vec<u8>, bool)> {
        let (fd, desc) = omabeam_encoder::dmabuf::recv_frame_fd(&self.socket, self.ring.count)
            .context("GPU frame handoff failed")?;
        ensure!(desc.id == id, "pipe and socket disagree on the GPU frame");
        ensure!(
            desc.width == self.ring.width && desc.height == self.ring.height,
            "capture size changed; reopen the zero-copy session"
        );
        let bytes = match &mut self.backend {
            Some(backend) => backend.encode_one(fd, &desc, pts, force)?,
            None => self.probe(fd, &desc, pts, force)?,
        };
        let key = omabeam_encoder::inspect_h264(&bytes)?;
        Ok((bytes, key))
    }

    fn probe(
        &mut self,
        fd: OwnedFd,
        desc: &FrameDescriptor,
        pts: i64,
        force: bool,
    ) -> Result<Vec<u8>> {
        let mut errors = Vec::new();
        for kind in candidates(&self.ring.node) {
            let attempt = (|| -> Result<(Backend, Vec<u8>)> {
                let mut backend =
                    Backend::open(&kind, &self.config, (self.ring.width, self.ring.height))?;
                let bytes = backend.encode_one(fd.try_clone()?, desc, pts, force)?;
                Ok((backend, bytes))
            })();
            match attempt {
                Ok((backend, bytes)) => {
                    self.label = Some(backend.label.clone());
                    self.backend = Some(backend);
                    return Ok(bytes);
                }
                Err(error) => errors.push(format!("{}: {error:#}", kind.label())),
            }
        }
        anyhow::bail!(
            "no zero-copy backend accepted the frame ({})",
            errors.join("; ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omabeam_encoder::dmabuf::{FOURCC_XRGB8888, FrameDescriptor, GpuBufferId, RING_BUFFERS};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    fn config(stream: (u32, u32)) -> Config {
        Config {
            version: omabeam_encoder::VERSION,
            width: stream.0,
            height: stream.1,
            fps: 30,
            bitrate: 2_000_000,
            gop_frames: 0,
            gpu: None,
        }
    }

    fn ring(capture: (u32, u32), node: &str) -> GpuRing {
        GpuRing {
            version: omabeam_encoder::VERSION,
            width: capture.0,
            height: capture.1,
            fourcc: FOURCC_XRGB8888,
            modifier: omabeam_encoder::dmabuf::MOD_LINEAR,
            count: RING_BUFFERS,
            socket: "/tmp/omabeam-test-gpu.sock".into(),
            node: node.into(),
        }
    }

    /// Quadrant colors, top-left, top-right, bottom-left, bottom-right.
    const QUADRANTS: [[u8; 3]; 4] = [[255, 255, 255], [255, 0, 0], [0, 0, 255], [0, 0, 0]];

    /// Paint [`QUADRANTS`] into a dmabuf with a GPU upload through Vulkan, the
    /// way a compositor fills a capture buffer. CPU mappings are no stand-in:
    /// NVIDIA serves `mmap` on the fd, but its writes never reach the GPU.
    fn paint_quadrants(fd: &OwnedFd, desc: &FrameDescriptor, node: &str) -> Result<()> {
        use ffi::AVHWDeviceType::*;
        use ffi::AVPixelFormat::*;
        let (width, height) = (desc.width as usize, desc.height as usize);
        let drm = hwdevice(AV_HWDEVICE_TYPE_DRM, node)?;
        let vulkan = derived(AV_HWDEVICE_TYPE_VULKAN, &drm)?;
        let import = frames_pool(
            &vulkan,
            (desc.width, desc.height),
            AV_PIX_FMT_VULKAN,
            AV_PIX_FMT_BGR0,
            0,
        )?;
        let source = drm_frame(fd.try_clone()?, desc)?;
        // SAFETY: frames are checked before use; the software frame owns
        // `height` rows of `linesize` bytes once allocated.
        unsafe {
            let pixels = Frame::alloc()?;
            (*pixels.0).format = AV_PIX_FMT_BGR0 as c_int;
            (*pixels.0).width = width as c_int;
            (*pixels.0).height = height as c_int;
            checked(ffi::av_frame_get_buffer(pixels.0, 0))?;
            let stride = (*pixels.0).linesize[0] as usize;
            let bytes = std::slice::from_raw_parts_mut((*pixels.0).data[0], stride * height);
            for y in 0..height {
                for x in 0..width {
                    let [r, g, b] = QUADRANTS[2 * y / height * 2 + 2 * x / width];
                    bytes[y * stride + x * 4..][..4].copy_from_slice(&[b, g, r, 0xff]);
                }
            }
            let target = Frame::alloc()?;
            (*target.0).format = AV_PIX_FMT_VULKAN as c_int;
            (*target.0).hw_frames_ctx = import.reference()?;
            checked(ffi::av_hwframe_map(
                target.0,
                source.0,
                (ffi::AV_HWFRAME_MAP_WRITE as u32 | ffi::AV_HWFRAME_MAP_OVERWRITE as u32) as c_int,
            ))?;
            checked(ffi::av_hwframe_transfer_data(target.0, pixels.0, 0))?;
        }
        Ok(())
    }

    /// An XRGB8888 dmabuf painted with [`QUADRANTS`], linear or `tiled`.
    /// Returns the fd, its descriptor, and the render node that allocated
    /// it. The fd pins the memory, so the GBM objects can drop.
    fn quadrant_dmabuf(
        width: u32,
        height: u32,
        tiled: bool,
    ) -> Option<(OwnedFd, FrameDescriptor, String)> {
        for node in render_nodes() {
            let Ok(file) = std::fs::File::open(&node) else {
                continue;
            };
            let Ok(device) = gbm::Device::new(file) else {
                continue;
            };
            let format = drm_fourcc::DrmFourcc::Xrgb8888;
            let allocated = if tiled {
                // NVIDIA's default is a compressed layout Vulkan cannot fill;
                // Hyprland captures into this uncompressed block-linear one.
                const NVIDIA_BLOCK_LINEAR: u64 = 0x0300_0000_0060_6014;
                device
                    .create_buffer_object_with_modifiers2::<()>(
                        width,
                        height,
                        format,
                        [NVIDIA_BLOCK_LINEAR.into()].into_iter(),
                        gbm::BufferObjectFlags::RENDERING,
                    )
                    .or_else(|_| {
                        device.create_buffer_object::<()>(
                            width,
                            height,
                            format,
                            gbm::BufferObjectFlags::RENDERING,
                        )
                    })
            } else {
                device
                    .create_buffer_object_with_modifiers2::<()>(
                        width,
                        height,
                        format,
                        [drm_fourcc::DrmModifier::Linear].into_iter(),
                        gbm::BufferObjectFlags::empty(),
                    )
                    .or_else(|_| {
                        device.create_buffer_object::<()>(
                            width,
                            height,
                            format,
                            gbm::BufferObjectFlags::LINEAR,
                        )
                    })
            };
            let Ok(buffer) = allocated else {
                continue;
            };
            let modifier = u64::from(buffer.modifier());
            if tiled == (modifier == omabeam_encoder::dmabuf::MOD_LINEAR)
                || modifier == omabeam_encoder::dmabuf::MOD_INVALID
            {
                continue;
            }
            let Ok(fd) = buffer.fd() else {
                continue;
            };
            let descriptor = FrameDescriptor {
                id: GpuBufferId {
                    index: 0,
                    generation: 1,
                },
                width,
                height,
                stride: buffer.stride_for_plane(0),
                offset: buffer.offset(0),
                modifier,
                fourcc: FOURCC_XRGB8888,
            };
            if let Err(error) = paint_quadrants(&fd, &descriptor, &node) {
                eprintln!("{node}: cannot paint the {modifier:#x} test dmabuf: {error:#}");
                continue;
            }
            return Some((fd, descriptor, node));
        }
        None
    }

    /// Limited-range BT.601, the matrix the app's CPU converter uses.
    fn bt601([r, g, b]: [u8; 3]) -> [f64; 3] {
        let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
        [
            16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0,
            128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0,
            128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0,
        ]
    }

    /// Every quadrant's center decodes to its painted color. Wrong imports
    /// (an untouched surface, swapped channels, the wrong matrix) all fail.
    fn assert_quadrants(width: usize, height: usize, yuv: &[u8]) {
        let (luma, chroma) = (width * height, width * height / 4);
        for (quadrant, rgb) in QUADRANTS.iter().enumerate() {
            let x = width / 4 + (quadrant % 2) * width / 2;
            let y = height / 4 + (quadrant / 2) * height / 2;
            let c = (y / 2) * (width / 2) + x / 2;
            let actual = [yuv[y * width + x], yuv[luma + c], yuv[luma + chroma + c]];
            let expected = bt601(*rgb);
            for (plane, (actual, expected)) in actual.iter().zip(expected).enumerate() {
                assert!(
                    (f64::from(*actual) - expected).abs() <= 6.0,
                    "quadrant {quadrant} plane {plane}: decoded {actual:?}, expected {expected:.0} \
                     (all planes {actual_all:?})",
                    actual_all = [yuv[y * width + x], yuv[luma + c], yuv[luma + chroma + c]],
                );
            }
        }
    }

    fn open_fds() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    /// Decode a packet stream with the software decoder, returning each
    /// frame's dimensions and packed YUV420P bytes.
    /// Width, height, and the packed I420 planes.
    type Decoded = (u32, u32, Vec<u8>);

    fn decode_all(packets: &[Vec<u8>]) -> Vec<Decoded> {
        use ffmpeg_next::{codec, decoder};
        let found = decoder::find(codec::Id::H264).unwrap();
        let mut decoder = codec::context::Context::new_with_codec(found)
            .decoder()
            .video()
            .unwrap();
        let mut frames = Vec::new();
        let mut frame = ffmpeg_next::frame::Video::empty();
        for packet in packets {
            decoder
                .send_packet(&ffmpeg_next::Packet::copy(packet))
                .unwrap();
            while decoder.receive_frame(&mut frame).is_ok() {
                let (width, height) = (frame.width(), frame.height());
                let mut bytes = Vec::new();
                for plane in 0..3 {
                    let (w, h) = if plane == 0 {
                        (width as usize, height as usize)
                    } else {
                        (width as usize / 2, height as usize / 2)
                    };
                    let stride = frame.stride(plane);
                    for row in frame.data(plane).chunks(stride).take(h) {
                        bytes.extend_from_slice(&row[..w]);
                    }
                }
                frames.push((width, height, bytes));
                frame = ffmpeg_next::frame::Video::empty();
            }
        }
        frames
    }

    /// Runs wherever a zero-copy backend opens and checks nothing where none
    /// does, like the upload hardware tests. `OMABEAM_REQUIRE_ZERO_COPY=1`
    /// turns a missing backend into a failure, for GPU machines.
    fn skip(reason: impl std::fmt::Display) {
        assert!(
            std::env::var_os("OMABEAM_REQUIRE_ZERO_COPY").is_none_or(|v| v != "1"),
            "zero-copy required: {reason}"
        );
        eprintln!("skipped: {reason}");
    }

    /// Encode `frames` copies of the quadrant dmabuf and decode them.
    fn encode_quadrants(
        capture: (u32, u32),
        stream: (u32, u32),
        frames: usize,
        tiled: bool,
        mut each: impl FnMut(usize, bool),
    ) -> Option<(String, Vec<Decoded>)> {
        let Some((fd, descriptor, node)) = quadrant_dmabuf(capture.0, capture.1, tiled) else {
            skip(if tiled {
                "no tiled GBM dmabuf available"
            } else {
                "no linear GBM dmabuf available"
            });
            return None;
        };
        eprintln!("test dmabuf on {node}: modifier {:#x}", descriptor.modifier);
        let (parent, helper) = UnixStream::pair().unwrap();
        let mut session = ZeroCopy::open(&config(stream), &ring(capture, &node), helper).unwrap();
        let mut packets = Vec::new();
        for frame in 0..frames {
            omabeam_encoder::dmabuf::send_frame_fd(&parent, fd.as_fd(), &descriptor).unwrap();
            let (bytes, key) =
                match session.encode(frame as i64 * 33_333, descriptor.id, frame == 0) {
                    Ok(packet) => packet,
                    Err(error) if frame == 0 => {
                        skip(format!("{error:#}"));
                        return None;
                    }
                    Err(error) => panic!("frame {frame} failed: {error:#}"),
                };
            assert!(!bytes.is_empty());
            each(frame, key);
            packets.push(bytes);
        }
        let label = session.label().unwrap().to_owned();
        eprintln!("zero-copy backend: {label}");
        Some((label, decode_all(&packets)))
    }

    /// The decoded stream shows the painted pixels in every frame: import,
    /// GPU conversion, and encode all carry the real buffer contents.
    #[test]
    fn zero_copy_encodes_imported_dmabufs() {
        let _guard = crate::lock_hw_tests();
        ffmpeg_next::init().unwrap();
        let (capture, stream) = ((640, 360), (640, 360));
        let (mut idrs, mut fds_at_40) = (0, 0);
        let Some((label, decoded)) = encode_quadrants(capture, stream, 100, false, |frame, key| {
            idrs += key as u32;
            if frame == 40 {
                fds_at_40 = open_fds();
            }
        }) else {
            return;
        };
        // Frame 0 is forced; frame 60 is the two-second GOP interval.
        assert_eq!(idrs, 2, "forced plus periodic IDRs");
        assert!(!label.is_empty(), "probing backend reports its label");
        assert_eq!(decoded.len(), 100, "every packet decodes to a frame");
        for (width, height, bytes) in &decoded {
            assert_eq!((*width, *height), stream);
            assert_quadrants(stream.0 as usize, stream.1 as usize, bytes);
        }
        // Device setup holds its handles and interop releases late, but the
        // count must stop growing: each frame's fd closes with its mapping.
        let fds = open_fds();
        assert!(
            fds <= fds_at_40,
            "fd leak across frames: {fds_at_40} -> {fds}"
        );
    }

    /// Per-frame cost of import, GPU resize, and encode, as the parent sees
    /// it minus the pipe: the helper's whole share of a zero-copy frame.
    #[test]
    #[ignore = "timing report: cargo test --release -p omabeam-encoder -- --ignored --nocapture"]
    fn report_zero_copy_timings() {
        let _guard = crate::lock_hw_tests();
        ffmpeg_next::init().unwrap();
        for (capture, stream) in [
            ((1920, 1080), (1920, 1080)),
            ((3840, 2160), (3840, 2160)),
            ((3840, 2160), (1920, 1080)),
        ] {
            let Some((fd, descriptor, node)) = quadrant_dmabuf(capture.0, capture.1, false) else {
                skip("no linear GBM dmabuf available");
                return;
            };
            let (parent, helper) = UnixStream::pair().unwrap();
            let mut session =
                ZeroCopy::open(&config(stream), &ring(capture, &node), helper).unwrap();
            let mut each = Vec::new();
            for frame in 0..130 {
                omabeam_encoder::dmabuf::send_frame_fd(&parent, fd.as_fd(), &descriptor).unwrap();
                let started = std::time::Instant::now();
                if let Err(error) = session.encode(frame * 33_333, descriptor.id, frame == 0) {
                    skip(format!("{error:#}"));
                    return;
                }
                if frame >= 10 {
                    each.push(started.elapsed().as_secs_f64() * 1e3);
                }
            }
            each.sort_by(f64::total_cmp);
            println!(
                "{:?} -> {:?} on {}: median {:.2} ms, p95 {:.2} ms",
                capture,
                stream,
                session.label().unwrap(),
                each[each.len() / 2],
                each[each.len() * 95 / 100],
            );
        }
    }

    /// The GPU scaler runs when capture and stream sizes differ.
    #[test]
    fn zero_copy_scales_on_the_gpu() {
        let _guard = crate::lock_hw_tests();
        ffmpeg_next::init().unwrap();
        let (capture, stream) = ((640, 360), (320, 180));
        let Some((_, decoded)) = encode_quadrants(capture, stream, 10, false, |_, _| {}) else {
            return;
        };
        assert_eq!(decoded.len(), 10);
        for (width, height, bytes) in &decoded {
            assert_eq!((*width, *height), stream, "GPU-scaled dimensions");
            assert_quadrants(stream.0 as usize, stream.1 as usize, bytes);
        }
    }

    /// A tiled layout imports, scales, and converts like a linear one.
    #[test]
    fn zero_copy_encodes_tiled_dmabufs() {
        let _guard = crate::lock_hw_tests();
        ffmpeg_next::init().unwrap();
        for (capture, stream) in [((640, 360), (640, 360)), ((640, 360), (320, 180))] {
            let Some((_, decoded)) = encode_quadrants(capture, stream, 10, true, |_, _| {}) else {
                return;
            };
            assert_eq!(decoded.len(), 10);
            for (width, height, bytes) in &decoded {
                assert_eq!((*width, *height), stream);
                assert_quadrants(stream.0 as usize, stream.1 as usize, bytes);
            }
        }
    }

    #[test]
    fn nvidia_nodes_take_the_vulkan_route() {
        let preferred = render_nodes().pop().unwrap_or_default();
        let kinds = candidates(&preferred);
        if let Some(first) = kinds.first() {
            let node = match first {
                BackendKind::Nvenc(node) | BackendKind::Vaapi(node) => node,
            };
            assert_eq!(node, &preferred, "the ring's node probes first");
        }
        for kind in kinds {
            let (node, nvidia) = match &kind {
                BackendKind::Nvenc(node) => (node, true),
                BackendKind::Vaapi(node) => (node, false),
            };
            assert_eq!(driver(node).as_deref() == Some("nvidia"), nvidia, "{node}");
        }
    }
}
