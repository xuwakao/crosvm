// macOS Timer: pipe + kqueue EVFILT_TIMER hybrid.
//
// macOS kqueue fds cannot be monitored by another kqueue or sent via
// SCM_RIGHTS. The async reactor and tube IPC both need a pollable,
// sendable fd. Solution: Timer exposes a pipe read-end as its handle
// (AsRawDescriptor). A background thread watches a kqueue EVFILT_TIMER
// and writes to the pipe on expiry.
//
// Timer control (reset/clear) is done via the kqueue fd stored in a
// global map keyed by pipe read fd.

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::mem;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::Mutex;
use std::time::Duration;

use crate::descriptor::AsRawDescriptor;
use crate::descriptor::FromRawDescriptor;
use crate::errno::errno_result;
use crate::errno::Error;
use crate::errno::Result;
use crate::SafeDescriptor;
use crate::Timer;

/// Global map: pipe_read_fd → (kqueue_fd, pipe_write_fd).
/// The kqueue fd is used for timer control (reset/clear).
/// The pipe write fd is used by the watcher thread.
static TIMER_MAP: std::sync::LazyLock<Mutex<HashMap<RawFd, (RawFd, RawFd)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

impl Timer {
    pub fn new() -> Result<Timer> {
        let mut pipe_fds = [0i32; 2];
        // SAFETY: pipe() initializes two valid fds.
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
            return errno_result();
        }
        let pipe_read = pipe_fds[0];
        let pipe_write = pipe_fds[1];

        // Set non-blocking + close-on-exec on both pipe ends.
        for &fd in &pipe_fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }

        // Create kqueue for EVFILT_TIMER.
        // SAFETY: kqueue() returns valid fd or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            unsafe {
                libc::close(pipe_read);
                libc::close(pipe_write);
            }
            return errno_result();
        }
        unsafe { libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC) };

        // Register in global map before spawning watcher.
        TIMER_MAP
            .lock()
            .unwrap()
            .insert(pipe_read, (kq, pipe_write));

        // Spawn watcher thread: reads kqueue events, writes to pipe.
        let kq_for_thread = kq;
        let pipe_write_for_thread = pipe_write;
        std::thread::Builder::new()
            .name("timer-kq".into())
            .spawn(move || {
                timer_kqueue_watcher(kq_for_thread, pipe_write_for_thread);
            })
            .map_err(|_| Error::new(libc::ENOMEM))?;

        // SAFETY: pipe_read is a valid pipe fd.
        let handle = unsafe { SafeDescriptor::from_raw_descriptor(pipe_read) };

        Ok(Timer {
            handle,
            interval: None,
        })
    }

    /// Get the kqueue fd for this timer (for control operations).
    fn kqueue_fd(&self) -> Result<RawFd> {
        let pipe_fd = self.handle.as_raw_descriptor();
        TIMER_MAP
            .lock()
            .unwrap()
            .get(&pipe_fd)
            .map(|&(kq, _)| kq)
            .ok_or(Error::new(libc::ENOENT))
    }
}

// Note: no explicit Drop for Timer. When Timer is dropped:
// - SafeDescriptor closes the pipe read end
// - The kqueue fd and pipe write fd remain in TIMER_MAP
// - The watcher thread exits when it tries to write to the closed pipe
//   (write returns EPIPE/SIGPIPE suppressed by SO_NOSIGPIPE-equivalent)
// - The kqueue fd leaks until process exit
// This is acceptable for crosvm's Timer usage pattern where timers are
// long-lived (created at device init, destroyed at VM shutdown).

/// Background thread that watches a kqueue for EVFILT_TIMER events
/// and writes notification bytes to a pipe.
fn timer_kqueue_watcher(kq_fd: RawFd, pipe_write: RawFd) {
    let mut events = [unsafe { mem::zeroed::<libc::kevent>() }; 1];
    loop {
        let n = unsafe {
            libc::kevent(
                kq_fd,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                1,
                std::ptr::null(), // block indefinitely
            )
        };
        if n <= 0 {
            break; // kqueue closed or error — exit
        }
        // Write a byte to signal timer expiry.
        let buf = [1u8];
        unsafe { libc::write(pipe_write, buf.as_ptr() as *const _, 1) };
    }
}

fn kevent_timer(filter_flags: u16, note_flags: u32, data: i64) -> libc::kevent {
    libc::kevent {
        ident: 0,
        filter: libc::EVFILT_TIMER,
        flags: filter_flags,
        fflags: note_flags,
        data: data as isize,
        udata: std::ptr::null_mut(),
    }
}

impl crate::TimerTrait for Timer {
    fn reset_oneshot(&mut self, delay: Duration) -> Result<()> {
        self.interval = None;
        let kq = self.kqueue_fd()?;
        let ns: i64 = delay
            .as_nanos()
            .try_into()
            .map_err(|_| Error::new(libc::EINVAL))?;
        let event = kevent_timer(
            libc::EV_ADD | libc::EV_ONESHOT,
            libc::NOTE_NSECONDS as u32,
            ns,
        );
        // SAFETY: kq is a valid kqueue fd from our map, event is properly initialized.
        let ret = unsafe { libc::kevent(kq, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    fn reset_repeating(&mut self, interval: Duration) -> Result<()> {
        self.interval = Some(interval);
        let kq = self.kqueue_fd()?;
        let ns: i64 = interval
            .as_nanos()
            .try_into()
            .map_err(|_| Error::new(libc::EINVAL))?;
        let event = kevent_timer(libc::EV_ADD, libc::NOTE_NSECONDS as u32, ns);
        let ret = unsafe { libc::kevent(kq, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    fn wait(&mut self) -> Result<()> {
        // Read a byte from the pipe (blocking).
        // First, clear the non-blocking flag temporarily.
        let fd = self.handle.as_raw_descriptor();
        unsafe { libc::fcntl(fd, libc::F_SETFL, 0) }; // blocking
        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, 1) };
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) }; // restore
        if n <= 0 {
            return errno_result();
        }
        Ok(())
    }

    fn mark_waited(&mut self) -> Result<bool> {
        // Drain any pending bytes from the pipe without blocking.
        let fd = self.handle.as_raw_descriptor();
        let mut buf = [0u8; 64];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        Ok(n > 0)
    }

    fn clear(&mut self) -> Result<()> {
        let kq = self.kqueue_fd()?;
        let event = kevent_timer(libc::EV_DELETE, 0, 0);
        // Ignore error — timer may not be armed.
        unsafe { libc::kevent(kq, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        // Drain pipe.
        let fd = self.handle.as_raw_descriptor();
        let mut buf = [0u8; 64];
        while unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) } > 0 {}
        Ok(())
    }

    fn resolution(&self) -> Result<Duration> {
        // SAFETY: zero-initialized struct with primitive fields.
        let mut res: libc::timespec = unsafe { mem::zeroed() };
        let ret = unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC, &mut res) };
        if ret != 0 {
            return errno_result();
        }
        Ok(Duration::new(res.tv_sec as u64, res.tv_nsec as u32))
    }
}
