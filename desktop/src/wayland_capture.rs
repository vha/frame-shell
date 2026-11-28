//! Direct Wayland output capture via `ext-image-copy-capture-v1` feeding GStreamer hardware encoders.

mod dmabuf;

use crate::{Output, Shared, capture, lock};
use anyhow::{Context, Result, bail};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::{AppSink, AppSrc};
#[cfg(test)]
use std::sync::Arc;
use std::{
    ffi::CString,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use wayvr_ipc::{frame::DesktopSettings, metrics};
use wlx_capture::libc;
use wlx_capture::wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::WaylandError,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
};
use wlx_capture::wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
        ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

use wlx_capture::wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;

/// Returns true when the compositor advertises `ext_image_copy_capture_manager_v1` and
/// `ext_output_image_capture_source_manager_v1`.
pub fn supports_ext_image_copy_capture() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<ProbeState>(&conn) else {
        return false;
    };
    let list = globals.contents().clone_list();
    let has_capture = list
        .iter()
        .any(|g| g.interface == ExtImageCopyCaptureManagerV1::interface().name);
    let has_source = list
        .iter()
        .any(|g| g.interface == ExtOutputImageCaptureSourceManagerV1::interface().name);
    has_capture && has_source
}

struct ProbeState;

impl Dispatch<WlRegistry, GlobalListContents> for ProbeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameStatus {
    Idle,
    Pending,
    Ready,
    Failed(Option<FailureReason>),
}

struct OutputEntry {
    registry_name: u32,
    proxy: WlOutput,
    name: Option<String>,
}

struct CaptureState {
    target_name: String,
    outputs: Vec<OutputEntry>,
    output_removed: bool,

    pending_size: (u32, u32),
    pending_formats: Vec<wl_shm::Format>,
    pending_dmabuf: dmabuf::Constraints,

    active_size: (u32, u32),
    active_formats: Vec<wl_shm::Format>,
    active_dmabuf: dmabuf::Constraints,
    constraints_gen: u64,
    session_stopped: bool,
    frame_status: FrameStatus,
}

impl CaptureState {
    fn new(target_name: String) -> Self {
        Self {
            target_name,
            outputs: Vec::new(),
            output_removed: false,
            pending_size: (0, 0),
            pending_formats: Vec::new(),
            pending_dmabuf: dmabuf::Constraints::default(),
            active_size: (0, 0),
            active_formats: Vec::new(),
            active_dmabuf: dmabuf::Constraints::default(),
            constraints_gen: 0,
            session_stopped: false,
            frame_status: FrameStatus::Idle,
        }
    }

    fn target_output(&self) -> Option<&WlOutput> {
        self.outputs
            .iter()
            .find(|entry| entry.name.as_deref() == Some(self.target_name.as_str()))
            .map(|entry| &entry.proxy)
    }

    fn reset_session_state(&mut self) {
        self.pending_size = (0, 0);
        self.pending_formats.clear();
        self.pending_dmabuf = dmabuf::Constraints::default();
        self.active_size = (0, 0);
        self.active_formats.clear();
        self.active_dmabuf = dmabuf::Constraints::default();
        self.session_stopped = false;
        self.frame_status = FrameStatus::Idle;
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for CaptureState {
    fn event(
        state: &mut Self,
        proxy: &WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WlOutput::interface().name => {
                let wl_output: WlOutput = proxy.bind(name, version.min(4), qhandle, name);
                state.outputs.push(OutputEntry {
                    registry_name: name,
                    proxy: wl_output,
                    name: None,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(pos) = state.outputs.iter().position(|o| o.registry_name == name) {
                    let removed = state.outputs.remove(pos);
                    if removed.name.as_deref() == Some(state.target_name.as_str()) {
                        state.output_removed = true;
                    }
                    removed.proxy.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for CaptureState {
    fn event(
        state: &mut Self,
        _proxy: &WlOutput,
        event: wl_output::Event,
        registry_name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state
                .outputs
                .iter_mut()
                .find(|entry| entry.registry_name == *registry_name)
        {
            if name == state.target_name {
                state.output_removed = false;
            }
            entry.name = Some(name);
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.pending_size = (width, height);
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(fmt),
            } => {
                state.pending_formats.push(fmt);
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                state.pending_dmabuf.device =
                    device.try_into().ok().map(libc::dev_t::from_ne_bytes);
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                let modifiers = modifiers
                    .chunks_exact(8)
                    .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()))
                    .collect();
                state.pending_dmabuf.formats.push((format, modifiers));
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.active_size = state.pending_size;
                state.active_formats = std::mem::take(&mut state.pending_formats);
                state.active_dmabuf = std::mem::take(&mut state.pending_dmabuf);
                state.constraints_gen = state.constraints_gen.wrapping_add(1);
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.session_stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_status = FrameStatus::Ready;
                proxy.destroy();
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.frame_status = FrameStatus::Failed(reason.into_result().ok());
                proxy.destroy();
            }
            _ => {}
        }
    }
}

macro_rules! noop_dispatch {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Dispatch<$ty, ()> for CaptureState {
                fn event(
                    _state: &mut Self,
                    _proxy: &$ty,
                    _event: <$ty as Proxy>::Event,
                    _data: &(),
                    _conn: &Connection,
                    _qhandle: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

noop_dispatch!(
    ZwpLinuxDmabufV1,
    WlShm,
    WlShmPool,
    WlBuffer,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1,
    ExtImageCopyCaptureManagerV1,
);

struct ShmFrameBuffer {
    width: u32,
    height: u32,
    gst_format: &'static str,
    wl_buffer: WlBuffer,
    wl_pool: WlShmPool,
    ptr: NonNull<u8>,
    byte_len: usize,
    latest: Option<gst::Buffer>,
}

impl ShmFrameBuffer {
    fn pick_format(formats: &[wl_shm::Format]) -> Option<(wl_shm::Format, &'static str)> {
        const PREFERENCE: &[(wl_shm::Format, &str)] = &[
            (wl_shm::Format::Xrgb8888, "BGRx"),
            (wl_shm::Format::Argb8888, "BGRA"),
            (wl_shm::Format::Xbgr8888, "RGBx"),
            (wl_shm::Format::Abgr8888, "RGBA"),
        ];
        PREFERENCE
            .iter()
            .find(|(fmt, _)| formats.contains(fmt))
            .copied()
    }

    fn allocate(
        wl_shm: &WlShm,
        qh: &QueueHandle<CaptureState>,
        width: u32,
        height: u32,
        formats: &[wl_shm::Format],
    ) -> Result<Self> {
        let (format, gst_format) = Self::pick_format(formats)
            .with_context(|| format!("No supported 32-bit wl_shm format in {formats:?}"))?;
        let stride = width
            .checked_mul(4)
            .context("Frame width overflows stride")?;
        let byte_len = usize::try_from(stride)
            .ok()
            .and_then(|s| s.checked_mul(height as usize))
            .filter(|&n| n > 0 && n <= i32::MAX as usize)
            .context("Invalid SHM buffer dimensions")?;

        let fd = create_shm_fd(byte_len)?;
        let raw_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                byte_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if raw_ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap SHM capture buffer");
        }
        let ptr = NonNull::new(raw_ptr.cast::<u8>()).context("mmap returned null")?;
        let wl_pool = wl_shm.create_pool(fd.as_fd(), byte_len as i32, qh, ());
        let wl_buffer = wl_pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        Ok(Self {
            width,
            height,
            gst_format,
            wl_buffer,
            wl_pool,
            ptr,
            byte_len,
            latest: None,
        })
    }

    fn complete(&mut self) -> Result<()> {
        let bytes = unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.byte_len) };
        let mut buffer = gst::Buffer::with_size(bytes.len())?;
        buffer
            .get_mut()
            .expect("new GStreamer buffer")
            .map_writable()?
            .as_mut_slice()
            .copy_from_slice(bytes);
        self.latest = Some(buffer);
        Ok(())
    }

    fn caps(&self) -> gst::Caps {
        gst::Caps::builder("video/x-raw")
            .field("format", self.gst_format)
            .field("width", self.width as i32)
            .field("height", self.height as i32)
            .field("framerate", gst::Fraction::new(60, 1))
            .build()
    }
}

impl Drop for ShmFrameBuffer {
    fn drop(&mut self) {
        self.wl_buffer.destroy();
        self.wl_pool.destroy();
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.byte_len);
        }
    }
}

fn create_shm_fd(byte_len: usize) -> Result<OwnedFd> {
    let raw_fd = unsafe {
        let fd = libc::memfd_create(c"frame-virtual-shm".as_ptr(), libc::MFD_CLOEXEC);
        if fd >= 0 {
            fd
        } else {
            let name = CString::new(format!("/frame-shm-{}", std::process::id()))?;
            let shm_fd = libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                libc::S_IRUSR | libc::S_IWUSR,
            );
            let _ = libc::shm_unlink(name.as_ptr());
            if shm_fd < 0 {
                return Err(std::io::Error::last_os_error()).context("memfd_create / shm_open");
            }
            shm_fd
        }
    };
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), byte_len as libc::off_t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("ftruncate SHM capture buffer");
    }
    Ok(fd)
}

enum CaptureBuffers {
    Dmabuf(dmabuf::Pool),
    Shm(ShmFrameBuffer),
}

impl CaptureBuffers {
    fn is_dmabuf(&self) -> bool {
        matches!(self, Self::Dmabuf(_))
    }

    fn caps(&self) -> gst::Caps {
        match self {
            Self::Dmabuf(pool) => pool.caps(),
            Self::Shm(buffer) => buffer.caps(),
        }
    }

    fn available(&self) -> Option<(usize, &WlBuffer)> {
        match self {
            Self::Dmabuf(pool) => pool.available().map(|slot| (slot, pool.wl_buffer(slot))),
            Self::Shm(buffer) => Some((0, &buffer.wl_buffer)),
        }
    }

    fn complete(&mut self, slot: usize) -> Result<()> {
        match self {
            Self::Dmabuf(pool) => pool.complete(slot),
            Self::Shm(buffer) => buffer.complete()?,
        }
        Ok(())
    }

    fn latest(&self) -> Result<Option<gst::Buffer>> {
        match self {
            Self::Dmabuf(pool) => pool.latest(),
            Self::Shm(buffer) => Ok(buffer.latest.clone()),
        }
    }
}

struct EncoderPipeline {
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
    appsink: AppSink,
    hevc: bool,
    applied_bitrate: u32,
    width: u32,
    height: u32,
    started: Instant,
    seq: u64,
}

impl EncoderPipeline {
    fn new(
        width: u32,
        height: u32,
        buffers: &CaptureBuffers,
        settings: DesktopSettings,
    ) -> Result<Self> {
        let bitrate = capture::scaled_bitrate(settings, width, height);
        // Wayland capture is already paced. Feed static frames straight to
        // the encoder so a single frame can be sent on resume or keyframe request.
        let (_codec, tail) = capture::encoder_tail(settings.hevc, bitrate, buffers.is_dmabuf())?;
        let pipeline_desc = format!(
            "appsrc name=source is-live=true format=time do-timestamp=true block=false max-buffers=2 max-bytes=0 leaky-type=downstream ! {tail} ! appsink name=encoded sync=false max-buffers=2 drop=false"
        );
        let pipeline = gst::parse::launch(&pipeline_desc)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("Invalid virtual panel capture pipeline"))?;
        pipeline.use_clock(None::<&gst::Clock>);
        capture::install_live_latency_probe(&pipeline);

        let appsrc = pipeline
            .by_name("source")
            .context("Missing appsrc")?
            .downcast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("Invalid appsrc element"))?;
        appsrc.set_caps(Some(&buffers.caps()));
        let appsink = pipeline
            .by_name("encoded")
            .context("Missing appsink")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("Invalid appsink element"))?;

        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            appsrc,
            appsink,
            hevc: settings.hevc,
            applied_bitrate: bitrate,
            width,
            height,
            started: Instant::now(),
            seq: 0,
        })
    }

    fn push_latest(&mut self, output_name: &str, buffers: &CaptureBuffers) -> Result<bool> {
        let Some(mut buffer) = buffers.latest()? else {
            return Ok(false);
        };
        let now = gst::ClockTime::from_nseconds(self.started.elapsed().as_nanos() as u64);
        let buf_ref = buffer.make_mut();
        buf_ref.set_pts(now);
        buf_ref.set_dts(now);
        self.seq = self.seq.wrapping_add(1);
        buf_ref.set_offset(self.seq);
        metrics::emit(output_name, "captured", self.seq, buffer.size() as u64);
        self.appsrc
            .push_buffer(buffer)
            .map_err(|err| anyhow::anyhow!("appsrc push_buffer failed: {err:?}"))?;
        Ok(true)
    }

    fn update_bitrate(&mut self, id: u32, settings: DesktopSettings) -> Result<()> {
        let desired = capture::scaled_bitrate(settings, self.width, self.height);
        if desired != self.applied_bitrate {
            self.pipeline
                .by_name("encoder")
                .context("Missing encoder")?
                .set_property("bitrate", desired);
            self.applied_bitrate = desired;
            eprintln!(
                "Virtual surface {id} ({}x{}): {} Mbit/s",
                self.width,
                self.height,
                desired / 1000
            );
        }
        Ok(())
    }
}

impl Drop for EncoderPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn poll_wayland(
    conn: &Connection,
    queue: &mut EventQueue<CaptureState>,
    state: &mut CaptureState,
    timeout_ms: i32,
) -> Result<()> {
    let _ = conn.flush();
    if queue.dispatch_pending(state)? > 0 {
        return Ok(());
    }
    if let Some(guard) = queue.prepare_read() {
        let mut pfd = libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 {
            match guard.read() {
                Ok(_) => {
                    queue.dispatch_pending(state)?;
                }
                Err(WaylandError::Io(ref err)) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err.into()),
            }
        } else {
            drop(guard);
        }
    } else {
        queue.dispatch_pending(state)?;
    }
    Ok(())
}

/// Captures `output_name` over `ext-image-copy-capture-v1` and encodes into `Output::video` until
/// `cancel`, `output.stopped()`, or the compositor removes `output_name`.
pub fn stream_virtual_output(
    id: u32,
    output_name: &str,
    cancel: &AtomicBool,
    output: &Output,
    shared: &Shared,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("Connect to Wayland for virtual capture")?;
    let (globals, mut queue) = registry_queue_init::<CaptureState>(&conn)?;
    let qh = queue.handle();

    let wl_shm: WlShm = globals.bind(&qh, 1..=1, ()).context("Bind wl_shm")?;
    let dmabuf = globals.bind::<ZwpLinuxDmabufV1, _, _>(&qh, 4..=5, ()).ok();
    let source_mgr: ExtOutputImageCaptureSourceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("Bind ext_output_image_capture_source_manager_v1")?;
    let capture_mgr: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("Bind ext_image_copy_capture_manager_v1")?;

    let mut state = CaptureState::new(output_name.to_owned());
    for g in globals.contents().clone_list() {
        if g.interface == WlOutput::interface().name {
            let wl_output: WlOutput =
                globals
                    .registry()
                    .bind(g.name, g.version.min(4), &qh, g.name);
            state.outputs.push(OutputEntry {
                registry_name: g.name,
                proxy: wl_output,
                name: None,
            });
        }
    }
    queue.roundtrip(&mut state)?;

    let wait_deadline = Instant::now() + Duration::from_secs(3);
    while state.target_output().is_none() {
        if cancel.load(Ordering::Acquire) || output.stopped() {
            return Ok(());
        }
        if Instant::now() >= wait_deadline {
            bail!("Timed out waiting for Wayland output {output_name}");
        }
        poll_wayland(&conn, &mut queue, &mut state, 25)?;
    }

    let mut try_dmabuf = dmabuf.is_some();
    let mut playing = lock(&shared.visibility).get(&id).copied().unwrap_or(true);

    'session_loop: while !cancel.load(Ordering::Acquire)
        && !output.stopped()
        && !state.output_removed
    {
        let Some(wl_output) = state.target_output().cloned() else {
            break;
        };
        state.reset_session_state();
        let source = source_mgr.create_source(&wl_output, &qh, ());
        let session = capture_mgr.create_session(&source, Options::PaintCursors, &qh, ());
        let _ = conn.flush();

        let mut active_gen = state.constraints_gen;
        let mut buffers: Option<CaptureBuffers> = None;
        let mut encoder: Option<EncoderPipeline> = None;
        let mut announced: Option<(u32, u32)> = None;
        let mut in_flight_frame = None::<(ExtImageCopyCaptureFrameV1, usize)>;
        let mut next_capture_at = Instant::now();
        let mut last_push_at = next_capture_at - Duration::from_secs(1);
        let mut first_push_at = None;
        let frame_interval = Duration::from_nanos(1_000_000_000 / 60);

        while !cancel.load(Ordering::Acquire)
            && !output.stopped()
            && !state.output_removed
            && !state.session_stopped
        {
            let poll_timeout = if !playing {
                50
            } else if in_flight_frame.is_some() {
                5
            } else {
                let until_capture = next_capture_at.saturating_duration_since(Instant::now());
                (until_capture.as_micros().div_ceil(1000) as i32).clamp(1, 5)
            };
            poll_wayland(&conn, &mut queue, &mut state, poll_timeout)?;
            if state.session_stopped || state.output_removed {
                break;
            }

            let (width, height) = state.active_size;
            if width == 0 || height == 0 {
                continue;
            }
            let settings = shared.settings();

            if state.constraints_gen != active_gen || buffers.is_none() {
                if let Some((frame, _)) = in_flight_frame.take()
                    && state.frame_status == FrameStatus::Pending
                {
                    frame.destroy();
                }
                state.frame_status = FrameStatus::Idle;
                drop(encoder.take());
                active_gen = state.constraints_gen;
                let mut new_buffers = None;
                if try_dmabuf && let Some(dmabuf) = &dmabuf {
                    match dmabuf::Pool::new(dmabuf, &conn, &mut queue, &mut state) {
                        Ok(pool) => {
                            new_buffers = Some(CaptureBuffers::Dmabuf(pool));
                        }
                        Err(_) if state.session_stopped || state.output_removed => break,
                        Err(_) if state.constraints_gen != active_gen => continue,
                        Err(err) => {
                            eprintln!("Virtual surface {id} ({output_name}): using SHM: {err:#}");
                            try_dmabuf = false;
                        }
                    }
                }
                buffers = Some(match new_buffers {
                    Some(buffers) => buffers,
                    None => CaptureBuffers::Shm(ShmFrameBuffer::allocate(
                        &wl_shm,
                        &qh,
                        width,
                        height,
                        &state.active_formats,
                    )?),
                });
            }

            let buffers = buffers.as_mut().expect("allocated capture buffers");
            let rebuild_encoder = encoder.as_ref().is_none_or(|enc| enc.hevc != settings.hevc);
            if rebuild_encoder {
                drop(encoder.take());
                match EncoderPipeline::new(width, height, buffers, settings) {
                    Ok(new_encoder) => encoder = Some(new_encoder),
                    Err(err) if buffers.is_dmabuf() => {
                        eprintln!("Virtual surface {id} ({output_name}): using SHM: {err:#}");
                        try_dmabuf = false;
                        break;
                    }
                    Err(err) => return Err(err),
                }
                announced = None;
                first_push_at = None;
                if buffers.is_dmabuf() {
                    eprintln!("Virtual surface {id} ({output_name}): capturing via DMA-BUF");
                }
            }
            let enc = encoder.as_mut().expect("started capture encoder");
            enc.update_bitrate(id, settings)?;

            let visible = lock(&shared.visibility).get(&id).copied().unwrap_or(true);
            let resumed = visible && !playing;
            if visible != playing {
                playing = visible;
                if playing {
                    next_capture_at = Instant::now();
                    while enc.appsink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
                    metrics::event(output_name, "encoding");
                    eprintln!("Virtual surface {id} ({output_name}): streaming");
                } else {
                    first_push_at = None;
                    metrics::event(output_name, "encoding stopped");
                    eprintln!("Virtual surface {id} ({output_name}): hidden; encoding stopped");
                }
            }

            let mut fresh_frame = false;
            match state.frame_status {
                FrameStatus::Ready => {
                    let (_, slot) = in_flight_frame
                        .take()
                        .context("Missing finished capture slot")?;
                    state.frame_status = FrameStatus::Idle;
                    buffers.complete(slot)?;
                    fresh_frame = true;
                }
                FrameStatus::Failed(reason) => {
                    in_flight_frame = None;
                    state.frame_status = FrameStatus::Idle;
                    if reason == Some(FailureReason::Stopped) {
                        break;
                    }
                    if buffers.is_dmabuf() {
                        eprintln!(
                            "Virtual surface {id} ({output_name}): using SHM after DMA-BUF capture failure: {reason:?}"
                        );
                        try_dmabuf = false;
                        break;
                    }
                    if reason == Some(FailureReason::BufferConstraints) {
                        break;
                    }
                }
                FrameStatus::Pending | FrameStatus::Idle => {}
            }

            let keyframe_requested = playing && lock(&shared.keyframes).remove(&id);
            if playing && (resumed || keyframe_requested) {
                metrics::event(output_name, "keyframe requested");
                capture::force_keyframe(&enc.pipeline);
            }
            // Resend the latest frame on resume or if the encoder is waiting for
            // input on a static desktop. A fresh frame also serves either request.
            if playing
                && (fresh_frame
                    || resumed
                    || keyframe_requested
                    || rebuild_encoder
                    || (announced.is_none()
                        && last_push_at.elapsed() >= Duration::from_millis(100)))
            {
                match enc.push_latest(output_name, buffers) {
                    Ok(true) => {
                        last_push_at = Instant::now();
                        first_push_at.get_or_insert(last_push_at);
                    }
                    Ok(false) => {}
                    Err(err) if buffers.is_dmabuf() => {
                        eprintln!("Virtual surface {id} ({output_name}): using SHM: {err:#}");
                        try_dmabuf = false;
                        break;
                    }
                    Err(err) => return Err(err),
                }
            }

            if playing && in_flight_frame.is_none() && Instant::now() >= next_capture_at {
                if let Some((slot, wl_buffer)) = buffers.available() {
                    let frame = session.create_frame(&qh, ());
                    frame.attach_buffer(wl_buffer);
                    frame.damage_buffer(0, 0, width as i32, height as i32);
                    frame.capture();
                    let _ = conn.flush();
                    in_flight_frame = Some((frame, slot));
                    state.frame_status = FrameStatus::Pending;
                    next_capture_at = (next_capture_at + frame_interval).max(Instant::now());
                } else if next_capture_at.elapsed() >= Duration::from_secs(2) {
                    eprintln!(
                        "Virtual surface {id} ({output_name}): using SHM after DMA-BUF slots stopped recycling"
                    );
                    try_dmabuf = false;
                    break;
                }
            }

            while let Some(sample) = enc.appsink.try_pull_sample(gst::ClockTime::ZERO) {
                if !playing {
                    continue;
                }
                let caps = sample
                    .caps()
                    .context("Encoded virtual frame without caps")?;
                let structure = caps.structure(0).context("Empty virtual caps")?;
                let size = (
                    u32::try_from(structure.get::<i32>("width")?)?,
                    u32::try_from(structure.get::<i32>("height")?)?,
                );
                if announced != Some(size) {
                    let region = shared
                        .region_for_output(output_name)
                        .unwrap_or([0.0, 0.0, 1.0, 1.0]);
                    output.surface(id, size, region, output_name, settings.hevc);
                    shared.sync_surface_region(output, id);
                    announced = Some(size);
                }
                let buffer = sample.buffer().context("Empty virtual encoded buffer")?;
                let map = buffer.map_readable()?;
                let bytes = map.len() as u64;
                let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                if keyframe {
                    metrics::emit(output_name, "keyframe", buffer.offset(), bytes);
                }
                metrics::emit(output_name, "encoded", buffer.offset(), bytes);
                if map.len() > 4 * 1024 * 1024 {
                    bail!("Virtual panel frame exceeds 4 MiB");
                }
                output.video(id, &map);
                if keyframe {
                    lock(&shared.virtual_keyframe_sizes).insert(id, size);
                }
                metrics::emit(output_name, "sent", buffer.offset(), bytes);
            }

            if let Some(bus) = enc.pipeline.bus()
                && let Some(msg) = bus.pop_filtered(&[
                    gst::MessageType::Error,
                    gst::MessageType::Eos,
                    gst::MessageType::Warning,
                ])
            {
                if msg.type_() != gst::MessageType::Warning {
                    if buffers.is_dmabuf() {
                        eprintln!("Virtual surface {id} ({output_name}): using SHM: {msg:?}");
                        try_dmabuf = false;
                        break;
                    }
                    bail!("Virtual capture GStreamer error: {msg:?}");
                }
                metrics::event(output_name, &format!("{msg:?}"));
            }

            if buffers.is_dmabuf()
                && announced.is_none()
                && first_push_at.is_some_and(|first| first.elapsed() >= Duration::from_secs(2))
            {
                eprintln!(
                    "Virtual surface {id} ({output_name}): using SHM after DMA-BUF encoder timeout"
                );
                try_dmabuf = false;
                break;
            }
        }

        drop(encoder);
        if let Some((frame, _)) = in_flight_frame
            && state.frame_status == FrameStatus::Pending
        {
            frame.destroy();
        }
        drop(buffers);
        session.destroy();
        source.destroy();
        let _ = conn.flush();

        if state.output_removed || cancel.load(Ordering::Acquire) || output.stopped() {
            break 'session_loop;
        }
        std::thread::sleep(Duration::from_millis(50));
        let _ = queue.roundtrip(&mut state);
        if state.target_output().is_none() {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::wlr_output::{OutputConfigUpdate, apply_output_layout};
    use std::ffi::c_void;
    use std::process::Command;
    use std::sync::Mutex;

    #[ignore = "requires a running Hyprland desktop session and GStreamer hardware encoder"]
    #[test]
    fn live_hyprland_ext_image_copy_capture() {
        struct HeadlessGuard(String);
        impl Drop for HeadlessGuard {
            fn drop(&mut self) {
                let _ = Command::new("hyprctl")
                    .args(["output", "remove", &self.0])
                    .output();
            }
        }

        #[derive(Default)]
        struct CaptureEvents {
            surfaces: Vec<(u32, u32, u32)>,
            video_frames: usize,
            total_bytes: usize,
        }

        extern "C" fn on_surface(user: *mut c_void, surface: *const frame_abi::frame_host_surface) {
            if user.is_null() || surface.is_null() {
                return;
            }
            let log = unsafe { &*(user as *const Mutex<CaptureEvents>) };
            let surface = unsafe { &*surface };
            log.lock()
                .unwrap()
                .surfaces
                .push((surface.id, surface.width, surface.height));
        }

        extern "C" fn on_video(user: *mut c_void, _surface: u32, data: *const u8, size: usize) {
            if user.is_null() || data.is_null() {
                return;
            }
            let log = unsafe { &*(user as *const Mutex<CaptureEvents>) };
            let mut guard = log.lock().unwrap();
            guard.video_frames += 1;
            guard.total_bytes += size;
        }

        extern "C" fn on_msg(_user: *mut c_void, _data: *const u8, _size: usize) {}
        extern "C" fn on_removed(_user: *mut c_void, _id: u32) {}
        extern "C" fn on_region(_user: *mut c_void, _id: u32, _region: *const f32) {}

        assert!(supports_ext_image_copy_capture());
        gstreamer::init().expect("gstreamer init");

        // A preconfigured output can be used with an isolated Wayland server
        // or with an animation window already running on it.
        let mut reposition = None;
        let mut guard = None;
        let output_name = std::env::var("FRAME_TEST_CAPTURE_OUTPUT").unwrap_or_else(|_| {
            let name = format!("FRAME-CAPTURE-TEST-{}", std::process::id());
            let backend = crate::compositor::hyprland::HyprlandBackend::from_env()
                .expect("connect to Hyprland IPC");
            let monitors = crate::compositor::CompositorBackend::list_monitors(&backend)
                .expect("list monitors");
            let top = monitors.iter().map(|m| m.logical_pos.1).min().unwrap_or(0);
            let mut updates: Vec<_> = monitors
                .iter()
                .map(|m| OutputConfigUpdate {
                    name: m.name.clone(),
                    mode: None,
                    pos: Some(m.logical_pos),
                    scale: Some(m.scale),
                })
                .collect();
            crate::compositor::CompositorBackend::create_headless_output(&backend, &name)
                .expect("create test output");
            guard = Some(HeadlessGuard(name.clone()));
            updates.push(OutputConfigUpdate {
                name: name.clone(),
                mode: Some((1920, 1080, 60)),
                pos: Some((0, top - 1080)),
                scale: Some(1.0),
            });
            apply_output_layout(&updates).expect("configure test output");
            updates.last_mut().unwrap().pos = Some((1920, top - 1080));
            reposition = Some(updates);
            name
        });

        let events: Box<Mutex<CaptureEvents>> = Box::default();
        let user_ptr = Box::into_raw(events);
        let callbacks = frame_abi::frame_host_callbacks {
            user: user_ptr.cast(),
            surface: Some(on_surface),
            surface_region: Some(on_region),
            surface_removed: Some(on_removed),
            video: Some(on_video),
            message: Some(on_msg),
        };
        let output = Output::from_callbacks(callbacks);
        let shared = Arc::new(Shared::default());
        let cancel = Arc::new(AtomicBool::new(false));

        let output_thread = output.clone();
        let shared_thread = shared.clone();
        let cancel_thread = cancel.clone();
        let capture_name = output_name.clone();
        let handle = std::thread::spawn(move || {
            stream_virtual_output(
                wayvr_ipc::frame::VIRTUAL_SURFACE_BASE,
                &capture_name,
                &cancel_thread,
                &output_thread,
                &shared_thread,
            )
        });

        let events_ref = unsafe { &*user_ptr };
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut got_frame = false;
        while Instant::now() < deadline {
            let guard = events_ref.lock().unwrap();
            if !guard.surfaces.is_empty() && guard.video_frames > 0 && guard.total_bytes > 64 {
                got_frame = true;
                println!(
                    "ext-image-copy-capture announced {:?}, frames={}, total_bytes={}",
                    guard.surfaces, guard.video_frames, guard.total_bytes
                );
                break;
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(50));
        }

        if let Some(updates) = reposition {
            apply_output_layout(&updates).expect("reposition test output while capturing");
        }
        let surface = wayvr_ipc::frame::VIRTUAL_SURFACE_BASE;
        let frames_before = events_ref.lock().unwrap().video_frames;
        std::thread::sleep(Duration::from_secs(1));
        let frames_after = events_ref.lock().unwrap().video_frames;
        println!("Visible frames over 1s: {}", frames_after - frames_before);

        lock(&shared.visibility).insert(surface, false);
        std::thread::sleep(Duration::from_millis(200));
        let hidden_frames = events_ref.lock().unwrap().video_frames;
        std::thread::sleep(Duration::from_millis(250));
        let stayed_paused = hidden_frames == events_ref.lock().unwrap().video_frames;
        lock(&shared.visibility).insert(surface, true);
        let deadline = Instant::now() + Duration::from_secs(2);
        while events_ref.lock().unwrap().video_frames == hidden_frames && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let resumed = events_ref.lock().unwrap().video_frames > hidden_frames;

        cancel.store(true, Ordering::Release);
        let res = handle.join().expect("join capture thread");
        assert!(res.is_ok(), "capture returned error: {res:?}");
        assert!(
            got_frame,
            "expected encoded frame from ext-image-copy-capture"
        );
        drop(output);
        unsafe {
            drop(Box::from_raw(user_ptr));
        }
        assert!(stayed_paused, "hidden output continued streaming");
        assert!(resumed, "no frame sent on resume");
    }
}
