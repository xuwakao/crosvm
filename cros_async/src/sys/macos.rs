// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS platform module for cros_async.
// Provides a kqueue-based async executor and IO source.

pub mod async_types;
mod error;
pub mod event;
pub mod executor;
pub mod kqueue_reactor;
pub mod kqueue_source;
mod timer;

pub use error::AsyncErrorSys;
pub use executor::ExecutorKindSys;
pub(crate) use kqueue_reactor::KqueueReactor;
pub use kqueue_source::KqueueSource;

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
