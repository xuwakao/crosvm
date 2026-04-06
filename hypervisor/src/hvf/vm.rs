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
/// Tracks a DAX file mapping in guest address space.
struct DaxMapping {
    host_addr: *mut libc::c_void,
    guest_addr: u64,
    size: usize,
    /// If true, host_addr was obtained via mmap and must be munmap'd on cleanup.
    /// False for Venus zero-copy pointers (owned by MoltenVK vkMapMemory).
    mmap_owned: bool,
}
// SAFETY: DaxMapping contains a raw pointer to mmap'd memory that is only
// accessed within the VM's memory management methods (add_fd_mapping/remove_mapping).
unsafe impl Send for DaxMapping {}

pub struct HvfVm {
    hvf: Hvf,
    guest_mem: GuestMemory,
    mem_regions: Arc<Mutex<BTreeMap<MemSlot, (GuestAddress, Box<dyn MappedRegion>)>>>,
    next_mem_slot: Arc<Mutex<MemSlot>>,
    ioevents: Arc<Mutex<FnvHashMap<IoEventAddress, Event>>>,
    dax_mappings: Arc<Mutex<BTreeMap<(u32, usize), DaxMapping>>>,
    // Slots where hv_vm_map was actually called (not skipped for DAX windows).
    // Used by remove_memory_region to avoid unmapping regions that were never mapped.
    mapped_slots: Arc<Mutex<std::collections::HashSet<MemSlot>>>,
}

impl HvfVm {
    /// Create a new HVF VM and map all guest memory regions.
    /// The VM is already created by `Hvf::new()` (hv_vm_create).
    /// `gic_dist_base`: Guest physical address for GIC distributor (e.g. 0x3FFF0000).
    /// If native HVF GIC is available (macOS 15+), it will be created at this address.
    pub fn new(hvf: Hvf, guest_mem: GuestMemory, gic_dist_base: u64) -> Result<Self> {
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

        // Create HVF native GIC if available (macOS 15+).
        // Must be done after hv_vm_create but before hv_vcpu_create.
        if ffi::hvf_gic_is_available() {
            // Query required alignment
            type AlignFn = unsafe extern "C" fn(*mut usize) -> ffi::hv_return_t;
            let get_dist_align: Option<AlignFn> = unsafe {
                let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_get_distributor_base_alignment\0".as_ptr() as *const _);
                if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
            };
            let get_redist_align: Option<AlignFn> = unsafe {
                let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_get_redistributor_base_alignment\0".as_ptr() as *const _);
                if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
            };

            let mut dist_align: usize = 0x10000;
            let mut redist_align: usize = 0x10000;
            if let Some(f) = get_dist_align {
                unsafe { f(&mut dist_align) };
            }
            if let Some(f) = get_redist_align {
                unsafe { f(&mut redist_align) };
            }
            // Also query redistributor sizes
            let mut redist_region_size: usize = 0;
            let mut redist_per_cpu_size: usize = 0;
            let mut spi_base: u32 = 0;
            let mut spi_count: u32 = 0;
            unsafe {
                let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_get_redistributor_region_size\0".as_ptr() as *const _);
                if !sym.is_null() {
                    let f: unsafe extern "C" fn(*mut usize) -> ffi::hv_return_t = std::mem::transmute(sym);
                    f(&mut redist_region_size);
                }
                let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_get_redistributor_size\0".as_ptr() as *const _);
                if !sym.is_null() {
                    let f: unsafe extern "C" fn(*mut usize) -> ffi::hv_return_t = std::mem::transmute(sym);
                    f(&mut redist_per_cpu_size);
                }
                let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_get_spi_interrupt_range\0".as_ptr() as *const _);
                if !sym.is_null() {
                    let f: unsafe extern "C" fn(*mut u32, *mut u32) -> ffi::hv_return_t = std::mem::transmute(sym);
                    f(&mut spi_base, &mut spi_count);
                }
            }
            base::info!(
                "HVF GIC params: dist_align={:#x} redist_align={:#x} redist_region={:#x} redist_per_cpu={:#x} spi_base={} spi_count={}",
                dist_align, redist_align, redist_region_size, redist_per_cpu_size, spi_base, spi_count
            );

            // Place GIC regions to avoid overlap.
            // Redistributor needs redist_region_size (32MB typically).
            // Put GICD at the top, GICR below with enough space.
            let gic_dist_base: u64 = gic_dist_base & !(dist_align as u64 - 1);
            let gic_redist_base: u64 = (gic_dist_base - redist_region_size as u64) & !(redist_align as u64 - 1);
            base::info!("HVF GIC: GICD@{:#x} GICR@{:#x}", gic_dist_base, gic_redist_base);

            let config = unsafe { ffi::hv_gic_config_create() };
            if config.is_null() {
                base::error!("hv_gic_config_create returned null");
            } else {
                let ret = unsafe { ffi::hv_gic_config_set_distributor_base(config, gic_dist_base) };
                if ret != ffi::HV_SUCCESS {
                    base::error!("hv_gic_config_set_distributor_base({:#x}) failed: {}", gic_dist_base, ret);
                }
                let ret = unsafe { ffi::hv_gic_config_set_redistributor_base(config, gic_redist_base) };
                if ret != ffi::HV_SUCCESS {
                    base::error!("hv_gic_config_set_redistributor_base({:#x}) failed: {}", gic_redist_base, ret);
                }
                let ret = unsafe { ffi::hv_gic_create(config) };
                if ret != ffi::HV_SUCCESS {
                    base::error!("hv_gic_create failed: {} (HV_BAD_ARGUMENT={})", ret, ffi::HV_BAD_ARGUMENT);
                } else {
                    base::info!("HVF native GIC created successfully");
                }
                // Release the config object (OS_OBJECT type, ref-counted).
                // SAFETY: config is a valid os_object returned by hv_gic_config_create.
                extern "C" { fn os_release(object: *mut std::ffi::c_void); }
                unsafe { os_release(config) };

                if ret == ffi::HV_SUCCESS {
                    // Enable the GIC distributor: write GICD_CTLR (offset 0x0000)
                    // with EnableGrp1NS (bit 1) = 1. This allows Group 1 Non-Secure
                    // interrupts to be forwarded to the CPU interface.
                    const GICD_CTLR: u16 = 0x0000;
                    const GICD_CTLR_ENABLE_GRP1_NS: u64 = 0x2;
                    let ret = unsafe { ffi::hv_gic_set_distributor_reg(GICD_CTLR, GICD_CTLR_ENABLE_GRP1_NS) };
                    if ret != ffi::HV_SUCCESS {
                        base::warn!("hv_gic_set_distributor_reg(CTLR, EnableGrp1NS) failed: {}", ret);
                    } else {
                        base::info!("GIC distributor EnableGrp1NS set");
                    }

                    // Configure ALL SPIs as edge-triggered via GICD_ICFGRn.
                    // GICD_ICFGR2+ covers SPIs (INTID 32+). Each register covers
                    // 16 interrupts, 2 bits each. Bit 1 of each field: 1=edge, 0=level.
                    // Setting all to edge-triggered (0xAAAAAAAA) ensures that
                    // hv_gic_set_spi assert+deassert reliably creates a pending
                    // interrupt regardless of timing — no level-triggered state
                    // machine issues.
                    // Configure ICFGRs for all SPIs we might use.
                    // The actual SPI count is queried during GIC config (typically 988),
                    // but we only need to configure the first few dozen for our devices.
                    let spi_count: u32 = 64; // covers GSIs 0-63, sufficient for all devices
                    let icfgr_count = (spi_count + 15) / 16;
                    for i in 0..icfgr_count {
                        let offset: u16 = 0x0C08 + (i as u16) * 4; // GICD_ICFGR2+
                        let val: u64 = 0xAAAAAAAA; // all edge-triggered
                        let r = unsafe { ffi::hv_gic_set_distributor_reg(offset, val) };
                        if r != ffi::HV_SUCCESS {
                            base::warn!("GICD_ICFGR[{}] (offset {:#x}) write failed: {}", i + 2, offset, r);
                        } else if i == 0 {
                            base::info!("GICD_ICFGR[2] (offset {:#x}) = {:#x} — first SPI config written OK", offset, val);
                        }
                    }
                    base::info!("GIC: configured {} SPIs as edge-triggered", spi_count);
                }
            }
        } else {
            base::info!("HVF native GIC not available (macOS <15), using MMIO emulation");
        }

        Ok(HvfVm {
            hvf,
            guest_mem,
            mem_regions: Arc::new(Mutex::new(BTreeMap::new())),
            next_mem_slot: Arc::new(Mutex::new(0)),
            ioevents: Arc::new(Mutex::new(FnvHashMap::default())),
            dax_mappings: Arc::new(Mutex::new(BTreeMap::new())),
            mapped_slots: Arc::new(Mutex::new(std::collections::HashSet::new())),
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
            dax_mappings: self.dax_mappings.clone(),
            mapped_slots: self.mapped_slots.clone(),
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
        cache: MemCacheType,
    ) -> Result<MemSlot> {
        let size = mem_region.size();
        let host_addr = mem_region.as_ptr() as *const std::ffi::c_void;

        // CacheNonCoherent is used as a signal from prepare_shared_memory_region
        // to register the DAX window in mem_regions for address tracking WITHOUT
        // actually mapping it into guest IPA space via hv_vm_map. HVF does not
        // support partial remapping within a larger mapping, so the DAX window
        // must be mapped on-demand by add_fd_mapping, not pre-mapped here.
        let skip_mapping = cache == MemCacheType::CacheNonCoherent;

        if !skip_mapping {
            // Apple Silicon uses 16KB pages. hv_vm_map requires size to be
            // page-aligned. Round up if the region isn't aligned.
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let aligned_size = (size + page_size - 1) & !(page_size - 1);

            let mut flags = ffi::HV_MEMORY_READ;
            if !read_only {
                flags |= ffi::HV_MEMORY_WRITE;
            }
            flags |= ffi::HV_MEMORY_EXEC;

            base::info!(
                "HvfVm::add_memory_region: guest={:#x} size={:#x} aligned={:#x} host={:p} flags={:#x}",
                guest_addr.0, size, aligned_size, host_addr, flags
            );

            // SAFETY: host_addr points to a valid mmap'd region. aligned_size rounds
            // up to the host page boundary (16KB on Apple Silicon). The extra bytes
            // beyond `size` are within the same mmap allocation (mmap rounds up).
            let ret = unsafe { ffi::hv_vm_map(host_addr, guest_addr.0, aligned_size, flags) };
            if ret != ffi::HV_SUCCESS {
                base::error!("hv_vm_map failed: ret={}", ret);
            }
            hvf_result(ret)?;
        } else {
            base::info!(
                "HvfVm::add_memory_region (DAX, skip hv_vm_map): guest={:#x} size={:#x}",
                guest_addr.0, size
            );
        }

        let mut slot_lock = self.next_mem_slot.lock();
        let slot = *slot_lock;
        *slot_lock += 1;

        if !skip_mapping {
            self.mapped_slots.lock().insert(slot);
        }

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
        // Clean up any DAX file mappings within this memory region first.
        // Without this, mmap'd file pages for active DAX mappings would leak.
        {
            let mut dax_map = self.dax_mappings.lock();
            let keys: Vec<_> = dax_map
                .keys()
                .filter(|(s, _)| *s == slot)
                .copied()
                .collect();
            for key in keys {
                if let Some(mapping) = dax_map.remove(&key) {
                    unsafe { ffi::hv_vm_unmap(mapping.guest_addr, mapping.size) };
                    if mapping.mmap_owned {
                        unsafe { libc::munmap(mapping.host_addr, mapping.size) };
                    }
                }
            }
        }

        let (guest_addr, mem_region) = self
            .mem_regions
            .lock()
            .remove(&slot)
            .ok_or_else(|| base::Error::new(libc::EINVAL))?;

        // Only unmap regions that were actually mapped via hv_vm_map.
        // DAX windows (CacheNonCoherent) skip the initial mapping and are
        // mapped on-demand by add_fd_mapping; the sub-region cleanup above
        // already handled those.
        if self.mapped_slots.lock().remove(&slot) {
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let aligned_size = (mem_region.size() + page_size - 1) & !(page_size - 1);
            let ret = unsafe { ffi::hv_vm_unmap(guest_addr.0, aligned_size) };
            hvf_result(ret)?;
        }

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
        let map = self.ioevents.lock();
        if let Some(evt) = map.get(&addr) {
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
        slot: u32,
        offset: usize,
        size: usize,
        fd: &dyn base::AsRawDescriptor,
        fd_offset: u64,
        prot: Protection,
    ) -> Result<()> {
        // MAP_SHARED file mapping → hv_vm_map for zero-copy DAX.
        //
        // Apple Silicon uses 16KB pages. hv_vm_map requires guest_addr and size
        // to be 16KB-aligned. GPU blob sizes are padded to 16KB in
        // resource_create_blob (macOS), so each blob maps to complete 16KB pages
        // without overlapping adjacent blobs.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let map_size = (size + page_size - 1) & !(page_size - 1);

        // Check for Venus zero-copy marker: if the fd starts with "VENUSPTR"
        // magic + a host pointer, use the pointer directly for hv_vm_map
        // instead of mmap'ing the fd. This gives zero-copy access to
        // MoltenVK Metal shared storage (CPU/GPU coherent on Apple Silicon).
        let mut venus_host_ptr: *mut libc::c_void = std::ptr::null_mut();
        {
            let mut header = [0u64; 2];
            // Header is at the END of the file: offset = size (the blob data size)
            let n = unsafe {
                libc::pread(
                    fd.as_raw_descriptor(),
                    header.as_mut_ptr() as *mut libc::c_void,
                    16,
                    size as libc::off_t,
                )
            };
            if n == 16 && header[0] == 0x56454E5553505452u64 {
                let ptr = header[1] as *mut libc::c_void;
                // Only use zero-copy if the pointer is 16KB-aligned (Apple Silicon page size)
                if (ptr as usize & (page_size - 1)) == 0 {
                    venus_host_ptr = ptr;
                }
            }
        }

        let (host_addr, mmap_owned) = if !venus_host_ptr.is_null() {
            // Zero-copy: use vkMapMemory host_ptr directly
            (venus_host_ptr, false)
        } else {
            // Standard path: mmap the fd
            let addr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    map_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd.as_raw_descriptor(),
                    fd_offset as libc::off_t,
                )
            };
            if addr == libc::MAP_FAILED {
                return Err(base::Error::new(unsafe { *libc::__error() }));
            }
            (addr, true)
        };

        let mem_regions = self.mem_regions.lock();
        let (guest_base, mem_region) = mem_regions
            .get(&slot)
            .ok_or(base::Error::new(libc::EINVAL))?;
        if offset.checked_add(size).map_or(true, |end| end > mem_region.size()) {
            drop(mem_regions);
            if mmap_owned { unsafe { libc::munmap(host_addr, map_size) }; }
            return Err(base::Error::new(libc::EINVAL));
        }
        let guest_addr = guest_base.0 + offset as u64;
        drop(mem_regions);

        let mut hvf_flags = ffi::HV_MEMORY_READ;
        if prot.allows(&Protection::write()) {
            hvf_flags |= ffi::HV_MEMORY_WRITE;
        }

        // Unmap previous mapping at this exact key only (no blind unmap).
        let key = (slot, offset);
        if let Some(old) = self.dax_mappings.lock().remove(&key) {
            unsafe { ffi::hv_vm_unmap(old.guest_addr, old.size) };
            if old.mmap_owned { unsafe { libc::munmap(old.host_addr, old.size) }; }
        }

        let ret = unsafe { ffi::hv_vm_map(host_addr as *const _, guest_addr, map_size, hvf_flags) };
        if ret != ffi::HV_SUCCESS {
            base::error!("hv_vm_map failed in add_fd_mapping: ret={:#x} guest={:#x} size={:#x}",
                         ret, guest_addr, map_size);
            if mmap_owned { unsafe { libc::munmap(host_addr, map_size) }; }
            return Err(base::Error::new(libc::EINVAL));
        }

        self.dax_mappings.lock().insert(
            key,
            DaxMapping { host_addr, guest_addr, size: map_size, mmap_owned },
        );

        Ok(())
    }

    fn remove_mapping(&mut self, slot: u32, offset: usize, size: usize) -> Result<()> {
        let key = (slot, offset);
        if let Some(mapping) = self.dax_mappings.lock().remove(&key) {
            if mapping.size != size {
                base::warn!(
                    "DAX remove_mapping size mismatch: stored={:#x} requested={:#x}",
                    mapping.size, size
                );
            }
            // Unmap from guest.
            let ret = unsafe { ffi::hv_vm_unmap(mapping.guest_addr, mapping.size) };
            if ret != ffi::HV_SUCCESS {
                base::warn!("hv_vm_unmap failed: ret={:#x} guest={:#x}", ret, mapping.guest_addr);
            }
            // Unmap from host (skip for Venus zero-copy pointers).
            if mapping.mmap_owned {
                if unsafe { libc::munmap(mapping.host_addr, mapping.size) } != 0 {
                    base::warn!("munmap failed for DAX host mapping at {:?}", mapping.host_addr);
                }
            }
        }
        Ok(())
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
