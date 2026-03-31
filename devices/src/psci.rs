// PSCI (Power State Coordination Interface) device for HVF.
//
// On KVM, PSCI is handled natively by the kernel. On HVF (macOS), PSCI
// hypercalls are delivered to userspace and must be emulated. This device
// registers on the hypercall bus and handles all standard PSCI 1.0 calls.
//
// SYSTEM_OFF and SYSTEM_RESET signal the vCPU loop via a shared atomic flag.
// CPU_ON signals secondary vCPU threads via a callback.

use std::ops::Range;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::bail;
use base::info;
use hypervisor::HypercallAbi;
use vm_control::DeviceId;
use vm_control::PlatformDeviceId;

use crate::BusAccessInfo;
use crate::BusDevice;
use crate::BusDeviceSync;
use crate::Suspendable;

// PSCI function IDs — SMCCC 32-bit fast calls (0x84xxxxxx).
const PSCI_VERSION: u32 = 0x84000000;
const PSCI_CPU_SUSPEND_32: u32 = 0x84000001;
const PSCI_CPU_OFF: u32 = 0x84000002;
const PSCI_CPU_ON_32: u32 = 0x84000003;
const PSCI_AFFINITY_INFO_32: u32 = 0x84000004;
const PSCI_MIGRATE_INFO_TYPE: u32 = 0x84000006;
const PSCI_SYSTEM_OFF: u32 = 0x84000008;
const PSCI_SYSTEM_RESET: u32 = 0x84000009;
const PSCI_FEATURES: u32 = 0x8400000a;

// PSCI function IDs — SMCCC 64-bit fast calls (0xc4xxxxxx).
const PSCI_CPU_SUSPEND_64: u32 = 0xc4000001;
const PSCI_CPU_ON_64: u32 = 0xc4000003;
const PSCI_AFFINITY_INFO_64: u32 = 0xc4000004;

// PSCI return values (signed, per spec).
const PSCI_SUCCESS: i32 = 0;
const PSCI_NOT_SUPPORTED: i32 = -1;
const PSCI_INVALID_PARAMETERS: i32 = -2;
const PSCI_ALREADY_ON: i32 = -4;

// PSCI_VERSION returns major.minor packed as (major << 16) | minor.
const PSCI_VERSION_1_0: usize = 0x00010000;

// PSCI_MIGRATE_INFO_TYPE: Trusted OS not present on any core.
const TOS_NOT_PRESENT_MP: usize = 2;

/// Exit request values stored in the shared atomic flag.
pub const PSCI_EXIT_NONE: u8 = 0;
pub const PSCI_EXIT_SHUTDOWN: u8 = 1;
pub const PSCI_EXIT_RESET: u8 = 2;

/// Result of a CPU_ON attempt.
pub enum CpuOnResult {
    Success,
    AlreadyOn,
    InvalidParameters,
}

/// Callback for PSCI CPU_ON: (target_cpu_mpidr, entry_point, context_id) -> result.
pub type CpuOnCallback = Arc<dyn Fn(u64, u64, u64) -> CpuOnResult + Send + Sync>;

/// Callback for PSCI SYSTEM_OFF/RESET: cancel all vCPUs.
pub type SystemOffCallback = Arc<dyn Fn() + Send + Sync>;

/// PSCI device that handles ARM Power State Coordination Interface calls.
pub struct PsciDevice {
    exit_request: Arc<AtomicU8>,
    cpu_on_callback: Option<CpuOnCallback>,
    system_off_callback: Option<SystemOffCallback>,
}

impl PsciDevice {
    /// 32-bit PSCI function IDs: 0x84000000..0x84000010.
    pub const HVC32_FID_RANGE: Range<u32> = 0x8400_0000..0x8400_0010;
    /// 64-bit PSCI function IDs: 0xC4000000..0xC4000010.
    pub const HVC64_FID_RANGE: Range<u32> = 0xC400_0000..0xC400_0010;

    pub fn new(exit_request: Arc<AtomicU8>) -> Self {
        Self {
            exit_request,
            cpu_on_callback: None,
            system_off_callback: None,
        }
    }

    pub fn with_cpu_on_callback(mut self, cb: CpuOnCallback) -> Self {
        self.cpu_on_callback = Some(cb);
        self
    }

    pub fn with_system_off_callback(mut self, cb: SystemOffCallback) -> Self {
        self.system_off_callback = Some(cb);
        self
    }

    fn psci_return(val: i32) -> usize {
        val as u32 as usize
    }
}

impl BusDevice for PsciDevice {
    fn device_id(&self) -> DeviceId {
        PlatformDeviceId::Psci.into()
    }

    fn debug_label(&self) -> String {
        "PsciDevice".to_owned()
    }

    fn handle_hypercall(&self, abi: &mut HypercallAbi) -> anyhow::Result<()> {
        let fid = abi.hypercall_id() as u32;
        let regs = match fid {
            PSCI_VERSION => {
                [PSCI_VERSION_1_0, 0, 0, 0]
            }
            PSCI_MIGRATE_INFO_TYPE => {
                [TOS_NOT_PRESENT_MP, 0, 0, 0]
            }
            PSCI_FEATURES => {
                let feature_id = *abi.get_argument(0).unwrap_or(&0) as u32;
                let supported = matches!(
                    feature_id,
                    PSCI_VERSION
                        | PSCI_CPU_ON_32
                        | PSCI_CPU_ON_64
                        | PSCI_CPU_OFF
                        | PSCI_SYSTEM_OFF
                        | PSCI_SYSTEM_RESET
                        | PSCI_FEATURES
                        | PSCI_MIGRATE_INFO_TYPE
                );
                if supported {
                    [PSCI_SUCCESS as usize, 0, 0, 0]
                } else {
                    [Self::psci_return(PSCI_NOT_SUPPORTED), 0, 0, 0]
                }
            }
            PSCI_CPU_OFF => {
                [PSCI_SUCCESS as usize, 0, 0, 0]
            }
            PSCI_CPU_ON_32 | PSCI_CPU_ON_64 => {
                let target_cpu = *abi.get_argument(0).unwrap_or(&0) as u64;
                let entry_point = *abi.get_argument(1).unwrap_or(&0) as u64;
                let context_id = *abi.get_argument(2).unwrap_or(&0) as u64;
                if let Some(ref cb) = self.cpu_on_callback {
                    match cb(target_cpu, entry_point, context_id) {
                        CpuOnResult::Success => [PSCI_SUCCESS as usize, 0, 0, 0],
                        CpuOnResult::AlreadyOn => [Self::psci_return(PSCI_ALREADY_ON), 0, 0, 0],
                        CpuOnResult::InvalidParameters => {
                            [Self::psci_return(PSCI_INVALID_PARAMETERS), 0, 0, 0]
                        }
                    }
                } else {
                    [Self::psci_return(PSCI_NOT_SUPPORTED), 0, 0, 0]
                }
            }
            PSCI_CPU_SUSPEND_32 | PSCI_CPU_SUSPEND_64 => {
                [Self::psci_return(PSCI_NOT_SUPPORTED), 0, 0, 0]
            }
            PSCI_AFFINITY_INFO_32 | PSCI_AFFINITY_INFO_64 => {
                [Self::psci_return(PSCI_NOT_SUPPORTED), 0, 0, 0]
            }
            PSCI_SYSTEM_OFF => {
                info!("PSCI: SYSTEM_OFF requested");
                self.exit_request
                    .store(PSCI_EXIT_SHUTDOWN, Ordering::Release);
                if let Some(ref cb) = self.system_off_callback {
                    cb();
                }
                [PSCI_SUCCESS as usize, 0, 0, 0]
            }
            PSCI_SYSTEM_RESET => {
                info!("PSCI: SYSTEM_RESET requested");
                self.exit_request.store(PSCI_EXIT_RESET, Ordering::Release);
                if let Some(ref cb) = self.system_off_callback {
                    cb();
                }
                [PSCI_SUCCESS as usize, 0, 0, 0]
            }
            _ => {
                bail!("PsciDevice: unknown PSCI function {fid:#x}");
            }
        };
        abi.set_results(&regs);
        Ok(())
    }

    fn read(&mut self, _info: BusAccessInfo, _data: &mut [u8]) {
        unimplemented!("PsciDevice: MMIO read not supported");
    }

    fn write(&mut self, _info: BusAccessInfo, _data: &[u8]) {
        unimplemented!("PsciDevice: MMIO write not supported");
    }
}

impl BusDeviceSync for PsciDevice {
    fn read(&self, _info: BusAccessInfo, _data: &mut [u8]) {
        unimplemented!("PsciDevice: MMIO read not supported");
    }

    fn write(&self, _info: BusAccessInfo, _data: &[u8]) {
        unimplemented!("PsciDevice: MMIO write not supported");
    }
}

impl Suspendable for PsciDevice {}
