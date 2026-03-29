// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS AsyncTube implementation using kqueue executor.

use base::Tube;
use base::TubeResult;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Executor;
use crate::IoSource;

pub struct AsyncTube {
    inner: IoSource<Tube>,
}

impl AsyncTube {
    pub fn new(ex: &Executor, tube: Tube) -> std::io::Result<AsyncTube> {
        let inner = ex.async_from(tube).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("async_from failed: {e}"))
        })?;
        Ok(AsyncTube { inner })
    }

    pub async fn next<D: DeserializeOwned>(&self) -> TubeResult<D> {
        self.inner.wait_readable().await.map_err(|e| {
            base::TubeError::Recv(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("wait_readable: {e}"),
            ))
        })?;
        self.inner.as_source().recv()
    }

    pub async fn send<M: Serialize>(&self, msg: M) -> TubeResult<()> {
        self.inner.as_source().send(&msg)
    }
}
