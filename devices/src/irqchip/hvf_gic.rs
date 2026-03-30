// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// Minimal IRQ chip for macOS HVF on AArch64.
// Routes device IRQ events to vCPUs via hv_vcpu_set_pending_interrupt.
// Does not emulate GIC registers — the kernel handles GIC system register traps.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use base::Error;
use base::Event;
use base::Result;
use hypervisor::DeviceKind;
use hypervisor::IrqRoute;
use hypervisor::MPState;
use hypervisor::Vcpu;
use resources::SystemAllocator;
use sync::Mutex;

use crate::Bus;
use crate::IrqChip;
use crate::IrqChipAArch64;
use crate::IrqChipCap;
use crate::IrqEdgeEvent;
use crate::IrqEventIndex;
use crate::IrqEventSource;
use crate::IrqLevelEvent;
use crate::VcpuRunState;

struct IrqEvent {
    gsi: u32,
    trigger_event: Event,
    source: IrqEventSource,
    level_triggered: bool,
}

/// Minimal IRQ chip for HVF on macOS.
///
/// This implementation routes device IRQ events (eventfds) to vCPUs using
/// Apple's `hv_vcpu_set_pending_interrupt` API. The kernel guest handles
/// GIC distributor/redistributor register access via system register traps.
pub struct HvfGicChip {
    num_vcpus: usize,
    // Shared IRQ event list — accessible from IRQ handler thread via clone.
    irq_events: Arc<Mutex<Vec<IrqEvent>>>,
    // Track which IRQs are pending (one bool per GSI).
    pending_irqs: Arc<Mutex<Vec<bool>>>,
    // HVF vCPU handles for cross-thread interrupt injection.
    vcpu_handles: Arc<Mutex<Vec<u64>>>,
}

// Number of SPI (Shared Peripheral Interrupts).
const MAX_IRQS: usize = 288;

impl HvfGicChip {
    pub fn new(num_vcpus: usize) -> Result<Self> {
        Ok(HvfGicChip {
            num_vcpus,
            irq_events: Arc::new(Mutex::new(Vec::new())),
            pending_irqs: Arc::new(Mutex::new(vec![false; MAX_IRQS])),
            vcpu_handles: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Register a vCPU handle for cross-thread interrupt injection.
    /// Called from the vCPU thread after creating the HVF vCPU.
    pub fn set_vcpu_handle(&self, vcpu_id: usize, handle: u64) {
        let mut handles = self.vcpu_handles.lock();
        if handles.len() <= vcpu_id {
            handles.resize(vcpu_id + 1, 0);
        }
        handles[vcpu_id] = handle;
    }
}

impl IrqChip for HvfGicChip {
    fn add_vcpu(&mut self, _vcpu_id: usize, _vcpu: &dyn Vcpu) -> Result<()> {
        // HVF vCPU handles are obtained via downcast in inject_interrupts.
        // We just track the count.
        Ok(())
    }

    fn register_edge_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqEdgeEvent,
        source: IrqEventSource,
    ) -> Result<Option<IrqEventIndex>> {
        let mut events = self.irq_events.lock();
        let index = events.len();
        events.push(IrqEvent {
            gsi: irq,
            trigger_event: irq_event.get_trigger().try_clone()?,
            source,
            level_triggered: false,
        });
        Ok(Some(index))
    }

    fn unregister_edge_irq_event(&mut self, irq: u32, _irq_event: &IrqEdgeEvent) -> Result<()> {
        self.irq_events.lock().retain(|e| e.gsi != irq);
        Ok(())
    }

    fn register_level_irq_event(
        &mut self,
        irq: u32,
        irq_event: &IrqLevelEvent,
        source: IrqEventSource,
    ) -> Result<Option<IrqEventIndex>> {
        let mut events = self.irq_events.lock();
        let index = events.len();
        events.push(IrqEvent {
            gsi: irq,
            trigger_event: irq_event.get_trigger().try_clone()?,
            source,
            level_triggered: true,
        });
        Ok(Some(index))
    }

    fn unregister_level_irq_event(
        &mut self,
        irq: u32,
        _irq_event: &IrqLevelEvent,
    ) -> Result<()> {
        self.irq_events.lock().retain(|e| e.gsi != irq);
        Ok(())
    }

    fn route_irq(&mut self, _route: IrqRoute) -> Result<()> {
        // HVF doesn't have a routing table — IRQs go directly to vCPU 0.
        Ok(())
    }

    fn set_irq_routes(&mut self, _routes: &[IrqRoute]) -> Result<()> {
        Ok(())
    }

    fn irq_event_tokens(&self) -> Result<Vec<(IrqEventIndex, IrqEventSource, Event)>> {
        self.irq_events
            .lock()
            .iter()
            .enumerate()
            .map(|(i, e)| Ok((i, e.source.clone(), e.trigger_event.try_clone()?)))
            .collect()
    }

    fn service_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        let mut pending = self.pending_irqs.lock();
        if (irq as usize) < pending.len() {
            pending[irq as usize] = level;
        }
        Ok(())
    }

    fn service_irq_event(&mut self, event_index: IrqEventIndex) -> Result<()> {
        let events = self.irq_events.lock();
        if let Some(irq_event) = events.get(event_index) {
            let gsi = irq_event.gsi;
            let is_level = irq_event.level_triggered;
            // Clear the event so the IRQ handler doesn't re-enter immediately.
            let _ = irq_event.trigger_event.wait();

            // Mark IRQ as pending.
            let mut pending = self.pending_irqs.lock();
            if (gsi as usize) < pending.len() {
                pending[gsi as usize] = true;
            }
            drop(pending);

            // Inject interrupt via the best available mechanism.
            use hypervisor::hvf::ffi;
            if ffi::hvf_gic_is_available() {
                // macOS 15+: Use native HVF GIC to inject SPI.
                let intid = gsi + ffi::GIC_SPI_BASE;
                let ret = unsafe { ffi::hv_gic_set_spi(intid, true) };
                if ret != ffi::HV_SUCCESS {
                    base::error!("hv_gic_set_spi({}, true) failed: {}", intid, ret);
                }
                if !is_level {
                    // Edge-triggered: deassert immediately to create a rising edge.
                    unsafe { ffi::hv_gic_set_spi(intid, false) };
                }
                // Level-triggered: keep asserted. The GIC will deliver the interrupt
                // to the guest. When the guest EOIs, the GIC deasserts automatically
                // for level-sensitive SPIs configured via hv_gic_set_spi.
            } else {
                // macOS <15: Fallback — inject physical IRQ signal to all vCPUs.
                let handles = self.vcpu_handles.lock();
                for &handle in handles.iter() {
                    if handle != 0 {
                        unsafe {
                            ffi::hv_vcpu_set_pending_interrupt(
                                handle,
                                ffi::HV_INTERRUPT_TYPE_IRQ,
                                true,
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn broadcast_eoi(&self, _vector: u8) -> Result<()> {
        // EOI handled by kernel GIC emulation.
        Ok(())
    }

    fn inject_interrupts(&self, vcpu: &dyn Vcpu) -> Result<()> {
        use hypervisor::hvf::ffi;
        use hypervisor::hvf::vcpu::HvfVcpu;

        let hvf_vcpu: &HvfVcpu = vcpu
            .downcast_ref()
            .expect("HvfGicChip::inject_interrupts called with non-HvfVcpu");

        let mut pending = self.pending_irqs.lock();
        let has_pending = pending.iter().any(|&p| p);
        if has_pending {
            // Inject IRQ signal. The guest GIC will determine which specific interrupt
            // to service based on its own priority/enable state.
            let ret = unsafe {
                ffi::hv_vcpu_set_pending_interrupt(
                    hvf_vcpu.hvf_handle(),
                    ffi::HV_INTERRUPT_TYPE_IRQ,
                    true,
                )
            };
            if ret != ffi::HV_SUCCESS {
                return Err(Error::new(libc::EIO));
            }
            // Clear all pending (edge-triggered: inject once).
            for p in pending.iter_mut() {
                *p = false;
            }
        } else {
            // Deassert IRQ line if nothing pending.
            let ret = unsafe {
                ffi::hv_vcpu_set_pending_interrupt(
                    hvf_vcpu.hvf_handle(),
                    ffi::HV_INTERRUPT_TYPE_IRQ,
                    false,
                )
            };
            if ret != ffi::HV_SUCCESS {
                return Err(Error::new(libc::EIO));
            }
        }
        Ok(())
    }

    fn halted(&self, _vcpu_id: usize) {
        // HVF handles WFI natively.
    }

    fn wait_until_runnable(&self, _vcpu: &dyn Vcpu) -> Result<VcpuRunState> {
        Ok(VcpuRunState::Runnable)
    }

    fn kick_halted_vcpus(&self) {
        // HVF handles this via hv_vcpu_cancel.
    }

    fn get_mp_state(&self, _vcpu_id: usize) -> Result<MPState> {
        Ok(MPState::Runnable)
    }

    fn set_mp_state(&mut self, _vcpu_id: usize, _state: &MPState) -> Result<()> {
        Ok(())
    }

    fn try_clone(&self) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(HvfGicChip {
            num_vcpus: self.num_vcpus,
            irq_events: Arc::clone(&self.irq_events),
            pending_irqs: Arc::clone(&self.pending_irqs),
            vcpu_handles: Arc::clone(&self.vcpu_handles),
        })
    }

    fn finalize_devices(
        &mut self,
        _resources: &mut SystemAllocator,
        _io_bus: &Bus,
        _mmio_bus: &Bus,
    ) -> Result<()> {
        Ok(())
    }

    fn process_delayed_irq_events(&mut self) -> Result<()> {
        Ok(())
    }

    fn irq_delayed_event_token(&self) -> Result<Option<Event>> {
        Ok(None)
    }

    fn check_capability(&self, _c: IrqChipCap) -> bool {
        false
    }
}

impl IrqChipAArch64 for HvfGicChip {
    fn try_box_clone(&self) -> Result<Box<dyn IrqChipAArch64>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn as_irq_chip(&self) -> &dyn IrqChip {
        self
    }

    fn as_irq_chip_mut(&mut self) -> &mut dyn IrqChip {
        self
    }

    fn get_vgic_version(&self) -> DeviceKind {
        DeviceKind::ArmVgicV3
    }

    fn has_vgic_its(&self) -> bool {
        false
    }

    fn finalize(&self) -> Result<()> {
        // HVF doesn't expose GIC device controls. The kernel handles GIC via traps.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hvf_gic_chip_creation() {
        let chip = HvfGicChip::new(4).unwrap();
        assert_eq!(chip.num_vcpus, 4);
        assert_eq!(chip.get_vgic_version(), DeviceKind::ArmVgicV3);
        assert!(!chip.has_vgic_its());
    }

    #[test]
    fn test_irq_event_registration() {
        let mut chip = HvfGicChip::new(1).unwrap();
        let irq_evt = IrqEdgeEvent::new().unwrap();
        let source = IrqEventSource {
            device_id: vm_control::DeviceId::PciDeviceId(vm_control::PciId::new(0, 0)),
            queue_id: 0,
            device_name: "test".to_string(),
        };
        let idx = chip
            .register_edge_irq_event(4, &irq_evt, source)
            .unwrap();
        assert!(idx.is_some());

        let tokens = chip.irq_event_tokens().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].0, 0); // first index
    }

    #[test]
    fn test_service_irq() {
        let mut chip = HvfGicChip::new(1).unwrap();
        chip.service_irq(5, true).unwrap();
        assert!(chip.pending_irqs.lock()[5]);
        chip.service_irq(5, false).unwrap();
        assert!(!chip.pending_irqs.lock()[5]);
    }

    #[test]
    fn test_try_clone() {
        let chip = HvfGicChip::new(2).unwrap();
        let cloned = chip.try_clone().unwrap();
        assert_eq!(cloned.num_vcpus, 2);
        // Shared pending_irqs
        chip.pending_irqs.lock()[10] = true;
        assert!(cloned.pending_irqs.lock()[10]);
    }
}
