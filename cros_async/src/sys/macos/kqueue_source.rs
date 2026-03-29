// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// Async IO source for macOS kqueue, equivalent to Linux's PollSource.
// Wraps a file descriptor and provides async read/write using kqueue readiness notification.

use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use base::handle_eintr_errno;
use base::AsRawDescriptor;
use base::VolatileSlice;

use super::kqueue_reactor::KqueueReactor;
use super::kqueue_reactor::RegisteredSource;
use crate::common_executor::RawExecutor;
use crate::mem::BackingMemory;
use crate::AsyncError;
use crate::AsyncResult;
use crate::MemRegion;

/// Async wrapper for an IO source that uses the kqueue executor to drive async operations.
pub struct KqueueSource<F> {
    registered_source: RegisteredSource<F>,
}

impl<F: AsRawDescriptor> KqueueSource<F> {
    pub fn new(f: F, ex: &Arc<RawExecutor<KqueueReactor>>) -> io::Result<Self> {
        RegisteredSource::new(ex, f).map(|f| KqueueSource {
            registered_source: f,
        })
    }
}

impl<F: AsRawDescriptor> KqueueSource<F> {
    /// Reads from the source at `file_offset` and fills the given `vec`.
    pub async fn read_to_vec(
        &self,
        file_offset: Option<u64>,
        mut vec: Vec<u8>,
    ) -> AsyncResult<(usize, Vec<u8>)> {
        loop {
            let res = if let Some(offset) = file_offset {
                // SAFETY: pread is safe with valid fd and buffer.
                handle_eintr_errno!(unsafe {
                    libc::pread(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_mut_ptr() as *mut libc::c_void,
                        vec.len(),
                        offset as libc::off_t,
                    )
                })
            } else {
                // SAFETY: read is safe with valid fd and buffer.
                handle_eintr_errno!(unsafe {
                    libc::read(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_mut_ptr() as *mut libc::c_void,
                        vec.len(),
                    )
                })
            };

            if res >= 0 {
                return Ok((res as usize, vec));
            }

            match base::Error::last().errno() {
                libc::EWOULDBLOCK => {
                    self.registered_source
                        .wait_readable()
                        .map_err(AsyncError::Io)?
                        .await
                        .map_err(AsyncError::Io)?;
                }
                e => {
                    return Err(AsyncError::Io(io::Error::from_raw_os_error(e)));
                }
            }
        }
    }

    /// Writes from the given `vec` to the source at `file_offset`.
    pub async fn write_from_vec(
        &self,
        file_offset: Option<u64>,
        vec: Vec<u8>,
    ) -> AsyncResult<(usize, Vec<u8>)> {
        loop {
            let res = if let Some(offset) = file_offset {
                // SAFETY: pwrite is safe with valid fd and buffer.
                handle_eintr_errno!(unsafe {
                    libc::pwrite(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_ptr() as *const libc::c_void,
                        vec.len(),
                        offset as libc::off_t,
                    )
                })
            } else {
                // SAFETY: write is safe with valid fd and buffer.
                handle_eintr_errno!(unsafe {
                    libc::write(
                        self.registered_source.duped_fd.as_raw_fd(),
                        vec.as_ptr() as *const libc::c_void,
                        vec.len(),
                    )
                })
            };

            if res >= 0 {
                return Ok((res as usize, vec));
            }

            match base::Error::last().errno() {
                libc::EWOULDBLOCK => {
                    self.registered_source
                        .wait_writable()
                        .map_err(AsyncError::Io)?
                        .await
                        .map_err(AsyncError::Io)?;
                }
                e => {
                    return Err(AsyncError::Io(io::Error::from_raw_os_error(e)));
                }
            }
        }
    }

    /// Reads to the given `mem` at the given offsets from the file starting at `file_offset`.
    pub async fn read_to_mem(
        &self,
        file_offset: Option<u64>,
        mem: Arc<dyn BackingMemory + Send + Sync>,
        mem_offsets: impl IntoIterator<Item = MemRegion>,
    ) -> AsyncResult<usize> {
        let mem_offsets: Vec<MemRegion> = mem_offsets.into_iter().collect();
        let mut total = 0usize;
        for region in &mem_offsets {
            let slice = mem
                .get_volatile_slice(MemRegion {
                    offset: region.offset,
                    len: region.len,
                })
                .map_err(|e| AsyncError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
            loop {
                let offset = file_offset.map(|o| o + total as u64);
                let res = if let Some(off) = offset {
                    handle_eintr_errno!(unsafe {
                        libc::pread(
                            self.registered_source.duped_fd.as_raw_fd(),
                            slice.as_mut_ptr() as *mut libc::c_void,
                            slice.size(),
                            off as libc::off_t,
                        )
                    })
                } else {
                    handle_eintr_errno!(unsafe {
                        libc::read(
                            self.registered_source.duped_fd.as_raw_fd(),
                            slice.as_mut_ptr() as *mut libc::c_void,
                            slice.size(),
                        )
                    })
                };

                if res >= 0 {
                    total += res as usize;
                    break;
                }
                match base::Error::last().errno() {
                    libc::EWOULDBLOCK => {
                        self.registered_source
                        .wait_readable()
                        .map_err(AsyncError::Io)?
                        .await
                        .map_err(AsyncError::Io)?;
                    }
                    e => {
                        return Err(AsyncError::Io(io::Error::from_raw_os_error(e)));
                    }
                }
            }
        }
        Ok(total)
    }

    /// Writes from the given `mem` at the given offsets to the file starting at `file_offset`.
    pub async fn write_from_mem(
        &self,
        file_offset: Option<u64>,
        mem: Arc<dyn BackingMemory + Send + Sync>,
        mem_offsets: impl IntoIterator<Item = MemRegion>,
    ) -> AsyncResult<usize> {
        let mem_offsets: Vec<MemRegion> = mem_offsets.into_iter().collect();
        let mut total = 0usize;
        for region in &mem_offsets {
            let slice = mem
                .get_volatile_slice(MemRegion {
                    offset: region.offset,
                    len: region.len,
                })
                .map_err(|e| AsyncError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
            loop {
                let offset = file_offset.map(|o| o + total as u64);
                let res = if let Some(off) = offset {
                    handle_eintr_errno!(unsafe {
                        libc::pwrite(
                            self.registered_source.duped_fd.as_raw_fd(),
                            slice.as_ptr() as *const libc::c_void,
                            slice.size(),
                            off as libc::off_t,
                        )
                    })
                } else {
                    handle_eintr_errno!(unsafe {
                        libc::write(
                            self.registered_source.duped_fd.as_raw_fd(),
                            slice.as_ptr() as *const libc::c_void,
                            slice.size(),
                        )
                    })
                };

                if res >= 0 {
                    total += res as usize;
                    break;
                }
                match base::Error::last().errno() {
                    libc::EWOULDBLOCK => {
                        self.registered_source
                        .wait_writable()
                        .map_err(AsyncError::Io)?
                        .await
                        .map_err(AsyncError::Io)?;
                    }
                    e => {
                        return Err(AsyncError::Io(io::Error::from_raw_os_error(e)));
                    }
                }
            }
        }
        Ok(total)
    }

    /// Syncs all completed write operations to the backing storage.
    pub async fn fsync(&self) -> AsyncResult<()> {
        let fd = self.registered_source.duped_fd.as_raw_fd();
        let ret = unsafe { libc::fsync(fd) };
        if ret < 0 {
            Err(AsyncError::Io(io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    /// Syncs all data (not metadata) to the backing storage.
    pub async fn fdatasync(&self) -> AsyncResult<()> {
        // macOS doesn't have fdatasync; use fcntl(F_FULLFSYNC) for data integrity.
        let fd = self.registered_source.duped_fd.as_raw_fd();
        let ret = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if ret < 0 {
            // Fall back to fsync if F_FULLFSYNC not supported.
            let ret = unsafe { libc::fsync(fd) };
            if ret < 0 {
                return Err(AsyncError::Io(io::Error::last_os_error()));
            }
        }
        Ok(())
    }

    /// Yields the underlying IO source.
    pub fn into_source(self) -> F {
        self.registered_source.source
    }

    /// Provides a ref to the underlying IO source.
    pub fn as_source(&self) -> &F {
        &self.registered_source.source
    }

    /// Provides a mutable ref to the underlying IO source.
    pub fn as_source_mut(&mut self) -> &mut F {
        &mut self.registered_source.source
    }

    pub async fn wait_readable(&self) -> AsyncResult<()> {
        self.registered_source
            .wait_readable()
            .map_err(AsyncError::Io)?
            .await
            .map_err(AsyncError::Io)?;
        Ok(())
    }

    pub async fn punch_hole(&self, file_offset: u64, len: u64) -> AsyncResult<()> {
        // macOS: use fcntl(F_PUNCHHOLE)
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
            fp_offset: file_offset,
            fp_length: len,
        };
        let ret = unsafe {
            libc::fcntl(
                self.registered_source.duped_fd.as_raw_fd(),
                libc::F_PUNCHHOLE,
                &args,
            )
        };
        if ret < 0 {
            Err(AsyncError::Io(io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    pub async fn write_zeroes_at(&self, file_offset: u64, len: u64) -> AsyncResult<()> {
        let zeros = vec![0u8; len as usize];
        let ret = unsafe {
            libc::pwrite(
                self.registered_source.duped_fd.as_raw_fd(),
                zeros.as_ptr() as *const libc::c_void,
                zeros.len(),
                file_offset as libc::off_t,
            )
        };
        if ret < 0 {
            Err(AsyncError::Io(io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }
}
