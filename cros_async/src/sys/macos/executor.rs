// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS stub for cros_async executor.

use serde::Deserialize;
use serde::Serialize;

/// Stub executor kind for macOS. Currently no async I/O backend is implemented.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, serde_keyvalue::FromKeyValues,
)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum ExecutorKindSys {
    Kqueue,
}
