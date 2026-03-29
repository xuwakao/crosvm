// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS stub for udmabuf (not available on macOS).

use base::SafeDescriptor;

use crate::udmabuf::UdmabufDriverTrait;
use crate::udmabuf::UdmabufError;
use crate::udmabuf::UdmabufResult;
use crate::GuestAddress;
use crate::GuestMemory;

pub struct MacosUdmabufDriver;

impl UdmabufDriverTrait for MacosUdmabufDriver {
    fn new() -> UdmabufResult<MacosUdmabufDriver> {
        Err(UdmabufError::UdmabufUnsupported)
    }

    fn create_udmabuf(
        &self,
        _mem: &GuestMemory,
        _iovecs: &[(GuestAddress, usize)],
    ) -> UdmabufResult<SafeDescriptor> {
        Err(UdmabufError::UdmabufUnsupported)
    }
}
