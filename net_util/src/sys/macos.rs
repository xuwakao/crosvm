// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS network stub — TAP devices are Linux-specific.
// macOS networking will need a different approach (utun/vmnet).

use crate::TapTCommon;

/// macOS TAP trait — placeholder. Real implementation will use utun or vmnet.
pub trait TapT: TapTCommon {}
