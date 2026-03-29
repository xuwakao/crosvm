// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS platform module for guest memory.

use base::MappedRegion;
use base::SharedMemory;
use bitflags::bitflags;

use crate::FileBackedMappingParameters;
use crate::GuestMemory;
use crate::MemoryRegion;
use crate::Result;

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    #[repr(transparent)]
    pub struct MemoryPolicy: u32 {
        const USE_HUGEPAGES = 1;
    }
}

pub(crate) fn finalize_shm(_shm: &mut SharedMemory) -> Result<()> {
    // macOS SharedMemory does not support sealing.
    Ok(())
}

impl GuestMemory {
    pub fn set_memory_policy(&self, _mem_policy: MemoryPolicy) {
        // Memory policy hints are best-effort on macOS.
    }
}

impl MemoryRegion {
    pub(crate) fn zero_range(&self, offset: usize, size: usize) -> anyhow::Result<()> {
        // Write zeros to the region.
        // SAFETY: offset + size within mapped region bounds (checked by caller).
        let slice = unsafe {
            std::slice::from_raw_parts_mut(self.mapping.as_ptr().add(offset), size)
        };
        slice.fill(0);
        Ok(())
    }

    pub(crate) fn find_data_ranges(&self) -> anyhow::Result<Vec<std::ops::Range<usize>>> {
        // On macOS, assume the entire region is data (no hole detection like SEEK_DATA on Linux).
        Ok(vec![0..self.mapping.size()])
    }
}

impl FileBackedMappingParameters {
    pub(crate) fn open(&self) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(!self.writable)
            .open(&self.path)
    }
}
