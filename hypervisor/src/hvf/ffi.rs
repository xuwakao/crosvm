// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// Rust FFI bindings for Apple Hypervisor.framework (ARM64 / Apple Silicon).
//
// Reference: https://developer.apple.com/documentation/hypervisor
// These bindings cover the subset of HVF API needed for crosvm's hypervisor backend.

#![allow(non_camel_case_types)]
#![allow(dead_code)]

use std::ffi::c_void;

/// HVF return type — 0 is success.
pub type hv_return_t = i32;

/// HVF VM configuration handle.
pub type hv_vm_config_t = *mut c_void;

/// HVF vCPU instance handle.
pub type hv_vcpu_t = u64;

/// HVF vCPU configuration handle.
pub type hv_vcpu_config_t = *mut c_void;

// ============================================================================
// Return codes
// ============================================================================

pub const HV_SUCCESS: hv_return_t = 0;
pub const HV_ERROR: hv_return_t = -1; // 0xfaf1
pub const HV_BUSY: hv_return_t = -2;
pub const HV_BAD_ARGUMENT: hv_return_t = -3;
pub const HV_NO_RESOURCES: hv_return_t = -5;
pub const HV_NO_DEVICE: hv_return_t = -6;
pub const HV_DENIED: hv_return_t = -7;
pub const HV_UNSUPPORTED: hv_return_t = -8;

// ============================================================================
// Memory permissions
// ============================================================================

pub type hv_memory_flags_t = u64;

pub const HV_MEMORY_READ: hv_memory_flags_t = 1 << 0;
pub const HV_MEMORY_WRITE: hv_memory_flags_t = 1 << 1;
pub const HV_MEMORY_EXEC: hv_memory_flags_t = 1 << 2;

// ============================================================================
// vCPU exit reason
// ============================================================================

pub type hv_exit_reason_t = u32;

pub const HV_EXIT_REASON_CANCELED: hv_exit_reason_t = 0;
pub const HV_EXIT_REASON_EXCEPTION: hv_exit_reason_t = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: hv_exit_reason_t = 2;
pub const HV_EXIT_REASON_UNKNOWN: hv_exit_reason_t = 3;

// ============================================================================
// ARM64 Exception Classes (EC field of ESR_EL2, bits [31:26])
// ============================================================================

pub const EC_WFX_TRAP: u32 = 0x01;
pub const EC_AA32_HVC: u32 = 0x12;
pub const EC_AA32_SMC: u32 = 0x13;
pub const EC_AA64_HVC: u32 = 0x16;
pub const EC_AA64_SMC: u32 = 0x17;
pub const EC_SYSTEMREGISTERTRAP: u32 = 0x18;
pub const EC_INSNABORT: u32 = 0x20;
pub const EC_INSNABORT_SAME_EL: u32 = 0x21;
pub const EC_DATAABORT: u32 = 0x24;
pub const EC_DATAABORT_SAME_EL: u32 = 0x25;
pub const EC_BREAKPOINT: u32 = 0x30;
pub const EC_BREAKPOINT_SAME_EL: u32 = 0x31;
pub const EC_SOFTWARESTEP: u32 = 0x32;
pub const EC_SOFTWARESTEP_SAME_EL: u32 = 0x33;
pub const EC_WATCHPOINT: u32 = 0x34;
pub const EC_WATCHPOINT_SAME_EL: u32 = 0x35;
pub const EC_AA64_BKPT: u32 = 0x3c;

// ============================================================================
// Syndrome field helpers (for EC_DATAABORT)
// ============================================================================

/// Instruction Syndrome Valid bit
pub const ARM_EL_ISV: u64 = 1 << 24;

/// Extract Exception Class from syndrome
#[inline]
pub fn syn_get_ec(syndrome: u64) -> u32 {
    ((syndrome >> 26) & 0x3f) as u32
}

/// Extract data abort fields from syndrome
#[inline]
pub fn data_abort_isv(syndrome: u64) -> bool {
    (syndrome & ARM_EL_ISV) != 0
}

#[inline]
pub fn data_abort_iswrite(syndrome: u64) -> bool {
    ((syndrome >> 6) & 1) != 0
}

#[inline]
pub fn data_abort_sas(syndrome: u64) -> u32 {
    ((syndrome >> 22) & 3) as u32
}

#[inline]
pub fn data_abort_srt(syndrome: u64) -> u32 {
    ((syndrome >> 16) & 0x1f) as u32
}

#[inline]
pub fn data_abort_s1ptw(syndrome: u64) -> bool {
    ((syndrome >> 7) & 1) != 0
}

/// System register trap: extract read/write direction
#[inline]
pub fn sysreg_isread(syndrome: u64) -> bool {
    (syndrome & 1) != 0
}

/// System register trap: extract target register (Xt)
#[inline]
pub fn sysreg_rt(syndrome: u64) -> u32 {
    ((syndrome >> 5) & 0x1f) as u32
}

/// System register trap: extract CRm
#[inline]
pub fn sysreg_crm(syndrome: u64) -> u32 {
    ((syndrome >> 1) & 0xf) as u32
}

/// System register trap: extract CRn
#[inline]
pub fn sysreg_crn(syndrome: u64) -> u32 {
    ((syndrome >> 10) & 0xf) as u32
}

/// System register trap: extract Op1
#[inline]
pub fn sysreg_op1(syndrome: u64) -> u32 {
    ((syndrome >> 14) & 0x7) as u32
}

/// System register trap: extract Op2
#[inline]
pub fn sysreg_op2(syndrome: u64) -> u32 {
    ((syndrome >> 17) & 0x7) as u32
}

/// System register trap: extract Op0
#[inline]
pub fn sysreg_op0(syndrome: u64) -> u32 {
    ((syndrome >> 20) & 0x3) as u32
}

/// Encode a system register ID from Op0, Op1, CRn, CRm, Op2 in HVF format.
#[inline]
pub fn sysreg_encode(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u16 {
    ((op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2) as u16
}

/// WFx trap: is it WFE (true) or WFI (false)?
#[inline]
pub fn wfx_is_wfe(syndrome: u64) -> bool {
    (syndrome & 1) != 0
}

// ============================================================================
// vCPU exit info structure (returned by hv_vcpu_run)
// ============================================================================

#[repr(C)]
#[derive(Debug, Default)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
pub struct hv_vcpu_exit_t {
    pub reason: hv_exit_reason_t,
    pub exception: hv_vcpu_exit_exception_t,
}

// ============================================================================
// General-purpose registers
// ============================================================================

pub type hv_reg_t = u32;

pub const HV_REG_X0: hv_reg_t = 0;
pub const HV_REG_X1: hv_reg_t = 1;
pub const HV_REG_X2: hv_reg_t = 2;
pub const HV_REG_X3: hv_reg_t = 3;
pub const HV_REG_X4: hv_reg_t = 4;
pub const HV_REG_X5: hv_reg_t = 5;
pub const HV_REG_X6: hv_reg_t = 6;
pub const HV_REG_X7: hv_reg_t = 7;
pub const HV_REG_X8: hv_reg_t = 8;
pub const HV_REG_X9: hv_reg_t = 9;
pub const HV_REG_X10: hv_reg_t = 10;
pub const HV_REG_X11: hv_reg_t = 11;
pub const HV_REG_X12: hv_reg_t = 12;
pub const HV_REG_X13: hv_reg_t = 13;
pub const HV_REG_X14: hv_reg_t = 14;
pub const HV_REG_X15: hv_reg_t = 15;
pub const HV_REG_X16: hv_reg_t = 16;
pub const HV_REG_X17: hv_reg_t = 17;
pub const HV_REG_X18: hv_reg_t = 18;
pub const HV_REG_X19: hv_reg_t = 19;
pub const HV_REG_X20: hv_reg_t = 20;
pub const HV_REG_X21: hv_reg_t = 21;
pub const HV_REG_X22: hv_reg_t = 22;
pub const HV_REG_X23: hv_reg_t = 23;
pub const HV_REG_X24: hv_reg_t = 24;
pub const HV_REG_X25: hv_reg_t = 25;
pub const HV_REG_X26: hv_reg_t = 26;
pub const HV_REG_X27: hv_reg_t = 27;
pub const HV_REG_X28: hv_reg_t = 28;
pub const HV_REG_X29: hv_reg_t = 29;  // FP
pub const HV_REG_X30: hv_reg_t = 30;  // LR
pub const HV_REG_PC: hv_reg_t = 31;
pub const HV_REG_FPCR: hv_reg_t = 32;
pub const HV_REG_FPSR: hv_reg_t = 33;
pub const HV_REG_CPSR: hv_reg_t = 34;

// ============================================================================
// System registers (subset needed for VMM operation)
// ============================================================================

pub type hv_sys_reg_t = u16;

pub const HV_SYS_REG_SP_EL0: hv_sys_reg_t = 0xC208;
pub const HV_SYS_REG_SP_EL1: hv_sys_reg_t = 0xE208;
pub const HV_SYS_REG_ELR_EL1: hv_sys_reg_t = 0xE201;
pub const HV_SYS_REG_SPSR_EL1: hv_sys_reg_t = 0xE200;
pub const HV_SYS_REG_VBAR_EL1: hv_sys_reg_t = 0xE600;
pub const HV_SYS_REG_SCTLR_EL1: hv_sys_reg_t = 0xC080;
pub const HV_SYS_REG_MAIR_EL1: hv_sys_reg_t = 0xE510;
pub const HV_SYS_REG_TCR_EL1: hv_sys_reg_t = 0xE102;
pub const HV_SYS_REG_TTBR0_EL1: hv_sys_reg_t = 0xE100;
pub const HV_SYS_REG_TTBR1_EL1: hv_sys_reg_t = 0xE101;
pub const HV_SYS_REG_MPIDR_EL1: hv_sys_reg_t = 0xC005;
pub const HV_SYS_REG_MIDR_EL1: hv_sys_reg_t = 0xC000;
pub const HV_SYS_REG_CNTV_CTL_EL0: hv_sys_reg_t = 0xDA19;
pub const HV_SYS_REG_CNTV_CVAL_EL0: hv_sys_reg_t = 0xDA1A;

// ============================================================================
// Interrupt types
// ============================================================================

pub type hv_interrupt_type_t = u32;

pub const HV_INTERRUPT_TYPE_IRQ: hv_interrupt_type_t = 0;
pub const HV_INTERRUPT_TYPE_FIQ: hv_interrupt_type_t = 1;

// ============================================================================
// Hypervisor.framework C API bindings
// ============================================================================

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    // --- VM management ---
    pub fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;

    // --- Memory management ---
    pub fn hv_vm_map(
        addr: *const c_void,
        ipa: u64,
        size: usize,
        flags: hv_memory_flags_t,
    ) -> hv_return_t;
    pub fn hv_vm_unmap(ipa: u64, size: usize) -> hv_return_t;
    pub fn hv_vm_protect(ipa: u64, size: usize, flags: hv_memory_flags_t) -> hv_return_t;

    // --- vCPU management ---
    pub fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *const hv_vcpu_exit_t,
        config: hv_vcpu_config_t,
    ) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;

    // --- Register access ---
    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: hv_reg_t, value: u64) -> hv_return_t;
    pub fn hv_vcpu_get_sys_reg(
        vcpu: hv_vcpu_t,
        reg: hv_sys_reg_t,
        value: *mut u64,
    ) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(
        vcpu: hv_vcpu_t,
        reg: hv_sys_reg_t,
        value: u64,
    ) -> hv_return_t;

    // --- SIMD/FP register access ---
    pub fn hv_vcpu_get_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: hv_reg_t,
        value: *mut u128,
    ) -> hv_return_t;
    pub fn hv_vcpu_set_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: hv_reg_t,
        value: u128,
    ) -> hv_return_t;

    // --- Interrupt injection ---
    pub fn hv_vcpu_set_pending_interrupt(
        vcpu: hv_vcpu_t,
        r#type: hv_interrupt_type_t,
        pending: bool,
    ) -> hv_return_t;
    pub fn hv_vcpu_get_pending_interrupt(
        vcpu: hv_vcpu_t,
        r#type: hv_interrupt_type_t,
        pending: *mut bool,
    ) -> hv_return_t;

    // --- Timer ---
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, masked: bool) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, offset: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, offset: u64) -> hv_return_t;

    // --- Debug ---
    pub fn hv_vcpu_set_trap_debug_exceptions(vcpu: hv_vcpu_t, enable: bool) -> hv_return_t;

}

/// Force the vCPU to exit from hv_vcpu_run. Safe to call from any thread.
/// This API is only available on macOS 15+. On older versions, returns HV_SUCCESS (no-op).
pub unsafe fn hv_vcpu_cancel(vcpu: hv_vcpu_t) -> hv_return_t {
    type HvVcpuCancelFn = unsafe extern "C" fn(hv_vcpu_t) -> hv_return_t;
    static FUNC: std::sync::OnceLock<Option<HvVcpuCancelFn>> = std::sync::OnceLock::new();
    let func = FUNC.get_or_init(|| {
        let sym = libc::dlsym(
            libc::RTLD_DEFAULT,
            b"hv_vcpu_cancel\0".as_ptr() as *const _,
        );
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut libc::c_void, HvVcpuCancelFn>(sym))
        }
    });
    match func {
        Some(f) => f(vcpu),
        None => HV_SUCCESS, // No-op on older macOS
    }
}

// ============================================================================
// GIC API (macOS 15.0+, runtime detected)
// ============================================================================

/// GIC config object (opaque pointer).
pub type hv_gic_config_t = *mut c_void;

/// IPA type for guest physical addresses.
pub type hv_ipa_t = u64;

/// Check if native HVF GIC API is available (macOS 15+).
pub fn hvf_gic_is_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| unsafe {
        !libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_create\0".as_ptr() as *const _).is_null()
    })
}

macro_rules! dlsym_fn {
    ($name:ident, $sym:expr, $ty:ty, $default:expr) => {
        pub unsafe fn $name(args: impl FnOnce($ty) -> hv_return_t) -> hv_return_t {
            static FUNC: std::sync::OnceLock<Option<$ty>> = std::sync::OnceLock::new();
            let func = FUNC.get_or_init(|| {
                let sym = libc::dlsym(libc::RTLD_DEFAULT, $sym.as_ptr() as *const _);
                if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
            });
            match func {
                Some(f) => args(*f),
                None => $default,
            }
        }
    };
}

/// Create GIC config object. Returns null on macOS <15.
/// This is an OS_OBJECT type — must be released with dispatch_release/os_release.
pub unsafe fn hv_gic_config_create() -> hv_gic_config_t {
    // Use weak linking: this symbol only exists on macOS 15+
    type Fn = unsafe extern "C" fn() -> hv_gic_config_t;
    static FUNC: std::sync::OnceLock<Option<Fn>> = std::sync::OnceLock::new();
    let func = FUNC.get_or_init(|| {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_config_create\0".as_ptr() as *const _);
        if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
    });
    match func {
        Some(f) => {
            let obj = f();
            if !obj.is_null() {
                base::info!("hv_gic_config_create returned {:p}", obj);
            }
            obj
        }
        None => std::ptr::null_mut(),
    }
}

/// Set GIC distributor base address.
pub unsafe fn hv_gic_config_set_distributor_base(config: hv_gic_config_t, addr: hv_ipa_t) -> hv_return_t {
    type Fn = unsafe extern "C" fn(hv_gic_config_t, hv_ipa_t) -> hv_return_t;
    static FUNC: std::sync::OnceLock<Option<Fn>> = std::sync::OnceLock::new();
    let func = FUNC.get_or_init(|| {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_config_set_distributor_base\0".as_ptr() as *const _);
        if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
    });
    match func { Some(f) => f(config, addr), None => -1 }
}

/// Set GIC redistributor base address.
pub unsafe fn hv_gic_config_set_redistributor_base(config: hv_gic_config_t, addr: hv_ipa_t) -> hv_return_t {
    type Fn = unsafe extern "C" fn(hv_gic_config_t, hv_ipa_t) -> hv_return_t;
    static FUNC: std::sync::OnceLock<Option<Fn>> = std::sync::OnceLock::new();
    let func = FUNC.get_or_init(|| {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_config_set_redistributor_base\0".as_ptr() as *const _);
        if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
    });
    match func { Some(f) => f(config, addr), None => -1 }
}

/// Create GIC device.
pub unsafe fn hv_gic_create(config: hv_gic_config_t) -> hv_return_t {
    type Fn = unsafe extern "C" fn(hv_gic_config_t) -> hv_return_t;
    static FUNC: std::sync::OnceLock<Option<Fn>> = std::sync::OnceLock::new();
    let func = FUNC.get_or_init(|| {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_create\0".as_ptr() as *const _);
        if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
    });
    match func { Some(f) => f(config), None => -1 }
}

/// Inject SPI interrupt. Thread-safe.
pub unsafe fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t {
    type Fn = unsafe extern "C" fn(u32, bool) -> hv_return_t;
    static FUNC: std::sync::OnceLock<Option<Fn>> = std::sync::OnceLock::new();
    let func = FUNC.get_or_init(|| {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_set_spi\0".as_ptr() as *const _);
        if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
    });
    match func { Some(f) => f(intid, level), None => -1 }
}

/// Reset GIC device.
pub unsafe fn hv_gic_reset() -> hv_return_t {
    type Fn = unsafe extern "C" fn() -> hv_return_t;
    static FUNC: std::sync::OnceLock<Option<Fn>> = std::sync::OnceLock::new();
    let func = FUNC.get_or_init(|| {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"hv_gic_reset\0".as_ptr() as *const _);
        if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
    });
    match func { Some(f) => f(), None => -1 }
}

// ============================================================================
// Safe wrapper helpers
// ============================================================================

/// Convert HVF return code to a Rust Result.
pub fn hvf_result(ret: hv_return_t) -> std::result::Result<(), base::Error> {
    if ret == HV_SUCCESS {
        Ok(())
    } else {
        Err(base::Error::new(ret))
    }
}
