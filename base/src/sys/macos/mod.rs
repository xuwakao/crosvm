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

pub struct EventContext<T: crate::EventToken> {
    p: std::marker::PhantomData<T>,
}

impl<T: crate::EventToken> EventContext<T> {
    pub fn new() -> crate::errno::Result<EventContext<T>> {
        Ok(EventContext {
            p: std::marker::PhantomData,
        })
    }
    pub fn build_with(
        _fd_tokens: &[(&dyn crate::AsRawDescriptor, T)],
    ) -> crate::errno::Result<EventContext<T>> {
        // TODO: implement kqueue-based event context
        Ok(EventContext {
            p: std::marker::PhantomData,
        })
    }
    pub fn add_for_event(
        &self,
        _descriptor: &dyn crate::AsRawDescriptor,
        _event_type: crate::EventType,
        _token: T,
    ) -> crate::errno::Result<()> {
        // TODO: kevent register
        Ok(())
    }
    pub fn modify(
        &self,
        _fd: &dyn crate::AsRawDescriptor,
        _event_type: crate::EventType,
        _token: T,
    ) -> crate::errno::Result<()> {
        // TODO: kevent modify
        Ok(())
    }
    pub fn delete(&self, _fd: &dyn crate::AsRawDescriptor) -> crate::errno::Result<()> {
        // TODO: kevent delete
        Ok(())
    }
    pub fn wait(&self) -> crate::errno::Result<smallvec::SmallVec<[crate::TriggeredEvent<T>; 16]>> {
        // TODO: kevent wait
        Ok(smallvec::SmallVec::new())
    }
    pub fn wait_timeout(
        &self,
        _timeout: std::time::Duration,
    ) -> crate::errno::Result<smallvec::SmallVec<[crate::TriggeredEvent<T>; 16]>> {
        // TODO: kevent wait with timeout
        Ok(smallvec::SmallVec::new())
    }
}

impl<T: crate::EventToken> crate::AsRawDescriptor for EventContext<T> {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        // TODO: return kqueue fd
        -1
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
