// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS platform module for cros_async.
// This is a minimal stub to allow compilation on macOS.
// Full async I/O (kqueue-based) implementation is future work.

pub mod async_types;
mod error;
pub mod event;
pub mod executor;
mod timer;

pub use error::AsyncErrorSys;
pub use executor::ExecutorKindSys;

use crate::Error;

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        use Error::*;
        match e {
            EventAsync(e) => e.into(),
            Io(e) => e,
            Timer(e) => e.into(),
            TimerAsync(e) => e.into(),
        }
    }
}
