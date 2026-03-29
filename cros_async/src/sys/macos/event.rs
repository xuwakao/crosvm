// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS stub for cros_async event.

use base::Event;

use crate::AsyncResult;
use crate::EventAsync;
use crate::Executor;

impl EventAsync {
    pub fn new(_event: Event, _ex: &Executor) -> AsyncResult<EventAsync> {
        Err(crate::AsyncError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "async I/O not supported on macOS",
        )))
    }

    pub async fn next_val(&self) -> AsyncResult<u64> {
        unreachable!("EventAsync cannot be constructed on macOS")
    }

    pub async fn next_val_reset(&self) -> AsyncResult<u64> {
        unreachable!("EventAsync cannot be constructed on macOS")
    }
}
