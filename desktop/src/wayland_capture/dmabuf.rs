//! GBM-backed Wayland capture buffers imported by the VA postprocessor.

use super::{CaptureState, poll_wayland};
use anyhow::{Context, Result, ensure};
use gstreamer as gst;
use gstreamer_allocators::{DmaBufAllocator, prelude::*};
use gstreamer_video::{VideoFormat, VideoFrameFlags, VideoMeta};
use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
    },
    time::{Duration, Instant},
};
use wlx_capture::{
    libc,
    wayland_client::{
        Connection, Dispatch, EventQueue, QueueHandle, protocol::wl_buffer::WlBuffer,
    },
    wayland_protocols::wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
        zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    },
};

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: libc::c_int) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
    fn gbm_bo_create_with_modifiers2(
        device: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: libc::c_uint,
        flags: u32,
    ) -> *mut c_void;
    fn gbm_bo_destroy(bo: *mut c_void);
    fn gbm_bo_get_plane_count(bo: *mut c_void) -> libc::c_int;
    fn gbm_bo_get_fd(bo: *mut c_void) -> libc::c_int;
    fn gbm_bo_get_modifier(bo: *mut c_void) -> u64;
    fn gbm_bo_get_offset(bo: *mut c_void, plane: libc::c_int) -> u32;
    fn gbm_bo_get_stride_for_plane(bo: *mut c_void, plane: libc::c_int) -> u32;
}

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const POOL_SIZE: usize = 4;

#[derive(Default)]
pub(super) struct Constraints {
    pub device: Option<libc::dev_t>,
    pub formats: Vec<(u32, Vec<u64>)>,
}

type ImportResult = Arc<Mutex<Option<Result<WlBuffer, ()>>>>;

impl Dispatch<ZwpLinuxBufferParamsV1, ImportResult> for CaptureState {
    fn event(
        _: &mut Self,
        _params: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        result: &ImportResult,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        *crate::lock(result) = match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => Some(Ok(buffer)),
            zwp_linux_buffer_params_v1::Event::Failed => Some(Err(())),
            _ => return,
        };
    }

    wlx_capture::wayland_client::event_created_child!(CaptureState, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ())
    ]);
}

struct GbmDevice {
    device: NonNull<c_void>,
    _fd: File,
}

impl GbmDevice {
    fn open(device: libc::dev_t) -> Result<Self> {
        // The compositor can advertise a primary or render node. Resolve both to the
        // same GPU's render node without opening a modesetting device.
        let device_path = format!(
            "/sys/dev/char/{}:{}/device/drm",
            libc::major(device),
            libc::minor(device)
        );
        let mut nodes = fs::read_dir(&device_path)
            .with_context(|| format!("Find DRM render node under {device_path}"))?;
        let node = nodes
            .find_map(|entry| {
                let name = entry.ok()?.file_name();
                name.to_str()?.starts_with("renderD").then_some(name)
            })
            .context("No render node for the compositor's DMA-BUF device")?;
        let path = std::path::Path::new("/dev/dri").join(node);
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Open {}", path.display()))?;
        let gbm = NonNull::new(unsafe { gbm_create_device(fd.as_raw_fd()) })
            .context("Create GBM device")?;
        Ok(Self {
            device: gbm,
            _fd: fd,
        })
    }
}

impl Drop for GbmDevice {
    fn drop(&mut self) {
        unsafe { gbm_device_destroy(self.device.as_ptr()) };
    }
}

struct GbmBuffer(NonNull<c_void>);

impl Drop for GbmBuffer {
    fn drop(&mut self) {
        unsafe { gbm_bo_destroy(self.0.as_ptr()) };
    }
}

#[derive(Clone, Copy)]
struct Format {
    fourcc: [u8; 4],
    video: VideoFormat,
    modifier: u64,
}

impl Format {
    fn caps(self, (width, height): (u32, u32)) -> gst::Caps {
        let fourcc = std::str::from_utf8(&self.fourcc).expect("ASCII DRM fourcc");
        let drm_format = if self.modifier == 0 {
            fourcc.to_owned()
        } else {
            format!("{fourcc}:0x{:016x}", self.modifier)
        };
        gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("format", "DMA_DRM")
            .field("drm-format", drm_format)
            .field("width", width as i32)
            .field("height", height as i32)
            .field("framerate", gst::Fraction::new(60, 1))
            .build()
    }
}

struct Slot {
    wl_buffer: WlBuffer,
    memory: gst::Memory,
    fd: libc::c_int, // Owned by memory.
    offset: usize,
    stride: i32,
}

impl Slot {
    fn allocate(
        device: &GbmDevice,
        format: Format,
        (width, height): (u32, u32),
        allocator: &DmaBufAllocator,
        params: &ZwpLinuxBufferParamsV1,
    ) -> Result<(gst::Memory, libc::c_int, usize, i32)> {
        let bo = unsafe {
            gbm_bo_create_with_modifiers2(
                device.device.as_ptr(),
                width,
                height,
                u32::from_le_bytes(format.fourcc),
                &format.modifier,
                1,
                GBM_BO_USE_RENDERING,
            )
        };
        let bo = GbmBuffer(NonNull::new(bo).context("Allocate GBM buffer")?);
        let raw = bo.0.as_ptr();
        ensure!(
            unsafe { gbm_bo_get_plane_count(raw) } == 1,
            "GBM RGB buffer has auxiliary planes"
        );
        ensure!(
            unsafe { gbm_bo_get_modifier(raw) } == format.modifier,
            "GBM chose a different modifier"
        );
        let stride = unsafe { gbm_bo_get_stride_for_plane(raw, 0) };
        let offset = unsafe { gbm_bo_get_offset(raw, 0) };
        let fd = unsafe { gbm_bo_get_fd(raw) };
        ensure!(fd >= 0, "Export GBM DMA-BUF");
        let raw_fd = fd;
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        ensure!(size > i64::from(offset), "Get DMA-BUF allocation size");
        let stride_i32 = i32::try_from(stride)?;
        params.add(
            fd.as_fd(),
            0,
            offset,
            stride,
            (format.modifier >> 32) as u32,
            format.modifier as u32,
        );
        // The exported fd keeps the allocation alive independently of the GBM object.
        let memory = unsafe { allocator.alloc_dmabuf(fd, usize::try_from(size)?) }?;
        Ok((memory, raw_fd, offset as usize, stride_i32))
    }

    fn available(&self) -> bool {
        // GstMemory writability only checks its locks. Wait for every GStreamer
        // reference to go away, including sub-memory references to this parent.
        let refs = unsafe {
            AtomicI32::from_ptr(&raw mut (*self.memory.as_mut_ptr()).mini_object.refcount)
                .load(Ordering::Acquire)
        };
        if refs != 1 {
            return false;
        }
        // VA may drop its reference after submitting GPU work. Wait for the
        // DMA-BUF's reservation fences before handing it back to the compositor.
        let mut fd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        (unsafe { libc::poll(&mut fd, 1, 0) }) > 0 && fd.revents & libc::POLLOUT != 0
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.wl_buffer.destroy();
    }
}

pub(super) struct Pool {
    slots: Vec<Slot>,
    size: (u32, u32),
    format: Format,
    latest: Option<usize>,
}

impl Pool {
    pub fn new(
        dmabuf: &ZwpLinuxDmabufV1,
        conn: &Connection,
        queue: &mut EventQueue<CaptureState>,
        state: &mut CaptureState,
    ) -> Result<Self> {
        let size = state.active_size;
        let caps = gst::ElementFactory::make("vapostproc")
            .build()?
            .static_pad("sink")
            .context("Missing VA postprocessor sink")?
            .query_caps(None);
        let mut formats = Vec::new();
        for (fourcc, video) in [
            (*b"XR24", VideoFormat::Bgrx),
            (*b"AR24", VideoFormat::Bgra),
            (*b"XB24", VideoFormat::Rgbx),
            (*b"AB24", VideoFormat::Rgba),
        ] {
            for (_, modifiers) in state
                .active_dmabuf
                .formats
                .iter()
                .filter(|(code, _)| *code == u32::from_le_bytes(fourcc))
            {
                formats.extend(modifiers.iter().filter_map(|&modifier| {
                    let format = Format {
                        fourcc,
                        video,
                        modifier,
                    };
                    // DRM_FORMAT_MOD_INVALID denotes an implicit modifier. It cannot
                    // be described as a DMA_DRM layout or requested explicitly from GBM.
                    (modifier != 0x00ff_ffff_ffff_ffff && caps.can_intersect(&format.caps(size)))
                        .then_some(format)
                }));
            }
        }
        ensure!(
            !formats.is_empty(),
            "No common Wayland and VA DMA-BUF format"
        );
        // Prefer portable linear layouts, then try any compatible device-specific layout.
        formats.sort_by_key(|format| format.modifier != 0);
        let device_id = state
            .active_dmabuf
            .device
            .context("Wayland did not advertise a DMA-BUF device")?;
        let device = GbmDevice::open(device_id)?;
        let allocator = DmaBufAllocator::new();
        let qh = queue.handle();
        let generation = state.constraints_gen;
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut last_error = None;
        for format in formats {
            let mut slots = Vec::with_capacity(POOL_SIZE);
            for _ in 0..POOL_SIZE {
                ensure!(
                    Instant::now() < deadline,
                    "Timed out allocating DMA-BUF capture buffers"
                );
                let imported = ImportResult::default();
                let params = dmabuf.create_params(&qh, imported.clone());
                let result = Slot::allocate(&device, format, size, &allocator, &params).and_then(
                    |(memory, fd, offset, stride)| {
                        params.create(
                            size.0 as i32,
                            size.1 as i32,
                            u32::from_le_bytes(format.fourcc),
                            zwp_linux_buffer_params_v1::Flags::empty(),
                        );
                        let deadline = Instant::now() + Duration::from_millis(500);
                        let buffer = loop {
                            if let Some(created) = crate::lock(&imported).take() {
                                break created.map_err(|()| {
                                    anyhow::anyhow!("Wayland rejected the DMA-BUF")
                                })?;
                            }
                            ensure!(
                                Instant::now() < deadline,
                                "Timed out importing the Wayland DMA-BUF"
                            );
                            poll_wayland(conn, queue, state, 10)?;
                        };
                        Ok(Slot {
                            wl_buffer: buffer,
                            memory,
                            fd,
                            offset,
                            stride,
                        })
                    },
                );
                params.destroy();
                ensure!(
                    state.constraints_gen == generation
                        && !state.session_stopped
                        && !state.output_removed,
                    "Capture constraints changed while importing DMA-BUFs"
                );
                match result {
                    Ok(slot) => slots.push(slot),
                    Err(err) => {
                        last_error = Some(err);
                        break;
                    }
                }
            }
            if slots.len() == POOL_SIZE {
                return Ok(Self {
                    slots,
                    size,
                    format,
                    latest: None,
                });
            }
        }
        Err(last_error.context("No GBM capture buffer could be imported")?)
    }

    pub fn caps(&self) -> gst::Caps {
        self.format.caps(self.size)
    }

    pub fn available(&self) -> Option<usize> {
        self.slots.iter().enumerate().position(|(i, slot)| {
            // The latest slot is retained for static keyframes.
            Some(i) != self.latest && slot.available()
        })
    }

    pub fn wl_buffer(&self, slot: usize) -> &WlBuffer {
        &self.slots[slot].wl_buffer
    }

    pub fn complete(&mut self, slot: usize) {
        self.latest = Some(slot);
    }

    pub fn latest(&self) -> Result<Option<gst::Buffer>> {
        let Some(slot) = self.latest.map(|idx| &self.slots[idx]) else {
            return Ok(None);
        };
        let mut buffer = gst::Buffer::new();
        let writable = buffer.get_mut().expect("new GStreamer buffer");
        writable.append_memory(slot.memory.clone());
        VideoMeta::add_full(
            writable,
            VideoFrameFlags::empty(),
            self.format.video,
            self.size.0,
            self.size.1,
            &[slot.offset],
            &[slot.stride],
        )?;
        Ok(Some(buffer))
    }
}
