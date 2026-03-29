// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS block device — same as Linux (POSIX-portable).

use std::cmp::max;
use std::cmp::min;

use anyhow::Context;
use cros_async::Executor;
use disk::DiskFile;

use crate::virtio::block::DiskOption;
use crate::virtio::BlockAsync;

pub fn get_seg_max(queue_size: u16) -> u32 {
    // macOS doesn't have iov_max() easily. Use a reasonable default.
    let seg_max = min(max(1024usize, 1), u32::MAX as usize) as u32;
    min(seg_max, u32::from(queue_size) - 2)
}

impl DiskOption {
    pub fn open(&self) -> anyhow::Result<Box<dyn DiskFile>> {
        disk::open_disk_file(disk::DiskFileParams {
            path: self.path.clone(),
            is_read_only: self.read_only,
            is_sparse_file: self.sparse,
            is_overlapped: false,
            is_direct: self.direct,
            lock: self.lock,
            depth: 0,
        })
        .context("open_disk_file failed")
    }
}

impl BlockAsync {
    pub fn create_executor(&self) -> Executor {
        Executor::with_executor_kind(self.executor_kind).expect("Failed to create an executor")
    }
}
