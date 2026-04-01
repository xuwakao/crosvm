// Copyright 2020 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::CStr;
use std::io;
use std::ops::Deref;
use std::ops::DerefMut;

use base::AsRawDescriptor;
use fuse::filesystem::DirEntry;
use fuse::filesystem::DirectoryIterator;

// ============================================================================
// Linux implementation: uses SYS_getdents64 for raw directory reading
// ============================================================================
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;
    use std::mem::size_of;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    #[repr(C, packed)]
    #[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
    struct LinuxDirent64 {
        d_ino: libc::ino64_t,
        d_off: libc::off64_t,
        d_reclen: libc::c_ushort,
        d_ty: libc::c_uchar,
    }

    pub struct ReadDir<P> {
        buf: P,
        current: usize,
        end: usize,
    }

    impl<P: DerefMut<Target = [u8]>> ReadDir<P> {
        pub fn new<D: AsRawDescriptor>(dir: &D, offset: libc::off64_t, mut buf: P) -> io::Result<Self> {
            let res = unsafe { libc::lseek64(dir.as_raw_descriptor(), offset, libc::SEEK_SET) };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }

            let res = unsafe {
                libc::syscall(
                    libc::SYS_getdents64,
                    dir.as_raw_descriptor(),
                    buf.as_mut_ptr() as *mut LinuxDirent64,
                    buf.len() as libc::c_int,
                )
            };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(ReadDir {
                buf,
                current: 0,
                end: res as usize,
            })
        }
    }

    impl<P> ReadDir<P> {
        pub fn remaining(&self) -> usize {
            self.end.saturating_sub(self.current)
        }
    }

    impl<P: Deref<Target = [u8]>> DirectoryIterator for ReadDir<P> {
        fn next(&mut self) -> Option<DirEntry> {
            let rem = &self.buf[self.current..self.end];
            if rem.is_empty() {
                return None;
            }

            let (dirent64, back) =
                LinuxDirent64::read_from_prefix(rem).expect("unable to get LinuxDirent64 from slice");

            let namelen = dirent64.d_reclen as usize - size_of::<LinuxDirent64>();
            debug_assert!(namelen <= back.len(), "back is smaller than `namelen`");

            let name = strip_padding(&back[..namelen]);
            let entry = DirEntry {
                ino: dirent64.d_ino,
                offset: dirent64.d_off as u64,
                type_: dirent64.d_ty as u32,
                name,
            };

            debug_assert!(
                rem.len() >= dirent64.d_reclen as usize,
                "rem is smaller than `d_reclen`"
            );
            self.current += dirent64.d_reclen as usize;
            Some(entry)
        }
    }
}

// ============================================================================
// macOS implementation: uses fdopendir + readdir (no getdents64 on macOS)
// ============================================================================
#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::os::unix::io::AsRawFd;

    pub struct ReadDir<P> {
        dir: *mut libc::DIR,
        _buf: P, // Keep buffer alive (not used on macOS, but maintains API compat)
    }

    impl<P> Drop for ReadDir<P> {
        fn drop(&mut self) {
            if !self.dir.is_null() {
                unsafe { libc::closedir(self.dir) };
            }
        }
    }

    impl<P: DerefMut<Target = [u8]>> ReadDir<P> {
        pub fn new<D: AsRawDescriptor>(dir: &D, offset: libc::off_t, buf: P) -> io::Result<Self> {
            let dup_fd = unsafe { libc::fcntl(dir.as_raw_descriptor(), libc::F_DUPFD_CLOEXEC, 0) };
            if dup_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let dirp = unsafe { libc::fdopendir(dup_fd) };
            if dirp.is_null() {
                unsafe { libc::close(dup_fd) };
                return Err(io::Error::last_os_error());
            }
            unsafe { libc::seekdir(dirp, offset) };

            Ok(ReadDir { dir: dirp, _buf: buf })
        }
    }

    impl<P> ReadDir<P> {
        pub fn remaining(&self) -> usize {
            1 // macOS: can't cheaply tell remaining; return nonzero to indicate "maybe more"
        }
    }

    impl<P> DirectoryIterator for ReadDir<P> {
        fn next(&mut self) -> Option<DirEntry> {
            let dirent = unsafe { libc::readdir(self.dir) };
            if dirent.is_null() {
                return None;
            }

            let (d_ino, d_type) = unsafe { ((*dirent).d_ino, (*dirent).d_type) };
            let d_off = unsafe { libc::telldir(self.dir) } as u64;

            // Get name via strlen (POSIX guarantees null termination from readdir).
            let name = unsafe {
                let ptr = (*dirent).d_name.as_ptr();
                let len = libc::strlen(ptr as *const libc::c_char);
                let bytes = std::slice::from_raw_parts(ptr as *const u8, len + 1); // include nul
                CStr::from_bytes_with_nul_unchecked(bytes)
            };

            Some(DirEntry {
                ino: d_ino,
                offset: d_off,
                type_: d_type as u32,
                name,
            })
        }
    }
}

pub use platform::ReadDir;

// Like `CStr::from_bytes_with_nul` but strips any bytes after the first '\0'-byte.
#[cfg(not(target_os = "macos"))]
fn strip_padding(b: &[u8]) -> &CStr {
    let pos = b
        .iter()
        .position(|&c| c == 0)
        .expect("`b` doesn't contain any nul bytes");

    unsafe { CStr::from_bytes_with_nul_unchecked(&b[..pos + 1]) }
}

#[cfg(test)]
mod test {
    #[cfg(not(target_os = "macos"))]
    use super::strip_padding;

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn padded_cstrings() {
        assert_eq!(strip_padding(b".\0\0\0\0\0\0\0").to_bytes(), b".");
        assert_eq!(strip_padding(b"..\0\0\0\0\0\0").to_bytes(), b"..");
        assert_eq!(
            strip_padding(b"normal cstring\0").to_bytes(),
            b"normal cstring"
        );
        assert_eq!(strip_padding(b"\0\0\0\0").to_bytes(), b"");
        assert_eq!(
            strip_padding(b"interior\0nul bytes\0\0\0").to_bytes(),
            b"interior"
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    #[should_panic(expected = "`b` doesn't contain any nul bytes")]
    fn no_nul_byte() {
        strip_padding(b"no nul bytes in string");
    }
}
