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
    /// Shared copy of vcpu handle for cross-thread cancellation.
    /// hv_vcpu_cancel is safe to call from any thread.
    vcpu_for_cancel: Arc<VcpuCancelHandle>,
    /// Pending IRQ number for GIC ICC_IAR1_EL1 acknowledgment.
    /// Set when vtimer fires, cleared when ICC_IAR1_EL1 is read.
    pending_irq: std::cell::Cell<Option<u32>>,
}

/// Thread-safe handle for cancelling a vCPU from another thread.
struct VcpuCancelHandle(ffi::hv_vcpu_t);
// SAFETY: hv_vcpu_cancel is explicitly documented as safe to call from any thread.
unsafe impl Send for VcpuCancelHandle {}
unsafe impl Sync for VcpuCancelHandle {}

// SAFETY: HvfVcpu is created on one thread but moved to a dedicated vCPU thread by crosvm.
// Send is needed for this transfer. Sync is required by the Vcpu trait (DowncastSync).
// The raw pointer `exit_info` prevents auto-Sync, but it is only accessed from the vCPU
// thread after `hv_vcpu_run` returns, and `hv_vcpu_cancel` (the only cross-thread operation)
// does not access exit_info.
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
            vcpu_for_cancel: Arc::new(VcpuCancelHandle(vcpu)),
            pending_irq: std::cell::Cell::new(None),
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

    /// Get the raw HVF vCPU handle for interrupt injection.
    pub fn hvf_handle(&self) -> ffi::hv_vcpu_t {
        self.vcpu
    }

    /// Handle a system register (MSR/MRS) trap.
    /// The syndrome encodes the register, direction, and transfer register.
    /// First tries HVF's own sysreg access; for GIC ICC registers, returns
    /// sensible defaults; for unknown registers, returns 0 on read.
    fn handle_sysreg_trap(&mut self, syndrome: u64) -> Result<()> {
        let is_read = ffi::sysreg_isread(syndrome);
        let rt = ffi::sysreg_rt(syndrome);
        let op0 = ffi::sysreg_op0(syndrome);
        let op1 = ffi::sysreg_op1(syndrome);
        let crn = ffi::sysreg_crn(syndrome);
        let crm = ffi::sysreg_crm(syndrome);
        let op2 = ffi::sysreg_op2(syndrome);

        let reg_id = ffi::sysreg_encode(op0, op1, crn, crm, op2);

        if is_read {
            // MRS Xt, <sysreg>
            let value = self.read_sysreg_value(reg_id);
            // Only log ICC/GIC-related registers to avoid noise
            if (crn == 12 && crm == 12) || (crn == 4 && crm == 6) {
                base::info!(
                    "  sysreg MRS X{}, S{}_{}_C{}_C{}_{} (id={:#06x}) → {:#x}",
                    rt, op0, op1, crn, crm, op2, reg_id, value
                );
            }
            if rt < 31 {
                self.set_reg(ffi::HV_REG_X0 + rt, value)?;
            }
        } else {
            // MSR <sysreg>, Xt
            let value = if rt < 31 {
                self.get_reg(ffi::HV_REG_X0 + rt)?
            } else {
                0 // XZR
            };
            if (crn == 12 && crm == 12) || (crn == 4 && crm == 6) {
                base::info!(
                    "  sysreg MSR S{}_{}_C{}_C{}_{} (id={:#06x}), X{} ← {:#x}",
                    op0, op1, crn, crm, op2, reg_id, rt, value
                );
            }
            self.write_sysreg_value(reg_id, value);
        }
        Ok(())
    }

    /// Read a trapped system register value.
    /// Tries HVF first, falls back to GIC ICC register defaults.
    fn read_sysreg_value(&self, reg_id: u16) -> u64 {
        // Try HVF's own register access first.
        let mut val: u64 = 0;
        let ret = unsafe { ffi::hv_vcpu_get_sys_reg(self.vcpu, reg_id, &mut val) };
        if ret == ffi::HV_SUCCESS {
            return val;
        }

        // GIC ICC system registers
        let icc_sre_el1 = ffi::sysreg_encode(3, 0, 12, 12, 5);     // ICC_SRE_EL1
        let icc_ctlr_el1 = ffi::sysreg_encode(3, 0, 12, 12, 4);    // ICC_CTLR_EL1
        let icc_pmr_el1 = ffi::sysreg_encode(3, 0, 4, 6, 0);       // ICC_PMR_EL1
        let icc_iar1_el1 = ffi::sysreg_encode(3, 0, 12, 12, 0);    // ICC_IAR1_EL1 (ack)
        let icc_eoir1_el1 = ffi::sysreg_encode(3, 0, 12, 12, 1);   // ICC_EOIR1_EL1 (eoi)
        let icc_igrpen1_el1 = ffi::sysreg_encode(3, 0, 12, 12, 7); // ICC_IGRPEN1_EL1
        let icc_bpr1_el1 = ffi::sysreg_encode(3, 0, 12, 12, 3);    // ICC_BPR1_EL1

        match reg_id {
            x if x == icc_sre_el1 => 0x7,    // SRE=1, DFB=1, DIB=1 (system registers enabled)
            x if x == icc_ctlr_el1 => 0x0,   // Default control
            x if x == icc_pmr_el1 => 0xff,    // All priorities enabled
            x if x == icc_iar1_el1 => {
                // Acknowledge: return pending IRQ number, or 1023 (spurious)
                match self.pending_irq.take() {
                    Some(irq) => irq as u64,
                    None => 1023,
                }
            }
            x if x == icc_eoir1_el1 => 0,   // EOI read returns 0
            x if x == icc_igrpen1_el1 => 0x1, // Group 1 enabled
            x if x == icc_bpr1_el1 => 0,      // Binary point = 0
            _ => 0,                            // Unknown register — return 0
        }
    }

    /// Write a trapped system register value.
    fn write_sysreg_value(&self, reg_id: u16, value: u64) {
        // Try HVF's own register access first.
        let ret = unsafe { ffi::hv_vcpu_set_sys_reg(self.vcpu, reg_id, value) };
        if ret == ffi::HV_SUCCESS {
            return;
        }
        // Silently ignore writes to unrecognized registers.
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
                        Ok(VcpuExit::Mmio)
                    }
                    ffi::EC_SYSTEMREGISTERTRAP => {
                        // System register access trapped to hypervisor.
                        // Syndrome encodes: direction (read/write), Rt, CRn, CRm, Op0, Op1, Op2.
                        // We handle the access here and advance PC.
                        self.handle_sysreg_trap(syndrome)?;
                        self.advance_pc()?;
                        Ok(VcpuExit::Intr) // Continue execution
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
                    ffi::EC_INSNABORT | ffi::EC_INSNABORT_SAME_EL => {
                        let ipa = exit.exception.physical_address;
                        let pc = self.get_reg(ffi::HV_REG_PC).unwrap_or(0);
                        base::error!(
                            "HVF instruction abort: EC=0x{:x} IPA=0x{:x} PC=0x{:x} syndrome=0x{:x}",
                            ec, ipa, pc, syndrome
                        );
                        Ok(VcpuExit::Exception)
                    }
                    _ => {
                        let ipa = exit.exception.physical_address;
                        let pc = self.get_reg(ffi::HV_REG_PC).unwrap_or(0);
                        base::error!(
                            "unhandled HVF exception EC=0x{:x} IPA=0x{:x} PC=0x{:x} syndrome=0x{:x}",
                            ec, ipa, pc, syndrome
                        );
                        Ok(VcpuExit::Exception)
                    }
                }
            }
            ffi::HV_EXIT_REASON_CANCELED => Ok(VcpuExit::Intr),
            ffi::HV_EXIT_REASON_VTIMER_ACTIVATED => {
                // Virtual timer fired. With HVF native GIC (macOS 15+),
                // the vtimer→PPI routing is handled by hardware. We just
                // unmask the timer so the IRQ is delivered to the vCPU.
                // The kernel acknowledges via ICC system registers managed
                // by HVF's GIC.
                //
                // Without native GIC (macOS 14), we use hv_vcpu_set_pending_interrupt
                // as a fallback, but this won't work for proper timer scheduling.
                if ffi::hvf_gic_is_available() {
                    // Native GIC: just unmask, HVF handles PPI delivery.
                    unsafe { ffi::hv_vcpu_set_vtimer_mask(self.vcpu, false) };
                } else {
                    // Fallback: inject physical IRQ.
                    unsafe {
                        ffi::hv_vcpu_set_pending_interrupt(
                            self.vcpu,
                            ffi::HV_INTERRUPT_TYPE_IRQ,
                            true,
                        );
                    };
                }
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
        if exit {
            // Force the vCPU to exit hv_vcpu_run immediately.
            // hv_vcpu_cancel is safe to call from any thread.
            unsafe { ffi::hv_vcpu_cancel(self.vcpu_for_cancel.0) };
        }
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

    fn handle_hypercall(
        &self,
        handle_fn: &mut dyn FnMut(&mut crate::HypercallAbi) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        const SMCCC_NOT_SUPPORTED: usize = u64::MAX as usize;

        // SMCCC: FID in W0, args in X1-X3, results in X0-X3.
        let function_id = (self.get_one_reg(VcpuRegAArch64::X(0))? as u32)
            .try_into()
            .unwrap();
        let args = &[
            self.get_one_reg(VcpuRegAArch64::X(1))?.try_into().unwrap(),
            self.get_one_reg(VcpuRegAArch64::X(2))?.try_into().unwrap(),
            self.get_one_reg(VcpuRegAArch64::X(3))?.try_into().unwrap(),
        ];
        let default_res = &[SMCCC_NOT_SUPPORTED, 0, 0, 0];

        let mut smccc_abi = crate::HypercallAbi::new(function_id, args, default_res);

        let result = handle_fn(&mut smccc_abi);

        for (i, value) in smccc_abi.get_results().iter().enumerate() {
            self.set_one_reg(VcpuRegAArch64::X(i as _), (*value).try_into().unwrap())?;
        }

        result
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
