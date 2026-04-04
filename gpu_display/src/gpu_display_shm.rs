// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: MIT
//
// gpu_display_shm.rs — Shared memory display backend for macOS.
//
// Writes framebuffer data (XRGB8888) to a mmap'd shared memory file and
// signals frame readiness via a Unix socket. An external process
// (AetheriaDisplay.app) reads the shared memory and renders via Metal.
//
// Shared memory layout (double-buffered):
//   Offset 0:                ShmHeader (padded to 4096)
//   Offset 4096:             Buffer 0 (width * height * 4, XRGB8888)
//   Offset 4096 + fb_size:   Buffer 1 (width * height * 4, XRGB8888)
//
// Control socket protocol:
//   crosvm → app: 'F' (frame ready)
//   crosvm → app: 'R' + u32le(width) + u32le(height) (resize)

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use anyhow::bail;
use base::AsRawDescriptor;
use base::Event;
use base::RawDescriptor;
use base::VolatileSlice;
use vm_control::gpu::DisplayParameters;

use crate::DisplayExternalResourceImport;
use crate::DisplayT;
use crate::FlipToExtraInfo;
use crate::GpuDisplayError;
use crate::GpuDisplayFramebuffer;
use crate::GpuDisplayResult;
use crate::GpuDisplaySurface;
use crate::SemaphoreTimepoint;
use crate::SurfaceType;
use crate::SysDisplayT;
use crate::Waitable;

/// Magic number for shared memory header: "AETH" (0x41455448).
const SHM_MAGIC: u32 = 0x4845_5441; // Little-endian "AETH"
const SHM_VERSION: u32 = 1;
const SHM_HEADER_SIZE: usize = 4096; // Page-aligned header
const BYTES_PER_PIXEL: u32 = 4; // XRGB8888

const SHM_PATH: &str = "/tmp/aetheria-display.shm";
const SOCKET_PATH: &str = "/tmp/aetheria-display.sock";

/// Shared memory header, mapped at offset 0 of the shm file.
/// All fields are little-endian u32.
///
/// Double-buffered layout:
///   Offset 0:                ShmHeader (padded to 4096)
///   Offset 4096:             Buffer 0 (width * height * 4)
///   Offset 4096 + fb_size:   Buffer 1 (width * height * 4)
///
/// crosvm writes to the BACK buffer (1 - active_buffer).
/// On flip(), active_buffer is swapped atomically.
/// Swift app reads from FRONT buffer (active_buffer).
#[repr(C)]
struct ShmHeader {
    magic: u32,
    version: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,          // DRM_FORMAT_XRGB8888 = 0x34325258
    frame_seq: AtomicU32,
    active_buffer: AtomicU32, // 0 or 1 — front buffer index for reader
}

/// An imported GPU resource — mmap'd fd from gfxstream export_blob.
struct ImportedResource {
    /// Pointer to the mmap'd GPU memory.
    ptr: *mut u8,
    /// Size of the mmap'd region.
    size: usize,
    /// Width, height, stride of the imported image.
    width: u32,
    height: u32,
    stride: u32,
}

// Safety: ImportedResource contains a raw pointer from mmap. The mmap'd region
// is valid for the lifetime of the resource (munmap in Drop). Access is serialized
// via Mutex in the shared imports map.
unsafe impl Send for ImportedResource {}

impl Drop for ImportedResource {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.size);
        }
    }
}

/// Display surface backed by shared memory.
struct ShmSurface {
    width: u32,
    height: u32,
    /// Pointer to the mmap'd region (header + framebuffer).
    mmap_ptr: *mut u8,
    mmap_len: usize,
    /// Connected display app (if any).
    client: Option<UnixStream>,
    /// Listener for reconnection after client disconnect.
    listener: Option<std::sync::Arc<UnixListener>>,
    /// Shared import map — populated by DisplayShm::import_resource,
    /// read by ShmSurface::flip_to. Shared via Arc<Mutex<>>.
    imports: std::sync::Arc<std::sync::Mutex<HashMap<u32, ImportedResource>>>,
}

// Safety: ShmSurface is only used from the GPU thread.
unsafe impl Send for ShmSurface {}

impl ShmSurface {
    fn new(
        width: u32,
        height: u32,
        client: Option<UnixStream>,
        listener: std::sync::Arc<UnixListener>,
        imports: std::sync::Arc<std::sync::Mutex<HashMap<u32, ImportedResource>>>,
    ) -> GpuDisplayResult<Self> {
        let fb_size = (width as usize) * (height as usize) * (BYTES_PER_PIXEL as usize);
        let mmap_len = SHM_HEADER_SIZE + fb_size * 2; // double buffer

        // Create and size the shared memory file.
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(SHM_PATH)
            .map_err(|e| {
                eprintln!("[shm-display] create shm file: {}", e);
                GpuDisplayError::CreateSurface
            })?;

        file.set_len(mmap_len as u64).map_err(|e| {
            eprintln!("[shm-display] set shm size: {}", e);
            GpuDisplayError::CreateSurface
        })?;

        // mmap the file.
        let mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mmap_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                0,
            )
        };

        if mmap_ptr == libc::MAP_FAILED {
            eprintln!("[shm-display] mmap failed");
            return Err(GpuDisplayError::CreateSurface);
        }

        let mmap_ptr = mmap_ptr as *mut u8;

        // Write header. Magic is written LAST — the reader checks magic first,
        // so all other fields are guaranteed initialized when magic is valid.
        let header = unsafe { &mut *(mmap_ptr as *mut ShmHeader) };
        header.version = SHM_VERSION;
        header.width = width;
        header.height = height;
        header.stride = width * BYTES_PER_PIXEL;
        header.format = 0x3432_5258; // DRM_FORMAT_XRGB8888
        header.frame_seq = AtomicU32::new(0);
        header.active_buffer = AtomicU32::new(0);
        // Memory barrier before writing magic to ensure all fields are visible.
        std::sync::atomic::fence(Ordering::Release);
        header.magic = SHM_MAGIC;

        eprintln!(
            "[shm-display] surface created: {}x{}, shm={}, socket={}",
            width,
            height,
            SHM_PATH,
            SOCKET_PATH
        );

        Ok(ShmSurface {
            width,
            height,
            mmap_ptr,
            mmap_len,
            client,
            listener: Some(listener),
            imports,
        })
    }

    fn single_buffer_size(&self) -> usize {
        (self.width as usize) * (self.height as usize) * (BYTES_PER_PIXEL as usize)
    }

    /// Returns pointer to the BACK buffer (the one crosvm writes to).
    /// Back buffer = 1 - active_buffer.
    fn back_buffer_ptr(&mut self) -> *mut u8 {
        let active = self.header().active_buffer.load(Ordering::Acquire) & 1;
        let back = 1 - active;
        let offset = SHM_HEADER_SIZE + (back as usize) * self.single_buffer_size();
        unsafe { self.mmap_ptr.add(offset) }
    }

    fn header(&self) -> &ShmHeader {
        unsafe { &*(self.mmap_ptr as *const ShmHeader) }
    }

    fn signal_frame(&mut self) {
        // Swap buffers: the back buffer (just written) becomes the front buffer.
        let old_active = self.header().active_buffer.load(Ordering::Acquire) & 1;
        self.header().active_buffer.store(1 - old_active, Ordering::Release);

        // Increment frame sequence number.
        self.header().frame_seq.fetch_add(1, Ordering::Release);

        // Signal display app via control socket.
        if let Some(ref mut client) = self.client {
            if client.write_all(b"F").is_err() {
                eprintln!("[shm-display] client disconnected");
                self.client = None;
            }
        }

        // Try to accept a new client if disconnected.
        if self.client.is_none() {
            if let Some(ref listener) = self.listener {
                if let Ok((stream, _)) = listener.accept() {
                    eprintln!("[shm-display] display app reconnected");
                    stream.set_nonblocking(true).ok();
                    self.client = Some(stream);
                }
            }
        }
    }
}

impl GpuDisplaySurface for ShmSurface {
    fn framebuffer(&mut self) -> Option<GpuDisplayFramebuffer> {
        let fb_ptr = self.back_buffer_ptr();
        let fb_size = self.single_buffer_size();
        let stride = self.width * BYTES_PER_PIXEL;

        // Safety: the mmap'd region is valid for the lifetime of ShmSurface.
        // Returns the back buffer — crosvm writes here while Swift reads from front.
        let slice = unsafe { VolatileSlice::from_raw_parts(fb_ptr, fb_size) };
        Some(GpuDisplayFramebuffer::new(slice, stride, BYTES_PER_PIXEL))
    }

    fn flip(&mut self) {
        self.signal_frame();
    }

    fn flip_to(
        &mut self,
        import_id: u32,
        _acquire_timepoint: Option<SemaphoreTimepoint>,
        _release_timepoint: Option<SemaphoreTimepoint>,
        _extra_info: Option<FlipToExtraInfo>,
    ) -> anyhow::Result<Waitable> {
        // Extract import data under lock, then release before signal_frame (needs &mut self).
        let (src_ptr, src_stride, src_height) = {
            let imports = self.imports.lock().unwrap();
            match imports.get(&import_id) {
                Some(r) => (r.ptr, r.stride as usize, r.height as usize),
                None => bail!("import_id {} not found", import_id),
            }
        };

        let back_ptr = self.back_buffer_ptr();
        let dst_stride = (self.width as usize) * (BYTES_PER_PIXEL as usize);
        let rows = (self.height as usize).min(src_height);
        let row_bytes = dst_stride.min(src_stride);

        if src_stride == dst_stride {
            // Fast path: strides match, single memcpy.
            let copy_size = rows * dst_stride;
            unsafe {
                std::ptr::copy_nonoverlapping(src_ptr, back_ptr, copy_size);
            }
        } else {
            // Stride mismatch: copy row by row to handle GPU alignment padding.
            for row in 0..rows {
                unsafe {
                    let src = src_ptr.add(row * src_stride);
                    let dst = back_ptr.add(row * dst_stride);
                    std::ptr::copy_nonoverlapping(src, dst, row_bytes);
                }
            }
        }

        self.signal_frame();
        Ok(Waitable::signaled())
    }
}

impl Drop for ShmSurface {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mmap_ptr as *mut libc::c_void, self.mmap_len);
        }
        // Don't remove shm file — display app may still be reading it.
    }
}

/// Shared memory display backend.
pub struct DisplayShm {
    event: Event,
    listener: std::sync::Arc<UnixListener>,
    client: Option<UnixStream>,
    /// Shared import map — populated here, read by ShmSurface::flip_to.
    imports: std::sync::Arc<std::sync::Mutex<HashMap<u32, ImportedResource>>>,
}

impl DisplayShm {
    pub fn new() -> GpuDisplayResult<Self> {
        let event = Event::new().map_err(|_| GpuDisplayError::CreateEvent)?;

        // Clean up stale socket.
        let _ = fs::remove_file(SOCKET_PATH);

        let listener = UnixListener::bind(SOCKET_PATH).map_err(|e| {
            eprintln!("[shm-display] bind socket {}: {}", SOCKET_PATH, e);
            GpuDisplayError::CreateEvent
        })?;

        // Non-blocking so we can check for connections without blocking the GPU thread.
        listener
            .set_nonblocking(true)
            .map_err(|_| GpuDisplayError::CreateEvent)?;

        eprintln!("[shm-display] listening on {}", SOCKET_PATH);

        Ok(DisplayShm {
            event,
            listener: std::sync::Arc::new(listener),
            client: None,
            imports: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Accept a pending client connection (non-blocking).
    fn try_accept(&mut self) {
        match self.listener.accept() {
            Ok((stream, _)) => {
                eprintln!("[shm-display] display app connected");
                stream.set_nonblocking(true).ok();
                self.client = Some(stream);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connection — normal.
            }
            Err(e) => {
                eprintln!("[shm-display] accept error: {}", e);
            }
        }
    }
}

impl DisplayT for DisplayShm {
    fn import_resource(
        &mut self,
        import_id: u32,
        _surface_id: u32,
        external_display_resource: DisplayExternalResourceImport,
    ) -> anyhow::Result<()> {
        match external_display_resource {
            DisplayExternalResourceImport::Dmabuf {
                descriptor,
                offset: _,
                stride,
                modifiers: _,
                width,
                height,
                fourcc: _,
            } => {
                let fd = descriptor.as_raw_descriptor();

                // Get the size of the backing memory.
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                let stat_ret = unsafe { libc::fstat(fd, &mut stat) };
                let size = if stat_ret == 0 && stat.st_size > 0 {
                    stat.st_size as usize
                } else {
                    // Fallback: use dimensions if fstat fails (common with opaque fds).
                    (height as usize) * (stride as usize)
                };

                if size == 0 {
                    bail!("import_resource: zero size for import_id {}", import_id);
                }

                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        fd,
                        0,
                    )
                };

                if ptr == libc::MAP_FAILED {
                    bail!(
                        "import_resource: mmap failed for import_id {} (fd={}, size={})",
                        import_id, fd, size
                    );
                }

                let resource = ImportedResource {
                    ptr: ptr as *mut u8,
                    size,
                    width,
                    height,
                    stride,
                };

                eprintln!(
                    "[shm-display] imported resource {}: {}x{} stride={} size={}",
                    import_id, width, height, stride, size
                );

                self.imports.lock().unwrap().insert(import_id, resource);
                Ok(())
            }
            _ => {
                // VulkanImage, VulkanTimelineSemaphore, AHardwareBuffer — not supported.
                // Let the caller fall back to transfer_read.
                bail!("import_resource: only Dmabuf supported on SharedMemory backend")
            }
        }
    }

    fn release_import(&mut self, import_id: u32, _surface_id: u32) {
        if self.imports.lock().unwrap().remove(&import_id).is_some() {
            eprintln!("[shm-display] released import {}", import_id);
        }
    }

    fn create_surface(
        &mut self,
        parent_surface_id: Option<u32>,
        _surface_id: u32,
        _scanout_id: Option<u32>,
        display_params: &DisplayParameters,
        _surf_type: SurfaceType,
    ) -> GpuDisplayResult<Box<dyn GpuDisplaySurface>> {
        if parent_surface_id.is_some() {
            return Err(GpuDisplayError::Unsupported);
        }

        // Check for client connection before creating surface.
        self.try_accept();

        let (width, height) = display_params.get_virtual_display_size();
        let surface = ShmSurface::new(width, height, self.client.take(), self.listener.clone(), self.imports.clone())?;
        Ok(Box::new(surface))
    }
}

impl SysDisplayT for DisplayShm {}

impl AsRawDescriptor for DisplayShm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.event.as_raw_descriptor()
    }
}
