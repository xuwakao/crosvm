// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// Compile-time and unit tests for the HVF hypervisor backend.
// Runtime tests that need actual HVF hardware are marked #[ignore].

#[cfg(test)]
mod tests {
    use super::super::ffi;

    // =========================================================================
    // Static trait assertions — verify types implement required traits at compile time.
    // These don't need HVF runtime, they just check the type system.
    // =========================================================================

    /// Verify Hvf implements Hypervisor trait.
    fn _assert_hvf_is_hypervisor<T: crate::Hypervisor>() {}
    fn _assert_hvf_hypervisor() {
        _assert_hvf_is_hypervisor::<super::super::Hvf>();
    }

    /// Verify HvfVm implements Vm + VmAArch64 traits.
    fn _assert_vm<T: crate::Vm>() {}
    fn _assert_vm_aarch64<T: crate::VmAArch64>() {}
    fn _assert_hvf_vm() {
        _assert_vm::<super::super::vm::HvfVm>();
        _assert_vm_aarch64::<super::super::vm::HvfVm>();
    }

    /// Verify HvfVcpu implements Vcpu + VcpuAArch64 traits.
    fn _assert_vcpu<T: crate::Vcpu>() {}
    fn _assert_vcpu_aarch64<T: crate::VcpuAArch64>() {}
    fn _assert_hvf_vcpu() {
        _assert_vcpu::<super::super::vcpu::HvfVcpu>();
        _assert_vcpu_aarch64::<super::super::vcpu::HvfVcpu>();
    }

    // =========================================================================
    // FFI constant verification — compare against ARM Architecture Reference Manual values.
    // =========================================================================

    #[test]
    fn ffi_constants_correct() {
        // Exception Class values from ARM DDI 0487 (ARMv8-A Architecture Reference Manual)
        // Table D13-6: Exception classes
        assert_eq!(ffi::EC_WFX_TRAP, 0x01, "EC_WFX_TRAP must be 0x01");
        assert_eq!(ffi::EC_AA64_HVC, 0x16, "EC_AA64_HVC must be 0x16");
        assert_eq!(ffi::EC_AA64_SMC, 0x17, "EC_AA64_SMC must be 0x17");
        assert_eq!(
            ffi::EC_SYSTEMREGISTERTRAP, 0x18,
            "EC_SYSTEMREGISTERTRAP must be 0x18"
        );
        assert_eq!(ffi::EC_INSNABORT, 0x20, "EC_INSNABORT must be 0x20");
        assert_eq!(ffi::EC_DATAABORT, 0x24, "EC_DATAABORT must be 0x24");
        assert_eq!(ffi::EC_BREAKPOINT, 0x30, "EC_BREAKPOINT must be 0x30");
        assert_eq!(ffi::EC_SOFTWARESTEP, 0x32, "EC_SOFTWARESTEP must be 0x32");
        assert_eq!(ffi::EC_WATCHPOINT, 0x34, "EC_WATCHPOINT must be 0x34");
        assert_eq!(ffi::EC_AA64_BKPT, 0x3c, "EC_AA64_BKPT must be 0x3c");

        // HVF exit reasons
        assert_eq!(ffi::HV_EXIT_REASON_CANCELED, 0);
        assert_eq!(ffi::HV_EXIT_REASON_EXCEPTION, 1);
        assert_eq!(ffi::HV_EXIT_REASON_VTIMER_ACTIVATED, 2);

        // HVF memory flags
        assert_eq!(ffi::HV_MEMORY_READ, 1 << 0);
        assert_eq!(ffi::HV_MEMORY_WRITE, 1 << 1);
        assert_eq!(ffi::HV_MEMORY_EXEC, 1 << 2);

        // HVF return codes
        assert_eq!(ffi::HV_SUCCESS, 0);
    }

    // =========================================================================
    // Syndrome parsing tests — using known syndrome values from QEMU test vectors.
    // =========================================================================

    #[test]
    fn syndrome_parsing_ec_extraction() {
        // EC is bits [31:26] of the syndrome (ESR_EL2)
        // Data Abort from lower EL: EC=0x24, syndrome with ISV=1, iswrite=1, SAS=2 (word), SRT=5
        let syndrome: u64 = (0x24u64 << 26) | (1 << 24) | (1 << 6) | (2 << 22) | (5 << 16);
        assert_eq!(ffi::syn_get_ec(syndrome), ffi::EC_DATAABORT);
    }

    #[test]
    fn syndrome_parsing_data_abort_write() {
        // Data abort: ISV=1, iswrite=1, SAS=2 (4-byte word), SRT=3 (register X3)
        let syndrome: u64 = (ffi::EC_DATAABORT as u64) << 26
            | (1u64 << 24)  // ISV
            | (1u64 << 6)   // iswrite
            | (2u64 << 22)  // SAS = 2 (word)
            | (3u64 << 16); // SRT = 3 (X3)

        assert!(ffi::data_abort_isv(syndrome), "ISV should be set");
        assert!(ffi::data_abort_iswrite(syndrome), "iswrite should be set");
        assert_eq!(ffi::data_abort_sas(syndrome), 2, "SAS should be 2 (word)");
        assert_eq!(ffi::data_abort_srt(syndrome), 3, "SRT should be 3 (X3)");
        assert!(!ffi::data_abort_s1ptw(syndrome), "s1ptw should not be set");
    }

    #[test]
    fn syndrome_parsing_data_abort_read() {
        // Data abort: ISV=1, iswrite=0, SAS=3 (8-byte dword), SRT=10 (X10)
        let syndrome: u64 = (ffi::EC_DATAABORT as u64) << 26
            | (1u64 << 24)   // ISV
            | (0u64 << 6)    // iswrite = 0 (read)
            | (3u64 << 22)   // SAS = 3 (doubleword)
            | (10u64 << 16); // SRT = 10 (X10)

        assert!(ffi::data_abort_isv(syndrome));
        assert!(!ffi::data_abort_iswrite(syndrome), "should be a read");
        assert_eq!(ffi::data_abort_sas(syndrome), 3, "SAS should be 3 (dword)");
        assert_eq!(ffi::data_abort_srt(syndrome), 10, "SRT should be 10 (X10)");
    }

    #[test]
    fn syndrome_parsing_data_abort_byte_access() {
        // Data abort: ISV=1, iswrite=1, SAS=0 (1-byte), SRT=0 (X0)
        let syndrome: u64 = (ffi::EC_DATAABORT as u64) << 26
            | (1u64 << 24)  // ISV
            | (1u64 << 6)   // iswrite
            | (0u64 << 22)  // SAS = 0 (byte)
            | (0u64 << 16); // SRT = 0 (X0)

        assert_eq!(ffi::data_abort_sas(syndrome), 0, "SAS should be 0 (byte)");
        assert_eq!(ffi::data_abort_srt(syndrome), 0, "SRT should be 0 (X0)");
        assert_eq!(1usize << ffi::data_abort_sas(syndrome), 1, "access size should be 1 byte");
    }

    #[test]
    fn syndrome_parsing_wfx() {
        // WFI: EC=0x01, bit 0 = 0 (WFI)
        let wfi_syndrome: u64 = (ffi::EC_WFX_TRAP as u64) << 26;
        assert!(!ffi::wfx_is_wfe(wfi_syndrome), "should be WFI, not WFE");

        // WFE: EC=0x01, bit 0 = 1 (WFE)
        let wfe_syndrome: u64 = (ffi::EC_WFX_TRAP as u64) << 26 | 1;
        assert!(ffi::wfx_is_wfe(wfe_syndrome), "should be WFE");
    }

    #[test]
    fn syndrome_parsing_sysreg_trap() {
        // System register trap: EC=0x18, bit 0 = 1 (read), RT = 7
        let syndrome: u64 = (ffi::EC_SYSTEMREGISTERTRAP as u64) << 26
            | 1           // isread
            | (7 << 5);   // RT = 7

        assert!(ffi::sysreg_isread(syndrome), "should be a read");
        assert_eq!(ffi::sysreg_rt(syndrome), 7, "RT should be 7");

        // Write: bit 0 = 0
        let write_syndrome: u64 = (ffi::EC_SYSTEMREGISTERTRAP as u64) << 26
            | (0)         // isread = 0 (write)
            | (15 << 5);  // RT = 15

        assert!(!ffi::sysreg_isread(write_syndrome), "should be a write");
        assert_eq!(ffi::sysreg_rt(write_syndrome), 15, "RT should be 15");
    }
}
