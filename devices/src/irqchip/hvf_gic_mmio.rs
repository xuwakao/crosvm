// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// GICv3 Distributor and Redistributor MMIO emulation for HVF.
// Provides minimal register emulation so the Linux kernel can detect
// and initialize the GIC, enabling interrupt routing and arch_timer.

use std::sync::Arc;

use base::error;
use sync::Mutex;

use crate::BusDevice;
use crate::Suspendable;
use vm_control::DeviceId;

// GIC Distributor register offsets (from ARM GIC Architecture Specification)
const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_IIDR: u64 = 0x0008;
const GICD_ISENABLER_BASE: u64 = 0x0100;
const GICD_ICENABLER_BASE: u64 = 0x0180;
const GICD_ISPENDR_BASE: u64 = 0x0200;
const GICD_ICPENDR_BASE: u64 = 0x0280;
const GICD_IPRIORITYR_BASE: u64 = 0x0400;
const GICD_ICFGR_BASE: u64 = 0x0C00;
const GICD_IROUTER_BASE: u64 = 0x6100;
const GICD_PIDR2: u64 = 0xFFE8;

// GIC Redistributor register offsets
const GICR_CTLR: u64 = 0x0000;
const GICR_IIDR: u64 = 0x0004;
const GICR_TYPER: u64 = 0x0008;
const GICR_WAKER: u64 = 0x0014;
const GICR_PIDR2: u64 = 0xFFE8;
// SGI base (offset 0x10000 from redistributor frame start)
const GICR_SGI_OFFSET: u64 = 0x10000;
const GICR_ISENABLER0: u64 = GICR_SGI_OFFSET + 0x0100;
const GICR_ICENABLER0: u64 = GICR_SGI_OFFSET + 0x0180;
const GICR_IPRIORITYR_BASE: u64 = GICR_SGI_OFFSET + 0x0400;

// PIDR2 value: ArchRev=3 (bits [7:4] = 0x3) → GICv3
const GICD_PIDR2_VALUE: u32 = 0x3B; // ArchRev=3, rest=implementation defined

/// GICv3 Distributor MMIO device.
/// Responds to MMIO reads/writes at the GICD address range.
pub struct GicDistributor {
    ctlr: u32,
    num_irqs: u32,
    enabled_irqs: [u32; 2], // 64 IRQs in 2 x 32-bit words
}

impl GicDistributor {
    pub fn new(num_irqs: u32) -> Self {
        GicDistributor {
            ctlr: 0,
            num_irqs,
            enabled_irqs: [0; 2],
        }
    }
}

impl Suspendable for GicDistributor {}

impl BusDevice for GicDistributor {
    fn device_id(&self) -> DeviceId {
        DeviceId::PciDeviceId(vm_control::PciId::new(0, 0))
    }

    fn debug_label(&self) -> String {
        "GICv3 Distributor".to_string()
    }

    fn read(&mut self, _info: crate::BusAccessInfo, data: &mut [u8]) {
        let offset = _info.offset;
        let val: u32 = match offset {
            GICD_CTLR => self.ctlr & !(1 << 31), // RWP (bit 31) always 0 — no pending writes
            GICD_TYPER => {
                // ITLinesNumber = (num_irqs / 32) - 1
                let it_lines = (self.num_irqs / 32).saturating_sub(1);
                // CPUNumber = 0 (single CPU), SecurityExtn = 0
                // MBIS = 0, LPIS = 0, RSS = 0, No1N = 0
                // A3V = 1 (affinity 3 valid for GICv3)
                // IDbits = 9 (10 bit INTID)
                it_lines | (1 << 24) | (9 << 19)
            }
            GICD_IIDR => 0x0100_043B, // Implementor: ARM (0x43B), ProductID: 0x01
            GICD_PIDR2 => GICD_PIDR2_VALUE,
            o if o >= GICD_ISENABLER_BASE && o < GICD_ISENABLER_BASE + 8 => {
                let idx = ((o - GICD_ISENABLER_BASE) / 4) as usize;
                if idx < self.enabled_irqs.len() { self.enabled_irqs[idx] } else { 0 }
            }
            o if o >= GICD_IPRIORITYR_BASE && o < GICD_IPRIORITYR_BASE + 256 => {
                0 // All priorities at 0 (highest)
            }
            o if o >= GICD_ICFGR_BASE && o < GICD_ICFGR_BASE + 32 => {
                0 // All level-triggered
            }
            _ => 0,
        };

        let bytes = val.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    fn write(&mut self, _info: crate::BusAccessInfo, data: &[u8]) {
        let offset = _info.offset;
        let val = match data.len() {
            1 => data[0] as u32,
            2 => u16::from_le_bytes([data[0], data[1]]) as u32,
            4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            _ => 0,
        };

        match offset {
            GICD_CTLR => {
                self.ctlr = val;
            }
            o if o >= GICD_ISENABLER_BASE && o < GICD_ISENABLER_BASE + 8 => {
                let idx = ((o - GICD_ISENABLER_BASE) / 4) as usize;
                if idx < self.enabled_irqs.len() {
                    self.enabled_irqs[idx] |= val; // Set enable bits
                }
            }
            o if o >= GICD_ICENABLER_BASE && o < GICD_ICENABLER_BASE + 8 => {
                let idx = ((o - GICD_ICENABLER_BASE) / 4) as usize;
                if idx < self.enabled_irqs.len() {
                    self.enabled_irqs[idx] &= !val; // Clear enable bits
                }
            }
            _ => {} // Ignore other writes
        }
    }
}

/// GICv3 Redistributor MMIO device.
/// Each vCPU has one redistributor frame (64KB RD_base + 64KB SGI_base = 128KB).
pub struct GicRedistributor {
    vcpu_id: u32,
    num_vcpus: u32,
    waker: u32,
    sgi_enabled: u32,
}

impl GicRedistributor {
    pub fn new(vcpu_id: u32, num_vcpus: u32) -> Self {
        GicRedistributor {
            vcpu_id,
            num_vcpus,
            waker: 0, // Processor is awake (ChildrenAsleep=0, ProcessorSleep=0)
            sgi_enabled: 0,
        }
    }
}

impl Suspendable for GicRedistributor {}

impl BusDevice for GicRedistributor {
    fn device_id(&self) -> DeviceId {
        DeviceId::PciDeviceId(vm_control::PciId::new(0, 0))
    }

    fn debug_label(&self) -> String {
        format!("GICv3 Redistributor {}", self.vcpu_id)
    }

    fn read(&mut self, _info: crate::BusAccessInfo, data: &mut [u8]) {
        let offset = _info.offset;
        let val: u64 = match offset {
            GICR_CTLR => 0, // Disabled, RWP=0
            GICR_IIDR => 0x0100_043B, // Same as distributor
            GICR_TYPER => {
                // Affinity_Value = vcpu_id in Aff0
                // Last = 1 if this is the last redistributor
                let affinity = (self.vcpu_id as u64) << 32;
                let last = if self.vcpu_id == self.num_vcpus - 1 { 1u64 << 4 } else { 0 };
                let processor_number = self.vcpu_id as u64;
                affinity | last | (processor_number << 8)
            }
            GICR_WAKER => self.waker as u64,
            GICR_PIDR2 => GICD_PIDR2_VALUE as u64,
            // SGI frame registers
            GICR_ISENABLER0 => self.sgi_enabled as u64,
            o if o >= GICR_IPRIORITYR_BASE && o < GICR_IPRIORITYR_BASE + 128 => {
                0 // All priorities 0
            }
            _ => 0,
        };

        // GICR_TYPER is 64-bit, others are 32-bit
        if offset == GICR_TYPER && data.len() >= 8 {
            let bytes = val.to_le_bytes();
            data[..8].copy_from_slice(&bytes);
        } else {
            let bytes = (val as u32).to_le_bytes();
            let len = data.len().min(4);
            data[..len].copy_from_slice(&bytes[..len]);
        }
    }

    fn write(&mut self, _info: crate::BusAccessInfo, data: &[u8]) {
        let offset = _info.offset;
        let val = match data.len() {
            1 => data[0] as u32,
            2 => u16::from_le_bytes([data[0], data[1]]) as u32,
            4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            _ => 0,
        };

        match offset {
            GICR_WAKER => {
                // Guest writes ProcessorSleep bit. We always report awake.
                self.waker = val & !0x6; // Clear ChildrenAsleep and ProcessorSleep
            }
            GICR_ISENABLER0 => {
                self.sgi_enabled |= val;
            }
            GICR_ICENABLER0 => {
                self.sgi_enabled &= !val;
            }
            _ => {}
        }
    }
}
