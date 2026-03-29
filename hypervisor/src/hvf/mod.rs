// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// Apple Hypervisor.framework (HVF) backend for crosvm.
// ARM64 / Apple Silicon only.

pub mod ffi;
pub mod vcpu;
pub mod vm;

use std::sync::Arc;

use base::Result;

use crate::Hypervisor;
use crate::HypervisorCap;

/// Guard that calls hv_vm_destroy when the last reference is dropped.
struct HvfVmGuard;

impl Drop for HvfVmGuard {
    fn drop(&mut self) {
        // SAFETY: Called only once when the last Arc<HvfVmGuard> drops.
        // All vCPUs must be destroyed before this point.
        unsafe {
            ffi::hv_vm_destroy();
        }
    }
}

/// Apple Hypervisor.framework instance.
///
/// Uses Arc<HvfVmGuard> to ensure hv_vm_destroy is called exactly once,
/// even when the Hvf instance is cloned (e.g. via try_clone or HvfVm::try_clone).
pub struct Hvf {
    _vm_guard: Arc<HvfVmGuard>,
}

impl Hvf {
    /// Create a new HVF hypervisor instance.
    ///
    /// This calls `hv_vm_create` to initialize the HVF subsystem.
    /// Only one VM per process is allowed by HVF.
    pub fn new() -> Result<Self> {
        // SAFETY: hv_vm_create with NULL config creates a default VM.
        let ret = unsafe { ffi::hv_vm_create(std::ptr::null_mut()) };
        ffi::hvf_result(ret)?;
        Ok(Hvf {
            _vm_guard: Arc::new(HvfVmGuard),
        })
    }
}

impl Hypervisor for Hvf {
    fn try_clone(&self) -> Result<Self> {
        Ok(Hvf {
            _vm_guard: self._vm_guard.clone(),
        })
    }

    fn check_capability(&self, cap: HypervisorCap) -> bool {
        match cap {
            HypervisorCap::UserMemory => true,
            HypervisorCap::ImmediateExit => true,
            _ => false,
        }
    }
}
