// macOS Event implementation using pipe for cross-kqueue compatibility.
//
// macOS kqueue fds cannot be monitored by another kqueue (EVFILT_READ
// on a kqueue fd returns ENOTTY) or sent via SCM_RIGHTS (sendmsg returns
// EINVAL). Since Events need to work with the kqueue-based async reactor
// and tube IPC, we use a pipe instead of kqueue EVFILT_USER.
//
// signal() = write a byte to the pipe
// wait()   = read a byte from the pipe (blocking)
// reset()  = drain all bytes (non-blocking)

use std::time::Duration;

use crate::errno::errno_result;
use crate::errno::Result;
use crate::event::EventWaitResult;
use crate::descriptor::AsRawDescriptor;
use crate::descriptor::FromRawDescriptor;
use crate::sys::unix::RawDescriptor;
use crate::SafeDescriptor;

/// Pipe-backed event for macOS.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlatformEvent {
    // Read end of the pipe — this is the "event fd".
    read_end: SafeDescriptor,
    // Write end of the pipe — used for signaling.
    write_end: SafeDescriptor,
}

impl PlatformEvent {
    pub fn new() -> Result<PlatformEvent> {
        let mut fds = [0i32; 2];
        // SAFETY: pipe() initializes two valid fds.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return errno_result();
        }
        // Set both ends non-blocking and close-on-exec.
        for &fd in &fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        // SAFETY: pipe fds are valid.
        unsafe {
            Ok(PlatformEvent {
                read_end: SafeDescriptor::from_raw_descriptor(fds[0]),
                write_end: SafeDescriptor::from_raw_descriptor(fds[1]),
            })
        }
    }

    pub fn signal(&self) -> Result<()> {
        let buf = [1u8];
        // SAFETY: write_end is a valid pipe fd, buf is valid.
        let ret = unsafe {
            libc::write(
                self.write_end.as_raw_descriptor(),
                buf.as_ptr() as *const _,
                1,
            )
        };
        // EAGAIN is OK — pipe already has data (already signaled).
        if ret < 0 {
            let err = unsafe { *libc::__error() };
            if err != libc::EAGAIN {
                return errno_result();
            }
        }
        Ok(())
    }

    pub fn wait(&self) -> Result<()> {
        // Block until a byte is available.
        let fd = self.read_end.as_raw_descriptor();
        // Temporarily set blocking mode.
        unsafe { libc::fcntl(fd, libc::F_SETFL, 0) };
        let mut buf = [0u8; 1];
        let ret = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, 1) };
        // Restore non-blocking mode.
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
        if ret <= 0 {
            return errno_result();
        }
        Ok(())
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Result<EventWaitResult> {
        let fd = self.read_end.as_raw_descriptor();
        // Use poll() with timeout.
        let timeout_ms = timeout.as_millis() as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd is properly initialized, 1 fd.
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret < 0 {
            return errno_result();
        }
        if ret == 0 {
            return Ok(EventWaitResult::TimedOut);
        }
        // Data available — read it.
        let mut buf = [0u8; 1];
        unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, 1) };
        Ok(EventWaitResult::Signaled)
    }

    pub fn reset(&self) -> Result<()> {
        // Drain all pending bytes without blocking.
        let fd = self.read_end.as_raw_descriptor();
        let mut buf = [0u8; 64];
        while unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) } > 0 {}
        Ok(())
    }

    pub fn try_clone(&self) -> Result<PlatformEvent> {
        Ok(PlatformEvent {
            read_end: self.read_end.try_clone()?,
            write_end: self.write_end.try_clone()?,
        })
    }
}

impl AsRawDescriptor for PlatformEvent {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        // Return the read end — this is what the kqueue reactor monitors.
        self.read_end.as_raw_descriptor()
    }
}

impl crate::FromRawDescriptor for PlatformEvent {
    /// Reconstruct from the read end fd. The write end is lost —
    /// this Event can only be used for waiting, not signaling.
    /// Used when receiving an Event via SCM_RIGHTS.
    unsafe fn from_raw_descriptor(descriptor: RawDescriptor) -> Self {
        PlatformEvent {
            read_end: SafeDescriptor::from_raw_descriptor(descriptor),
            // Create a dummy write end that will fail on write.
            // Events reconstructed from raw fd are receive-only.
            write_end: SafeDescriptor::from_raw_descriptor(-1),
        }
    }
}

impl crate::IntoRawDescriptor for PlatformEvent {
    fn into_raw_descriptor(self) -> RawDescriptor {
        // Leak the write end — caller is taking ownership of the read end only.
        let fd = self.read_end.as_raw_descriptor();
        std::mem::forget(self);
        fd
    }
}

impl From<PlatformEvent> for SafeDescriptor {
    fn from(evt: PlatformEvent) -> Self {
        evt.read_end
    }
}

impl From<SafeDescriptor> for PlatformEvent {
    fn from(sd: SafeDescriptor) -> Self {
        PlatformEvent {
            read_end: sd,
            write_end: unsafe { SafeDescriptor::from_raw_descriptor(-1) },
        }
    }
}
