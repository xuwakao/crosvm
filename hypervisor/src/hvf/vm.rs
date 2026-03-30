// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// HVF Vm and VmAArch64 trait implementations.

use std::collections::BTreeMap;
use std::sync::Arc;

use base::Event;
use base::MappedRegion;
use base::Protection;
use base::Result;
use base::SafeDescriptor;
use cros_fdt::Fdt;
use fnv::FnvHashMap;
use sync::Mutex;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

use super::ffi;
use super::ffi::hvf_result;
use super::Hvf;
use crate::aarch64::*;
use crate::BalloonEvent;
use crate::ClockState;
use crate::Datamatch;
use crate::DeviceKind;
use crate::Hypervisor;
use crate::HypervisorKind;
use crate::IoEventAddress;
use crate::MemCacheType;
use crate::MemSlot;
use crate::Vm;
use crate::VmCap;

/// Apple Hypervisor.framework VM instance.
pub struct HvfVm {
    hvf: Hvf,
    guest_mem: GuestMemory,
    mem_regions: Arc<Mutex<BTreeMap<MemSlot, (GuestAddress, Box<dyn MappedRegion>)>>>,
    next_mem_slot: Arc<Mutex<MemSlot>>,
    ioevents: Arc<Mutex<FnvHashMap<IoEventAddress, Event>>>,
}

impl HvfVm {
    /// Create a new HVF VM and map all guest memory regions.
    /// The VM is already created by `Hvf::new()` (hv_vm_create).
    pub fn new(hvf: Hvf, guest_mem: GuestMemory) -> Result<Self> {
        let flags = ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC;

        // Map all guest memory regions into the HVF VM.
        for region in guest_mem.regions() {
            let host_addr = region.host_addr as *const std::ffi::c_void;
            let guest_addr = region.guest_addr.0;
            let size = region.size;

            base::info!(
                "HvfVm::new mapping guest memory: guest={:#x} size={:#x} host={:p}",
                guest_addr, size, host_addr
            );

            let ret = unsafe { ffi::hv_vm_map(host_addr, guest_addr, size, flags) };
            if ret != ffi::HV_SUCCESS {
                base::error!("hv_vm_map failed for guest={:#x}: ret={}", guest_addr, ret);
                return Err(base::Error::new(ret));
            }
        }

        Ok(HvfVm {
            hvf,
            guest_mem,
            mem_regions: Arc::new(Mutex::new(BTreeMap::new())),
            next_mem_slot: Arc::new(Mutex::new(0)),
            ioevents: Arc::new(Mutex::new(FnvHashMap::default())),
        })
    }
}

impl Vm for HvfVm {
    fn try_clone(&self) -> Result<Self> {
        Ok(HvfVm {
            hvf: self.hvf.try_clone()?,
            guest_mem: self.guest_mem.clone(),
            mem_regions: self.mem_regions.clone(),
            next_mem_slot: self.next_mem_slot.clone(),
            ioevents: self.ioevents.clone(),
        })
    }

    fn try_clone_descriptor(&self) -> Result<SafeDescriptor> {
        // HVF is per-process singleton, no fd to clone.
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn hypervisor_kind(&self) -> HypervisorKind {
        HypervisorKind::Hvf
    }

    fn check_capability(&self, c: VmCap) -> bool {
        match c {
            VmCap::DirtyLog => false,
            VmCap::PvClock => false,
            VmCap::Protected => false,
            VmCap::EarlyInitCpuid => false,
            VmCap::ReadOnlyMemoryRegion => true,
            VmCap::MemNoncoherentDma => false,
            #[cfg(target_arch = "aarch64")]
            VmCap::ArmPmuV3 => false,
            #[cfg(target_arch = "aarch64")]
            VmCap::Sve => false,
        }
    }

    fn get_guest_phys_addr_bits(&self) -> u8 {
        // Apple Silicon supports 36-bit IPA (64GB guest physical address space).
        // Some chips support 40-bit. Use 36 as safe default.
        36
    }

    fn get_memory(&self) -> &GuestMemory {
        &self.guest_mem
    }

    fn add_memory_region(
        &mut self,
        guest_addr: GuestAddress,
        mem_region: Box<dyn MappedRegion>,
        read_only: bool,
        _log_dirty_pages: bool,
        _cache: MemCacheType,
    ) -> Result<MemSlot> {
        let size = mem_region.size();
        let host_addr = mem_region.as_ptr() as *const std::ffi::c_void;

        let mut flags = ffi::HV_MEMORY_READ;
        if !read_only {
            flags |= ffi::HV_MEMORY_WRITE;
        }
        flags |= ffi::HV_MEMORY_EXEC;

        base::info!(
            "HvfVm::add_memory_region: guest={:#x} size={:#x} host={:p} flags={:#x}",
            guest_addr.0, size, host_addr, flags
        );

        // SAFETY: host_addr points to a valid mapped region of `size` bytes.
        let ret = unsafe { ffi::hv_vm_map(host_addr, guest_addr.0, size, flags) };
        if ret != ffi::HV_SUCCESS {
            base::error!("hv_vm_map failed: ret={}", ret);
        }
        hvf_result(ret)?;

        let mut slot_lock = self.next_mem_slot.lock();
        let slot = *slot_lock;
        *slot_lock += 1;

        self.mem_regions
            .lock()
            .insert(slot, (guest_addr, mem_region));

        Ok(slot)
    }

    fn msync_memory_region(&mut self, _slot: MemSlot, _offset: usize, _size: usize) -> Result<()> {
        // No-op on HVF — memory is directly mapped.
        Ok(())
    }

    fn remove_memory_region(&mut self, slot: MemSlot) -> Result<Box<dyn MappedRegion>> {
        let (guest_addr, mem_region) = self
            .mem_regions
            .lock()
            .remove(&slot)
            .ok_or_else(|| base::Error::new(libc::EINVAL))?;

        let ret = unsafe { ffi::hv_vm_unmap(guest_addr.0, mem_region.size()) };
        hvf_result(ret)?;

        Ok(mem_region)
    }

    fn create_device(&self, _kind: DeviceKind) -> Result<SafeDescriptor> {
        // HVF doesn't have kernel-side device creation like KVM.
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn get_dirty_log(&self, _slot: MemSlot, _dirty_log: &mut [u8]) -> Result<()> {
        // HVF doesn't support dirty page tracking.
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn register_ioevent(
        &mut self,
        evt: &Event,
        addr: IoEventAddress,
        _datamatch: Datamatch,
    ) -> Result<()> {
        let evt_clone = evt.try_clone()?;
        self.ioevents.lock().insert(addr, evt_clone);
        Ok(())
    }

    fn unregister_ioevent(
        &mut self,
        _evt: &Event,
        addr: IoEventAddress,
        _datamatch: Datamatch,
    ) -> Result<()> {
        self.ioevents.lock().remove(&addr);
        Ok(())
    }

    fn handle_io_events(&self, addr: IoEventAddress, _data: &[u8]) -> Result<()> {
        if let Some(evt) = self.ioevents.lock().get(&addr) {
            evt.signal()?;
        }
        Ok(())
    }

    fn get_pvclock(&self) -> Result<ClockState> {
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn set_pvclock(&self, _state: &ClockState) -> Result<()> {
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn add_fd_mapping(
        &mut self,
        _slot: u32,
        _offset: usize,
        _size: usize,
        _fd: &dyn base::AsRawDescriptor,
        _fd_offset: u64,
        _prot: Protection,
    ) -> Result<()> {
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn remove_mapping(&mut self, _slot: u32, _offset: usize, _size: usize) -> Result<()> {
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn handle_balloon_event(&mut self, _event: BalloonEvent) -> Result<()> {
        // TODO: implement balloon support
        Ok(())
    }

    fn enable_hypercalls(&mut self, _nr: u64, _count: usize) -> Result<()> {
        // HVF handles hypercalls via VM exit, no pre-registration needed.
        Ok(())
    }
}

impl VmAArch64 for HvfVm {
    fn get_hypervisor(&self) -> &dyn Hypervisor {
        &self.hvf
    }

    fn load_protected_vm_firmware(
        &mut self,
        _fw_addr: GuestAddress,
        _fw_max_size: u64,
    ) -> Result<()> {
        Err(base::Error::new(libc::ENOTSUP))
    }

    fn create_vcpu(&self, id: usize) -> Result<Box<dyn VcpuAArch64>> {
        let vcpu = super::vcpu::HvfVcpu::new(id)?;
        Ok(Box::new(vcpu))
    }

    fn create_fdt(
        &self,
        _fdt: &mut Fdt,
        _phandles: &BTreeMap<&str, u32>,
    ) -> cros_fdt::Result<()> {
        // Minimal FDT — crosvm's arch layer handles most of it.
        Ok(())
    }

    fn init_arch(
        &self,
        _payload_entry_address: GuestAddress,
        _fdt_address: GuestAddress,
        _fdt_size: usize,
    ) -> anyhow::Result<()> {
        // HVF doesn't need special arch init — vCPU registers are set directly.
        Ok(())
    }
}
