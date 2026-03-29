// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS stub for cros_async async types.
// These types exist for compilation but cannot be constructed on macOS
// since there is no async executor backend.

use std::io;

use base::Tube;
use base::TubeResult;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Executor;

pub struct AsyncTube {
    _tube: Tube,
}

impl AsyncTube {
    pub fn new(_ex: &Executor, _tube: Tube) -> io::Result<AsyncTube> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "async I/O not supported on macOS",
        ))
    }

    pub async fn next<D: DeserializeOwned>(&self) -> TubeResult<D> {
        unreachable!("AsyncTube cannot be constructed on macOS")
    }

    pub async fn send<M: Serialize>(&self, _msg: M) -> TubeResult<()> {
        unreachable!("AsyncTube cannot be constructed on macOS")
    }
}
