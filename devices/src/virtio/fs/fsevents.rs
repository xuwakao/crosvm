// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS FSEvents monitor for virtiofs adaptive cache invalidation.
//
// Watches a directory tree via the CoreServices FSEvents API and tracks
// changed files as (dev, ino) pairs. PassthroughFs checks this stale set
// in GETATTR/LOOKUP to return timeout=0 for recently-changed files,
// forcing the guest kernel to revalidate on the next access.

use std::collections::HashSet;
use std::ffi::c_char;
use std::ffi::c_void;
use std::ffi::CStr;
use std::ffi::CString;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::io::FromRawFd;
use std::os::unix::io::RawFd;
use std::sync::Arc;

use sync::Mutex;

use super::passthrough::InodeAltKey;

// ── CoreServices FSEvents FFI ──────────────────────────────────────────

type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type DispatchQueueT = *mut c_void;

const K_CF_ALLOCATOR_DEFAULT: CFAllocatorRef = std::ptr::null();
const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;
const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: u64 = 0xFFFFFFFFFFFFFFFF;
const K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS: u32 = 0x00000010;
const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x00000002;

#[repr(C)]
struct FSEventStreamContext {
    version: i32,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
}

type FSEventStreamCallback = extern "C" fn(
    FSEventStreamRef,
    *mut c_void,
    usize,
    *const *const c_char,
    *const u32,
    *const u64,
);

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const *const c_void,
        num_values: isize,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    fn CFRelease(cf: *const c_void);
    static kCFTypeArrayCallBacks: c_void;

    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFArrayRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueueT);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> bool;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);

    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueueT;
    fn dispatch_release(object: *mut c_void);
}

// ── Callback ───────────────────────────────────────────────────────────

/// Context passed to the FSEvents callback via the info pointer.
/// Contains the write end of a pipe for communicating changed paths
/// to the Rust reader thread.
struct CallbackInfo {
    pipe_fd: RawFd,
}

/// FSEvents callback — writes changed file paths (newline-delimited) to a pipe.
/// Runs on a GCD serial dispatch queue. Non-blocking writes ensure the callback
/// never blocks FSEvents delivery even if the reader falls behind.
extern "C" fn fsevents_callback(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *const *const c_char,
    _event_flags: *const u32,
    _event_ids: *const u64,
) {
    let info = unsafe { &*(info as *const CallbackInfo) };
    for i in 0..num_events {
        let path_ptr = unsafe { *event_paths.add(i) };
        if path_ptr.is_null() {
            continue;
        }
        let path = unsafe { CStr::from_ptr(path_ptr) };
        let bytes = path.to_bytes();
        // Write path + newline atomically (if < PIPE_BUF).
        // Non-blocking: silently drops events if pipe is full.
        unsafe {
            libc::write(info.pipe_fd, bytes.as_ptr() as *const c_void, bytes.len());
            libc::write(info.pipe_fd, b"\n".as_ptr() as *const c_void, 1);
        }
    }
}

// ── Monitor ────────────────────────────────────────────────────────────

/// Monitors a directory tree for filesystem changes using macOS FSEvents.
///
/// Architecture:
/// - FSEvents callback (GCD dispatch queue) → writes paths to pipe
/// - Reader thread → reads from pipe, stat()s each path, adds (dev, ino) to stale set
/// - PassthroughFs → checks stale set in GETATTR/LOOKUP, returns timeout=0 if stale
pub struct FsEventsMonitor {
    stream: FSEventStreamRef,
    queue: DispatchQueueT,
    // Must outlive the FSEvents stream (pointer stored in stream context).
    _info: Box<CallbackInfo>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    // Write end of the pipe. Closed on Drop to signal reader thread exit.
    pipe_write_fd: RawFd,
}

// SAFETY: FSEventStreamRef and DispatchQueueT are opaque CoreServices pointers.
// Apple APIs manage their own thread safety. We only call Stop/Invalidate/Release
// from the Drop impl (single owner).
unsafe impl Send for FsEventsMonitor {}

impl FsEventsMonitor {
    /// Start monitoring `path` for changes. Changed inodes are added to
    /// `stale_inodes` for PassthroughFs to check in GETATTR/LOOKUP.
    pub fn start(
        path: &str,
        stale_inodes: Arc<Mutex<HashSet<InodeAltKey>>>,
    ) -> io::Result<Self> {
        // Pipe: FSEvents callback (write) → reader thread (read).
        let mut pipe_fds = [0i32; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let pipe_read_fd = pipe_fds[0];
        let pipe_write_fd = pipe_fds[1];

        // Non-blocking writes so the callback never stalls FSEvents.
        unsafe {
            let flags = libc::fcntl(pipe_write_fd, libc::F_GETFL);
            libc::fcntl(pipe_write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let info = Box::new(CallbackInfo {
            pipe_fd: pipe_write_fd,
        });

        // Create CFString path.
        let c_path = CString::new(path).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "path contains null byte")
        })?;
        let cf_path = unsafe {
            CFStringCreateWithCString(
                K_CF_ALLOCATOR_DEFAULT,
                c_path.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if cf_path.is_null() {
            unsafe {
                libc::close(pipe_read_fd);
                libc::close(pipe_write_fd);
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "CFStringCreateWithCString failed",
            ));
        }

        // Create CFArray with single path.
        let paths = unsafe {
            CFArrayCreate(
                K_CF_ALLOCATOR_DEFAULT,
                &cf_path as *const _ as *const *const c_void,
                1,
                &kCFTypeArrayCallBacks as *const _ as *const c_void,
            )
        };
        unsafe { CFRelease(cf_path) };
        if paths.is_null() {
            unsafe {
                libc::close(pipe_read_fd);
                libc::close(pipe_write_fd);
            }
            return Err(io::Error::new(io::ErrorKind::Other, "CFArrayCreate failed"));
        }

        // Stream context with pointer to our CallbackInfo.
        let stream_ctx = FSEventStreamContext {
            version: 0,
            info: &*info as *const CallbackInfo as *mut c_void,
            retain: None,
            release: None,
            copy_description: None,
        };

        // kFSEventStreamCreateFlagFileEvents: per-file events (not just directory).
        // kFSEventStreamCreateFlagNoDefer: leading-edge delivery for low latency.
        let stream = unsafe {
            FSEventStreamCreate(
                K_CF_ALLOCATOR_DEFAULT,
                fsevents_callback,
                &stream_ctx,
                paths,
                K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
                0.1, // 100ms coalesce latency (FSEvents minimum)
                K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS
                    | K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER,
            )
        };
        unsafe { CFRelease(paths) };

        if stream.is_null() {
            unsafe {
                libc::close(pipe_read_fd);
                libc::close(pipe_write_fd);
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "FSEventStreamCreate failed",
            ));
        }

        // Serial dispatch queue for FSEvents processing.
        let label = CString::new("com.aetheria.fsevents").unwrap();
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null()) };

        unsafe {
            FSEventStreamSetDispatchQueue(stream, queue);
            if !FSEventStreamStart(stream) {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
                libc::close(pipe_read_fd);
                libc::close(pipe_write_fd);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "FSEventStreamStart failed",
                ));
            }
        }

        // Reader thread: pipe → stat → stale set.
        let reader_thread = std::thread::Builder::new()
            .name("fsevents_reader".into())
            .spawn(move || {
                // SAFETY: we own pipe_read_fd, transferred from the pipe() call above.
                let file = unsafe { std::fs::File::from_raw_fd(pipe_read_fd) };
                let reader = BufReader::new(file);
                for line in reader.lines() {
                    match line {
                        Ok(path) if !path.is_empty() => {
                            // stat the changed file to get (dev, ino).
                            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                                use std::os::unix::fs::MetadataExt;
                                let key = InodeAltKey {
                                    ino: meta.ino() as _,
                                    dev: meta.dev() as _,
                                };
                                stale_inodes.lock().insert(key);
                            }
                            // stat failure (file deleted) — the parent directory entry
                            // will expire via normal timeout, no special handling needed.
                        }
                        Err(_) => break, // Pipe closed or I/O error → shutdown.
                        _ => {}
                    }
                }
                base::info!("FSEvents reader thread exiting");
            })?;

        base::info!("FSEvents monitor started for '{}'", path);

        Ok(Self {
            stream,
            queue,
            _info: info,
            reader_thread: Some(reader_thread),
            pipe_write_fd,
        })
    }
}

impl Drop for FsEventsMonitor {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
            // Close write end → reader thread sees EOF → exits.
            libc::close(self.pipe_write_fd);
        }
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        base::info!("FSEvents monitor stopped");
    }
}
