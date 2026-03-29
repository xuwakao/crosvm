// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// macOS platform layer for crosvm base.
// Extended by the Aetheria project with working implementations.

use std::fs::File;
use std::os::unix::io::AsRawFd;

use crate::descriptor::FromRawDescriptor;
use crate::sys::unix::RawDescriptor;
use crate::unix::set_descriptor_cloexec;
use crate::unix::Pid;
use crate::MmapError;

mod event;
pub(in crate::sys::macos) mod kqueue;
pub(crate) mod mmap;
mod net;
mod timer;

pub(crate) use event::PlatformEvent;
pub(in crate::sys) use libc::sendmsg;
pub(in crate::sys) use net::sockaddr_un;
pub(in crate::sys) use net::sockaddrv4_to_lib_c;
pub(in crate::sys) use net::sockaddrv6_to_lib_c;

pub fn set_thread_name(name: &str) -> crate::errno::Result<()> {
    // macOS pthread_setname_np only takes the name (sets for current thread).
    let c_name = std::ffi::CString::new(name).map_err(|_| crate::errno::Error::new(libc::EINVAL))?;
    // SAFETY: c_name is a valid null-terminated C string.
    let ret = unsafe { libc::pthread_setname_np(c_name.as_ptr()) };
    if ret == 0 {
        Ok(())
    } else {
        Err(crate::errno::Error::new(ret))
    }
}

pub fn get_cpu_affinity() -> crate::errno::Result<Vec<usize>> {
    // macOS does not support getting CPU affinity. Return all CPUs.
    let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if count < 1 {
        return Err(crate::errno::Error::last());
    }
    Ok((0..count as usize).collect())
}

pub fn getpid() -> Pid {
    // SAFETY: getpid is always safe.
    unsafe { libc::getpid() }
}

pub fn open_file_or_duplicate<P: AsRef<std::path::Path>>(
    path: P,
    options: &std::fs::OpenOptions,
) -> crate::Result<std::fs::File> {
    Ok(options.open(path)?)
}

pub mod platform_timer_resolution {
    pub struct UnixSetTimerResolution {}
    impl crate::EnabledHighResTimer for UnixSetTimerResolution {}

    pub fn enable_high_res_timers() -> crate::Result<Box<dyn crate::EnabledHighResTimer>> {
        // macOS already has high-resolution timers by default (mach_absolute_time).
        Ok(Box::new(UnixSetTimerResolution {}))
    }
}

pub fn set_cpu_affinity<I: IntoIterator<Item = usize>>(_cpus: I) -> crate::errno::Result<()> {
    // macOS thread_policy_set has limited affinity support. Best-effort no-op.
    Ok(())
}

const EVENT_CONTEXT_MAX_EVENTS: usize = 16;

/// kqueue-based event context, equivalent to Linux's epoll-based EventContext.
///
/// Uses kqueue to monitor multiple file descriptors for readability/writability.
/// Token data is stored in the kevent's `udata` field (64-bit on ARM64 macOS).
pub struct EventContext<T: crate::EventToken> {
    kqueue_fd: File,
    tokens: std::marker::PhantomData<[T]>,
}

impl<T: crate::EventToken> EventContext<T> {
    pub fn new() -> crate::errno::Result<EventContext<T>> {
        // SAFETY: kqueue() returns a new fd or -1 on error.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return crate::errno::errno_result();
        }
        // SAFETY: kq is a valid fd from kqueue().
        let kqueue_fd: File = unsafe { crate::descriptor::FromRawDescriptor::from_raw_descriptor(kq) };
        crate::unix::set_descriptor_cloexec(&kqueue_fd)?;
        Ok(EventContext {
            kqueue_fd,
            tokens: std::marker::PhantomData,
        })
    }

    pub fn build_with(
        fd_tokens: &[(&dyn crate::AsRawDescriptor, T)],
    ) -> crate::errno::Result<EventContext<T>> {
        let ctx = EventContext::new()?;
        for (fd, token) in fd_tokens {
            ctx.add_for_event(
                *fd,
                crate::EventType::Read,
                T::from_raw_token(token.as_raw_token()),
            )?;
        }
        Ok(ctx)
    }

    pub fn add_for_event(
        &self,
        descriptor: &dyn crate::AsRawDescriptor,
        event_type: crate::EventType,
        token: T,
    ) -> crate::errno::Result<()> {
        let fd = descriptor.as_raw_descriptor();
        let raw_token = token.as_raw_token();
        let mut changes = smallvec::SmallVec::<[libc::kevent; 2]>::new();

        match event_type {
            crate::EventType::Read | crate::EventType::ReadWrite => {
                changes.push(libc::kevent {
                    ident: fd as libc::uintptr_t,
                    filter: libc::EVFILT_READ,
                    flags: libc::EV_ADD | libc::EV_CLEAR,
                    fflags: 0,
                    data: 0,
                    udata: raw_token as *mut std::ffi::c_void,
                });
            }
            _ => {}
        }
        match event_type {
            crate::EventType::Write | crate::EventType::ReadWrite => {
                changes.push(libc::kevent {
                    ident: fd as libc::uintptr_t,
                    filter: libc::EVFILT_WRITE,
                    flags: libc::EV_ADD | libc::EV_CLEAR,
                    fflags: 0,
                    data: 0,
                    udata: raw_token as *mut std::ffi::c_void,
                });
            }
            _ => {}
        }

        if changes.is_empty() {
            return Ok(());
        }

        // SAFETY: valid kqueue fd and properly initialized kevent structs.
        let ret = unsafe {
            libc::kevent(
                self.kqueue_fd.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if ret < 0 {
            return crate::errno::errno_result();
        }
        Ok(())
    }

    /// Modify is equivalent to add on kqueue (EV_ADD replaces existing).
    pub fn modify(
        &self,
        fd: &dyn crate::AsRawDescriptor,
        event_type: crate::EventType,
        token: T,
    ) -> crate::errno::Result<()> {
        // On kqueue, EV_ADD with the same ident+filter replaces the existing entry.
        // First delete any existing filters, then add the new ones.
        let _ = self.delete(fd);
        self.add_for_event(fd, event_type, token)
    }

    pub fn delete(&self, fd: &dyn crate::AsRawDescriptor) -> crate::errno::Result<()> {
        let raw_fd = fd.as_raw_descriptor();
        let changes = [
            libc::kevent {
                ident: raw_fd as libc::uintptr_t,
                filter: libc::EVFILT_READ,
                flags: libc::EV_DELETE,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            },
            libc::kevent {
                ident: raw_fd as libc::uintptr_t,
                filter: libc::EVFILT_WRITE,
                flags: libc::EV_DELETE,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            },
        ];

        // SAFETY: valid kqueue fd and kevent structs.
        // Ignore errors — deleting a non-existent filter returns ENOENT, which is fine.
        unsafe {
            libc::kevent(
                self.kqueue_fd.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            );
        }
        Ok(())
    }

    pub fn wait(&self) -> crate::errno::Result<smallvec::SmallVec<[crate::TriggeredEvent<T>; 16]>> {
        self.wait_timeout(std::time::Duration::new(i64::MAX as u64, 0))
    }

    pub fn wait_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> crate::errno::Result<smallvec::SmallVec<[crate::TriggeredEvent<T>; 16]>> {
        let mut events: [std::mem::MaybeUninit<libc::kevent>; EVENT_CONTEXT_MAX_EVENTS] =
            unsafe { std::mem::MaybeUninit::uninit().assume_init() };

        let ts = if timeout.as_secs() as i64 == i64::MAX {
            // No timeout — block indefinitely.
            std::ptr::null()
        } else {
            &libc::timespec {
                tv_sec: timeout.as_secs() as libc::time_t,
                tv_nsec: timeout.subsec_nanos() as libc::c_long,
            } as *const libc::timespec
        };

        // SAFETY: valid kqueue fd and properly sized events array.
        let ret = unsafe {
            libc::kevent(
                self.kqueue_fd.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr() as *mut libc::kevent,
                EVENT_CONTEXT_MAX_EVENTS as i32,
                ts,
            )
        };
        if ret < 0 {
            return crate::errno::errno_result();
        }

        let count = ret as usize;
        let triggered = events[..count]
            .iter()
            .map(|e| {
                // SAFETY: kevent() initialized these entries.
                let e = unsafe { e.assume_init() };
                let raw_token = e.udata as u64;
                crate::TriggeredEvent {
                    token: T::from_raw_token(raw_token),
                    is_readable: e.filter == libc::EVFILT_READ,
                    is_writable: e.filter == libc::EVFILT_WRITE,
                    is_hungup: (e.flags & libc::EV_EOF) != 0,
                }
            })
            .collect();
        Ok(triggered)
    }
}

impl<T: crate::EventToken> crate::AsRawDescriptor for EventContext<T> {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.kqueue_fd.as_raw_fd()
    }
}

pub use mmap::*;

pub mod ioctl {
    use crate::AsRawDescriptor;

    pub type IoctlNr = std::ffi::c_ulong;

    /// # Safety
    /// Caller must ensure the ioctl is valid for the given descriptor.
    pub unsafe fn ioctl<F: AsRawDescriptor>(
        descriptor: &F,
        nr: IoctlNr,
    ) -> std::ffi::c_int {
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _)
    }

    /// # Safety
    /// Caller must ensure the ioctl is valid for the given descriptor and argument.
    pub unsafe fn ioctl_with_val(
        descriptor: &dyn AsRawDescriptor,
        nr: IoctlNr,
        arg: std::ffi::c_ulong,
    ) -> std::ffi::c_int {
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg)
    }

    /// # Safety
    /// Caller must ensure the ioctl is valid and arg points to valid memory.
    pub unsafe fn ioctl_with_ref<T>(
        descriptor: &dyn AsRawDescriptor,
        nr: IoctlNr,
        arg: &T,
    ) -> std::ffi::c_int {
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg as *const T)
    }

    /// # Safety
    /// Caller must ensure the ioctl is valid and arg points to valid memory.
    pub unsafe fn ioctl_with_mut_ref<T>(
        descriptor: &dyn AsRawDescriptor,
        nr: IoctlNr,
        arg: &mut T,
    ) -> std::ffi::c_int {
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg as *mut T)
    }

    /// # Safety
    /// Caller must ensure the ioctl is valid and arg points to valid memory.
    pub unsafe fn ioctl_with_ptr<T>(
        descriptor: &dyn AsRawDescriptor,
        nr: IoctlNr,
        arg: *const T,
    ) -> std::ffi::c_int {
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg)
    }

    /// # Safety
    /// Caller must ensure the ioctl is valid and arg points to valid memory.
    pub unsafe fn ioctl_with_mut_ptr<T>(
        descriptor: &dyn AsRawDescriptor,
        nr: IoctlNr,
        arg: *mut T,
    ) -> std::ffi::c_int {
        libc::ioctl(descriptor.as_raw_descriptor(), nr as _, arg)
    }
}

pub fn file_punch_hole(file: &std::fs::File, offset: u64, length: u64) -> std::io::Result<()> {
    // macOS supports F_PUNCHHOLE since macOS 10.12.
    #[repr(C)]
    struct FPunchholeArgs {
        fp_flags: u32,
        reserved: u32,
        fp_offset: u64,
        fp_length: u64,
    }
    let args = FPunchholeArgs {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset,
        fp_length: length,
    };
    // SAFETY: args is a valid struct for F_PUNCHHOLE.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &args) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn file_write_zeroes_at(
    file: &std::fs::File,
    offset: u64,
    length: usize,
) -> std::io::Result<usize> {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    // Write actual zeros. No fallocate on macOS.
    let zeros = vec![0u8; length];
    // SAFETY: pwrite is safe with valid fd.
    let ret = unsafe {
        libc::pwrite(
            file.as_raw_fd(),
            zeros.as_ptr() as *const _,
            length,
            offset as libc::off_t,
        )
    };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

pub mod syslog {
    pub struct PlatformSyslog {}

    impl crate::syslog::Syslog for PlatformSyslog {
        fn new(
            _proc_name: String,
            _facility: crate::syslog::Facility,
        ) -> Result<
            (
                Option<Box<dyn crate::syslog::Log + Send>>,
                Option<crate::RawDescriptor>,
            ),
            &'static crate::syslog::Error,
        > {
            // macOS: use stderr logging (no syslog setup needed).
            Ok((None, None))
        }
    }
}

impl PartialEq for crate::SafeDescriptor {
    fn eq(&self, other: &Self) -> bool {
        crate::AsRawDescriptor::as_raw_descriptor(self)
            == crate::AsRawDescriptor::as_raw_descriptor(other)
    }
}

impl crate::shm::PlatformSharedMemory for crate::SharedMemory {
    fn new(debug_name: &std::ffi::CStr, size: u64) -> crate::Result<crate::SharedMemory> {
        use std::os::unix::io::FromRawFd;

        // Use shm_open + ftruncate for POSIX shared memory.
        let name = debug_name.to_str().unwrap_or("aetheria_shm");
        let shm_name = format!("/crosvm_{}", name);
        let c_name = std::ffi::CString::new(shm_name.as_str())
            .map_err(|_| crate::Error::new(libc::EINVAL))?;

        // SAFETY: shm_open creates a new shared memory object.
        let fd = unsafe {
            libc::shm_open(
                c_name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            return Err(crate::Error::last());
        }

        // Unlink immediately so the name slot is freed.
        unsafe { libc::shm_unlink(c_name.as_ptr()) };

        // SAFETY: fd is a valid file descriptor.
        let ret = unsafe { libc::ftruncate(fd, size as libc::off_t) };
        if ret < 0 {
            unsafe { libc::close(fd) };
            return Err(crate::Error::last());
        }

        // SAFETY: fd is a valid open file descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let descriptor = crate::SafeDescriptor::from(file);

        Ok(crate::SharedMemory {
            descriptor,
            size,
        })
    }

    fn from_safe_descriptor(
        descriptor: crate::SafeDescriptor,
        size: u64,
    ) -> crate::Result<crate::SharedMemory> {
        Ok(crate::SharedMemory {
            descriptor,
            size,
        })
    }
}

pub(crate) use libc::off_t;
pub(crate) use libc::pread;
pub(crate) use libc::preadv;
pub(crate) use libc::pwrite;
pub(crate) use libc::pwritev;

/// Spawns a pipe pair where the first pipe is the read end and the second pipe is the write end.
pub fn pipe() -> crate::errno::Result<(File, File)> {
    let mut pipe_fds = [-1; 2];
    // SAFETY: pipe will only write 2 element array of i32.
    let ret = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
    if ret == -1 {
        return crate::errno::errno_result();
    }

    // SAFETY: both fds are valid from pipe().
    let pipes = unsafe {
        (
            File::from_raw_descriptor(pipe_fds[0]),
            File::from_raw_descriptor(pipe_fds[1]),
        )
    };

    set_descriptor_cloexec(&pipes.0)?;
    set_descriptor_cloexec(&pipes.1)?;

    Ok(pipes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;

    #[test]
    fn event_context_kqueue_basic() {
        let evt1 = Event::new().unwrap();
        let evt2 = Event::new().unwrap();

        let ctx: EventContext<u64> =
            EventContext::build_with(&[(&evt1, 1u64), (&evt2, 2u64)]).unwrap();

        // Signal evt1
        evt1.signal().unwrap();

        // Wait should return at least one event
        let events = ctx
            .wait_timeout(std::time::Duration::from_millis(100))
            .unwrap();
        assert!(!events.is_empty(), "expected at least one event");
        assert!(
            events.iter().any(|e| e.token == 1 && e.is_readable),
            "expected evt1 (token=1) to be readable"
        );

        // Consume the event
        evt1.wait().unwrap();

        // Signal evt2
        evt2.signal().unwrap();

        let events = ctx
            .wait_timeout(std::time::Duration::from_millis(100))
            .unwrap();
        assert!(
            events.iter().any(|e| e.token == 2 && e.is_readable),
            "expected evt2 (token=2) to be readable"
        );

        // Delete and verify no events
        evt2.wait().unwrap();
        ctx.delete(&evt1).unwrap();
        ctx.delete(&evt2).unwrap();

        let events = ctx
            .wait_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        assert!(events.is_empty(), "expected no events after delete");
    }
}
