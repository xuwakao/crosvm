# Progress: vhost-user-fs on macOS/HVF

Created: 2026-04-02
Source: [plan/vhost-user-fs]

## Log

### [2026-04-02T02:00] META-PHASE A — Feasibility Analysis

Investigated vhost-user-fs feasibility on macOS/HVF.

**Action**: Analyzed HVF's ioeventfd implementation, crosvm's existing
vhost-user infrastructure, and the vhost-user protocol's dependency on
kernel-level I/O routing.

**Result**: DEPRECATED — vhost-user-fs is not beneficial on HVF.

**Evidence**:
- HvfVm::register_ioevent (vm.rs:376-385): software HashMap, not hardware-accelerated
- HvfVm::handle_io_events (vm.rs:397-403): manual evt.signal() on every MMIO trap
- KVM ioeventfd: routes MMIO writes directly to eventfd in kernel, no VM exit
- Without ioeventfd, vhost-user adds IPC overhead without removing VM exits

**Cross-ref**: [plan/vhost-user-fs#F-001], [plan/vhost-user-fs#F-002]

Plan marked DEPRECATED. No implementation needed.

## Plan Corrections

N/A (plan deprecated before execution)

## Findings

### F-001: HVF performance ceiling is architectural
The fundamental performance limitation of virtiofs on HVF is that every
virtio I/O operation requires a VM exit. This cannot be solved by any
userspace optimization (vhost-user, batching, buffering). It requires
either hardware support (ioeventfd equivalent) or kernel-level virtio
handling (VZ framework).

This finding explains why our FUSE-path performance (5-38% native) is
similar to QEMU virtiofsd on KVM (14-26% native) despite different
architectures — both are limited by per-I/O VM exit overhead.
