// Copyright 2020 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::Result;
use std::os::unix::io::AsRawFd;

use libc::F_DUPFD_CLOEXEC;

use crate::protocol::P9String;

pub struct DirEntry {
    #[cfg(not(target_os = "macos"))]
    pub ino: libc::ino64_t,
    #[cfg(target_os = "macos")]
    pub ino: libc::ino_t,
    pub offset: u64,
    pub type_: u8,
    pub name: P9String,
}

pub struct ReadDir {
    dir: *mut libc::DIR,
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.dir) };
    }
}

impl ReadDir {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Result<DirEntry>> {
        #[cfg(not(target_os = "macos"))]
        let dirent = unsafe { libc::readdir64(self.dir) };
        #[cfg(target_os = "macos")]
        let dirent = unsafe { libc::readdir(self.dir) };

        if dirent.is_null() {
            return None;
        }

        let (d_ino, d_type) = unsafe { ((*dirent).d_ino, (*dirent).d_type) };

        // d_off: Linux has it in dirent; macOS uses telldir() for position.
        #[cfg(not(target_os = "macos"))]
        let d_off = unsafe { (*dirent).d_off } as u64;
        #[cfg(target_os = "macos")]
        let d_off = unsafe { libc::telldir(self.dir) } as u64;

        let d_name: &[u8] = unsafe { std::mem::transmute((*dirent).d_name.as_ref()) };
        let name = match P9String::new(strip_padding(d_name)) {
            Ok(name) => name,
            Err(e) => return Some(Err(e)),
        };

        let entry = DirEntry {
            ino: d_ino,
            offset: d_off,
            type_: d_type,
            name,
        };

        Some(Ok(entry))
    }
}

pub fn read_dir<D: AsRawFd>(dir: &mut D, offset: libc::c_long) -> Result<ReadDir> {
    let dup_fd = unsafe { libc::fcntl(dir.as_raw_fd(), F_DUPFD_CLOEXEC, 0) };
    let dir = unsafe { libc::fdopendir(dup_fd) };
    if dir.is_null() {
        unsafe { libc::close(dup_fd) };
        return Err(std::io::Error::last_os_error());
    }

    let read_dir = ReadDir { dir };

    unsafe { libc::seekdir(read_dir.dir, offset) };

    Ok(read_dir)
}

fn strip_padding(b: &[u8]) -> &[u8] {
    let pos = b
        .iter()
        .position(|&c| c == 0)
        .expect("`b` doesn't contain any nul bytes");
    &b[..pos]
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn padded_cstrings() {
        assert_eq!(strip_padding(b".\0\0\0\0\0\0\0"), b".");
        assert_eq!(strip_padding(b"..\0\0\0\0\0\0"), b"..");
        assert_eq!(strip_padding(b"normal cstring\0"), b"normal cstring");
        assert_eq!(strip_padding(b"\0\0\0\0"), b"");
        assert_eq!(strip_padding(b"interior\0nul bytes\0\0\0"), b"interior");
    }

    #[test]
    #[should_panic(expected = "`b` doesn't contain any nul bytes")]
    fn no_nul_byte() {
        strip_padding(b"no nul bytes in string");
    }
}
