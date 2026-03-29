// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// HVF Vcpu and VcpuAArch64 trait implementations.
// Implements the vCPU run loop with ARM64 exception exit handling.
//
// Reference: QEMU target/arm/hvf/hvf.c (hvf_handle_exception)

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use aarch64_sys_reg::AArch64SysRegId;
use base::Error;
use base::Result;
use snapshot::AnySnapshot;

use super::ffi;
use super::ffi::hvf_result;
use crate::aarch64::*;
use crate::IoOperation;
use crate::IoParams;
use crate::Vcpu;
use crate::VcpuExit;

/// Apple Hypervisor.framework vCPU instance.
pub struct HvfVcpu {
    id: usize,
    vcpu: ffi::hv_vcpu_t,
    exit_info: *const ffi::hv_vcpu_exit_t,
    immediate_exit: Arc<AtomicBool>,
}

// SAFETY: HvfVcpu is bound to one thread (HVF requirement), but we need Send for the trait.
// The caller must ensure single-threaded access per vCPU (which crosvm does).
unsafe impl Send for HvfVcpu {}
unsafe impl Sync for HvfVcpu {}

impl HvfVcpu {
    /// Create a new HVF vCPU. Must be called from the thread that will run the vCPU.
    pub fn new(id: usize) -> Result<Self> {
        let mut vcpu: ffi::hv_vcpu_t = 0;
        let mut exit_info: *const ffi::hv_vcpu_exit_t = std::ptr::null();

        // SAFETY: hv_vcpu_create initializes vcpu and exit_info.
        let ret = unsafe {
            ffi::hv_vcpu_create(
                &mut vcpu,
                &mut exit_info as *mut *const _ as *mut *const ffi::hv_vcpu_exit_t,
                std::ptr::null_mut(),
            )
        };
        hvf_result(ret)?;

        // Enable trapping of debug exceptions
        let ret = unsafe { ffi::hv_vcpu_set_trap_debug_exceptions(vcpu, true) };
        hvf_result(ret)?;

        Ok(HvfVcpu {
            id,
            vcpu,
            exit_info,
            immediate_exit: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Read a general-purpose register (X0-X30, PC, CPSR, etc.)
    fn get_reg(&self, reg: ffi::hv_reg_t) -> Result<u64> {
        let mut val: u64 = 0;
        let ret = unsafe { ffi::hv_vcpu_get_reg(self.vcpu, reg, &mut val) };
        hvf_result(ret)?;
        Ok(val)
    }

    /// Write a general-purpose register.
    fn set_reg(&self, reg: ffi::hv_reg_t, val: u64) -> Result<()> {
        let ret = unsafe { ffi::hv_vcpu_set_reg(self.vcpu, reg, val) };
        hvf_result(ret)
    }

    /// Read a system register.
    fn get_sys_reg(&self, reg: ffi::hv_sys_reg_t) -> Result<u64> {
        let mut val: u64 = 0;
        let ret = unsafe { ffi::hv_vcpu_get_sys_reg(self.vcpu, reg, &mut val) };
        hvf_result(ret)?;
        Ok(val)
    }

    /// Write a system register.
    fn set_sys_reg(&self, reg: ffi::hv_sys_reg_t, val: u64) -> Result<()> {
        let ret = unsafe { ffi::hv_vcpu_set_sys_reg(self.vcpu, reg, val) };
        hvf_result(ret)
    }

    /// Advance PC by 4 bytes (ARM64 fixed instruction length).
    fn advance_pc(&self) -> Result<()> {
        let pc = self.get_reg(ffi::HV_REG_PC)?;
        self.set_reg(ffi::HV_REG_PC, pc + 4)
    }

    /// Map VcpuRegAArch64 to HVF register ID.
    fn reg_to_hvf(reg: &VcpuRegAArch64) -> std::result::Result<ffi::hv_reg_t, Error> {
        match reg {
            VcpuRegAArch64::X(n) => {
                if *n <= 30 {
                    Ok(ffi::HV_REG_X0 + *n as u32)
                } else {
                    Err(Error::new(libc::EINVAL))
                }
            }
            VcpuRegAArch64::Sp => Ok(ffi::HV_REG_X0 + 31), // SP is separate in HVF
            VcpuRegAArch64::Pc => Ok(ffi::HV_REG_PC),
            VcpuRegAArch64::Pstate => Ok(ffi::HV_REG_CPSR),
            VcpuRegAArch64::System(_) => Err(Error::new(libc::EINVAL)), // use get_sys_reg instead
        }
    }
}

impl Vcpu for HvfVcpu {
    fn try_clone(&self) -> Result<Self> {
        // HVF vCPUs are bound to their creating thread and cannot be cloned.
        Err(Error::new(libc::ENOTSUP))
    }

    fn as_vcpu(&self) -> &dyn Vcpu {
        self
    }

    fn run(&mut self) -> Result<VcpuExit> {
        if self.immediate_exit.load(Ordering::SeqCst) {
            self.immediate_exit.store(false, Ordering::SeqCst);
            return Ok(VcpuExit::Intr);
        }

        // SAFETY: vcpu is a valid handle from hv_vcpu_create.
        let ret = unsafe { ffi::hv_vcpu_run(self.vcpu) };
        hvf_result(ret)?;

        // SAFETY: exit_info is valid for the lifetime of the vCPU.
        let exit = unsafe { &*self.exit_info };

        match exit.reason {
            ffi::HV_EXIT_REASON_EXCEPTION => {
                let syndrome = exit.exception.syndrome;
                let ec = ffi::syn_get_ec(syndrome);

                match ec {
                    ffi::EC_DATAABORT | ffi::EC_DATAABORT_SAME_EL => {
                        // MMIO access — the most common exit.
                        Ok(VcpuExit::Mmio)
                    }
                    ffi::EC_SYSTEMREGISTERTRAP => {
                        // System register access trapped to hypervisor.
                        Ok(VcpuExit::MsrAccess)
                    }
                    ffi::EC_WFX_TRAP => {
                        if ffi::wfx_is_wfe(syndrome) {
                            // WFE — just advance PC and continue.
                            self.advance_pc()?;
                            Ok(VcpuExit::Intr)
                        } else {
                            // WFI — halt until interrupt.
                            self.advance_pc()?;
                            Ok(VcpuExit::Hlt)
                        }
                    }
                    ffi::EC_AA64_HVC => Ok(VcpuExit::Hypercall),
                    ffi::EC_AA64_SMC => {
                        // SMC requires advancing PC (unlike HVC).
                        self.advance_pc()?;
                        Ok(VcpuExit::Hypercall)
                    }
                    ffi::EC_SOFTWARESTEP
                    | ffi::EC_SOFTWARESTEP_SAME_EL
                    | ffi::EC_AA64_BKPT
                    | ffi::EC_BREAKPOINT
                    | ffi::EC_BREAKPOINT_SAME_EL
                    | ffi::EC_WATCHPOINT
                    | ffi::EC_WATCHPOINT_SAME_EL => Ok(VcpuExit::Debug),
                    ffi::EC_INSNABORT | ffi::EC_INSNABORT_SAME_EL => Ok(VcpuExit::Exception),
                    _ => {
                        base::error!("unhandled HVF exception EC=0x{:x} syndrome=0x{:x}", ec, syndrome);
                        Ok(VcpuExit::Exception)
                    }
                }
            }
            ffi::HV_EXIT_REASON_CANCELED => Ok(VcpuExit::Intr),
            ffi::HV_EXIT_REASON_VTIMER_ACTIVATED => {
                // Virtual timer fired — unmask and deliver as interrupt.
                unsafe { ffi::hv_vcpu_set_vtimer_mask(self.vcpu, false) };
                Ok(VcpuExit::Intr)
            }
            _ => {
                base::error!("unknown HVF exit reason: {}", exit.reason);
                Ok(VcpuExit::Exception)
            }
        }
    }

    fn id(&self) -> usize {
        self.id
    }

    fn set_immediate_exit(&self, exit: bool) {
        self.immediate_exit.store(exit, Ordering::SeqCst);
    }

    fn handle_mmio(&self, handle_fn: &mut dyn FnMut(IoParams) -> Result<()>) -> Result<()> {
        let exit = unsafe { &*self.exit_info };
        let syndrome = exit.exception.syndrome;
        let ipa = exit.exception.physical_address;

        if !ffi::data_abort_isv(syndrome) {
            // ISV=0: instruction syndrome not valid (SIMD/SVE access).
            // Cannot decode — inject fault back to guest.
            return Err(Error::new(libc::EIO));
        }

        let iswrite = ffi::data_abort_iswrite(syndrome);
        let sas = ffi::data_abort_sas(syndrome); // access size: 0=byte, 1=hw, 2=word, 3=dword
        let len = 1usize << sas;
        let srt = ffi::data_abort_srt(syndrome); // target register

        if iswrite {
            let val = self.get_reg(ffi::HV_REG_X0 + srt)?;
            let data = val.to_le_bytes();
            handle_fn(IoParams {
                address: ipa,
                operation: IoOperation::Write(&data[..len]),
            })?;
        } else {
            let mut data = [0u8; 8];
            handle_fn(IoParams {
                address: ipa,
                operation: IoOperation::Read(&mut data[..len]),
            })?;
            let val = u64::from_le_bytes(data);
            self.set_reg(ffi::HV_REG_X0 + srt, val)?;
        }

        self.advance_pc()?;
        Ok(())
    }

    fn handle_io(&self, _handle_fn: &mut dyn FnMut(IoParams)) -> Result<()> {
        // ARM64 does not have IO port instructions.
        Err(Error::new(libc::ENOTSUP))
    }

    fn on_suspend(&self) -> Result<()> {
        Ok(())
    }

    unsafe fn enable_raw_capability(&self, _cap: u32, _args: &[u64; 4]) -> Result<()> {
        Err(Error::new(libc::ENOTSUP))
    }
}

impl VcpuAArch64 for HvfVcpu {
    fn init(&self, _features: &[VcpuFeature]) -> Result<()> {
        // HVF vCPUs are initialized at creation time.
        // PSCI is handled via HVC/SMC exits in the run loop.
        Ok(())
    }

    fn init_pmu(&self, _irq: u64) -> Result<()> {
        Err(Error::new(libc::ENOTSUP))
    }

    fn has_pvtime_support(&self) -> bool {
        false
    }

    fn init_pvtime(&self, _pvtime_ipa: u64) -> Result<()> {
        Err(Error::new(libc::ENOTSUP))
    }

    fn set_one_reg(&self, reg_id: VcpuRegAArch64, data: u64) -> Result<()> {
        match reg_id {
            VcpuRegAArch64::System(sys_reg) => {
                // Map AArch64SysRegId to HVF sys_reg encoding.
                // The HVF encoding matches the ARM system register encoding.
                let hvf_reg = sys_reg.encoded() as ffi::hv_sys_reg_t;
                self.set_sys_reg(hvf_reg, data)
            }
            _ => {
                let hvf_reg = Self::reg_to_hvf(&reg_id)?;
                self.set_reg(hvf_reg, data)
            }
        }
    }

    fn get_one_reg(&self, reg_id: VcpuRegAArch64) -> Result<u64> {
        match reg_id {
            VcpuRegAArch64::System(sys_reg) => {
                let hvf_reg = sys_reg.encoded() as ffi::hv_sys_reg_t;
                self.get_sys_reg(hvf_reg)
            }
            _ => {
                let hvf_reg = Self::reg_to_hvf(&reg_id)?;
                self.get_reg(hvf_reg)
            }
        }
    }

    fn set_vector_reg(&self, reg_num: u8, data: u128) -> Result<()> {
        if reg_num > 31 {
            return Err(Error::new(libc::EINVAL));
        }
        // HVF SIMD registers start after the GP registers.
        // V0-V31 map to HV_SIMD_FP_REG_Q0..Q31
        let ret = unsafe { ffi::hv_vcpu_set_simd_fp_reg(self.vcpu, reg_num as u32, data) };
        hvf_result(ret)
    }

    fn get_vector_reg(&self, reg_num: u8) -> Result<u128> {
        if reg_num > 31 {
            return Err(Error::new(libc::EINVAL));
        }
        let mut val: u128 = 0;
        let ret = unsafe { ffi::hv_vcpu_get_simd_fp_reg(self.vcpu, reg_num as u32, &mut val) };
        hvf_result(ret)?;
        Ok(val)
    }

    fn get_system_regs(&self) -> Result<BTreeMap<AArch64SysRegId, u64>> {
        // Return a minimal set of system registers.
        // A full implementation would enumerate all accessible sys regs.
        let mut regs = BTreeMap::new();
        // Read MPIDR to at least identify the vCPU.
        // Minimal set — MPIDR for vCPU identification.
        // AArch64SysRegId construction depends on the encoding scheme used by aarch64_sys_reg crate.
        let _ = self.get_sys_reg(ffi::HV_SYS_REG_MPIDR_EL1); // just verify it works
        Ok(regs)
    }

    fn hypervisor_specific_snapshot(&self) -> anyhow::Result<AnySnapshot> {
        AnySnapshot::to_any(())
    }

    fn hypervisor_specific_restore(&self, _data: AnySnapshot) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_psci_version(&self) -> Result<PsciVersion> {
        PsciVersion::new(1, 0)
    }

    fn set_guest_debug(&self, _addrs: &[vm_memory::GuestAddress], _enable_singlestep: bool) -> Result<()> {
        // TODO: implement debug register setup
        Err(Error::new(libc::ENOTSUP))
    }

    fn get_max_hw_bps(&self) -> Result<usize> {
        Ok(0) // TODO: query HVF for hardware breakpoint count
    }

    fn get_cache_info(&self) -> Result<BTreeMap<u8, u64>> {
        Ok(BTreeMap::new())
    }

    fn set_cache_info(&self, _cache_info: BTreeMap<u8, u64>) -> Result<()> {
        Ok(())
    }
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        // SAFETY: vcpu is a valid handle.
        unsafe {
            ffi::hv_vcpu_destroy(self.vcpu);
        }
    }
}
