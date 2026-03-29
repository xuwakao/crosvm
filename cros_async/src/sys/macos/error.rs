// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum AsyncErrorSys {
    #[error("macOS async I/O not yet implemented: {0}")]
    Unimplemented(io::Error),
}

impl From<AsyncErrorSys> for io::Error {
    fn from(err: AsyncErrorSys) -> Self {
        match err {
            AsyncErrorSys::Unimplemented(e) => e,
        }
    }
}
