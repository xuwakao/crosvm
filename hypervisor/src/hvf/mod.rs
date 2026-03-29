// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// Apple Hypervisor.framework (HVF) backend for crosvm.
// ARM64 / Apple Silicon only.

pub mod ffi;
pub mod vcpu;
pub mod vm;

use base::Result;

use crate::Hypervisor;
use crate::HypervisorCap;

/// Apple Hypervisor.framework instance.
pub struct Hvf;

impl Hvf {
    /// Create a new HVF hypervisor instance.
    ///
    /// This calls `hv_vm_create` to initialize the HVF subsystem.
    /// Only one VM per process is allowed by HVF.
    pub fn new() -> Result<Self> {
        // SAFETY: hv_vm_create with NULL config creates a default VM.
        let ret = unsafe { ffi::hv_vm_create(std::ptr::null_mut()) };
        ffi::hvf_result(ret)?;
        Ok(Hvf)
    }
}

impl Hypervisor for Hvf {
    fn try_clone(&self) -> Result<Self> {
        // HVF is per-process singleton. Cloning is a no-op — just return another handle.
        Ok(Hvf)
    }

    fn check_capability(&self, cap: HypervisorCap) -> bool {
        match cap {
            HypervisorCap::UserMemory => true,
            HypervisorCap::ImmediateExit => true,
            _ => false,
        }
    }
}

impl Drop for Hvf {
    fn drop(&mut self) {
        // SAFETY: Destroying the VM. All vCPUs must be destroyed before this.
        unsafe {
            ffi::hv_vm_destroy();
        }
    }
}
