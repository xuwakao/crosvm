// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS TimerAsync implementation using kqueue executor.

use base::TimerTrait;

use crate::AsyncResult;
use crate::IntoAsync;
use crate::TimerAsync;

impl<T: TimerTrait + IntoAsync> TimerAsync<T> {
    pub async fn wait_sys(&self) -> AsyncResult<()> {
        // Wait for the timer fd to become readable (timer expired).
        let (n, _v) = self
            .io_source
            .read_to_vec(None, 0u64.to_ne_bytes().to_vec())
            .await?;
        if n == 0 {
            return Err(crate::AsyncError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "timer read returned 0 bytes",
            )));
        }
        Ok(())
    }
}
