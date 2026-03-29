// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS FileAllocate implementation using fcntl(F_PREALLOCATE).

use std::fs::File;
use std::io::Error;
use std::io::Result;
use std::os::unix::io::AsRawFd;

use crate::FileAllocate;

impl FileAllocate for File {
    fn allocate(&self, offset: u64, len: u64) -> Result<()> {
        // macOS uses fcntl(F_PREALLOCATE) + ftruncate instead of fallocate.
        #[repr(C)]
        struct FStore {
            fst_flags: u32,
            fst_posmode: i32,
            fst_offset: i64,
            fst_length: i64,
            fst_bytesalloc: i64,
        }

        let store = FStore {
            fst_flags: 4, // F_ALLOCATEALL
            fst_posmode: 0, // F_PEOFPOSMODE
            fst_offset: offset as i64,
            fst_length: len as i64,
            fst_bytesalloc: 0,
        };

        // SAFETY: valid fd and properly initialized fstore struct.
        let ret = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_PREALLOCATE, &store) };
        if ret < 0 {
            // F_PREALLOCATE can fail on some filesystems. Fall back to ftruncate.
            let new_size = offset + len;
            let ret = unsafe { libc::ftruncate(self.as_raw_fd(), new_size as libc::off_t) };
            if ret < 0 {
                return Err(Error::last_os_error());
            }
        }
        Ok(())
    }
}
