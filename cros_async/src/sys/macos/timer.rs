// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause

use base::TimerTrait;

use crate::AsyncResult;
use crate::IntoAsync;
use crate::TimerAsync;

impl<T: TimerTrait + IntoAsync> TimerAsync<T> {
    pub async fn wait_sys(&self) -> AsyncResult<()> {
        unreachable!("TimerAsync cannot be used on macOS — no async executor")
    }
}
