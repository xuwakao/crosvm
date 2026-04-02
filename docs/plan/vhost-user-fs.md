# Plan: vhost-user-fs on macOS/HVF

Created: 2026-04-02
Status: DEPRECATED
Source: virtiofs performance optimization — bypass crosvm mediation

## Task Description

Implement vhost-user protocol for virtiofs on macOS to bypass crosvm's
I/O mediation, potentially improving FUSE-path throughput 2-3x.

## Feasibility Analysis

### How vhost-user works on Linux/KVM (fast path)

```
Guest writes virtio queue doorbell (MMIO)
    ↓ KVM ioeventfd: routes directly to eventfd, NO VM exit
virtiofsd: woken by eventfd → process FUSE → write response
    ↓ KVM irqfd: injects interrupt directly, NO crosvm involvement
Guest: receives interrupt
```

Key enablers: **ioeventfd** (MMIO write → eventfd without VM exit) and
**irqfd** (eventfd → guest interrupt without VMM). These are KVM kernel
features that completely bypass the VMM process.

### How it would work on macOS/HVF (no fast path)

```
Guest writes virtio doorbell (MMIO)
    ↓ ★ VM exit (HVF has no ioeventfd — all MMIO traps go to crosvm)
crosvm vCPU thread: handle_io_events() → evt.signal()
    ↓ ★ VM enter (crosvm re-enters guest)
virtiofsd: woken by eventfd → process FUSE → write response
    ↓ eventfd → crosvm irq handler → inject interrupt
Guest: receives interrupt
```

**HVF does NOT have ioeventfd or irqfd.** Every MMIO write triggers a
VM exit handled by crosvm (vm.rs:397-403 `handle_io_events`). The ioevents
HashMap is a software emulation: crosvm receives the exit, looks up the
eventfd, and signals it manually.

### Impact assessment

| Component | Linux/KVM + vhost-user | macOS/HVF + vhost-user |
|-----------|----------------------|----------------------|
| VM exit on doorbell | **Eliminated** (ioeventfd) | **Still happens** |
| crosvm involvement | **None** at runtime | **Required** for every I/O |
| FUSE processing | Separate process | Separate process |
| Net benefit | 2-5x faster | **Negligible or negative** |

On HVF, vhost-user would:
1. Still require a VM exit for every FUSE request (same as current)
2. Add cross-process IPC overhead (eventfd + shared memory setup)
3. Add process context switch latency (crosvm → virtiofsd)
4. NOT reduce the number of VM exits (the actual bottleneck)

### Current in-process architecture (already optimal for HVF)

```
Guest writes virtio doorbell
    ↓ VM exit
crosvm vCPU thread: WaitContext notification → VM enter immediately
crosvm worker thread: reads virtio queue → processes FUSE → writes response
    ↓ trigger_interrupt()
Guest: receives interrupt
```

The current design already decouples the vCPU thread from FUSE processing.
The worker thread runs independently. Moving it to a separate process adds
overhead without removing VM exits.

## Alternatives & Trade-offs

| Approach | Pros | Cons | Verdict |
|----------|------|------|---------|
| vhost-user-fs separate process | Matches QEMU architecture | HVF has no ioeventfd — VM exit still required, adds IPC overhead | **Rejected** |
| Keep in-process virtiofs | No IPC overhead, already decoupled | Cannot eliminate VM exits | Current (optimal for HVF) |
| Implement ioeventfd in HVF | Would enable true bypass | HVF is Apple's closed API, cannot modify | Impossible |
| Switch to VZ framework | Has kernel-level virtio | Loses GPU, Android, custom devices | Out of scope |

## Conclusion

**vhost-user-fs on macOS/HVF is not beneficial.** The performance gain
of vhost-user comes from ioeventfd/irqfd (KVM kernel features) that
eliminate VM exits. HVF lacks these features. Without them, vhost-user
adds IPC overhead without removing the bottleneck.

The current in-process virtiofs with worker threads is already the
optimal architecture for HVF. Further performance improvements require
either:
1. Apple exposing ioeventfd-equivalent APIs in HVF (unlikely)
2. Switching to VZ framework (sacrifices features)
3. Reducing FUSE request count (already done: DAX, writeback, 4MB buffer)

Superseded-by: N/A (no implementation needed)

## Findings

### F-001: HVF ioeventfd is software emulation only
HvfVm::register_ioevent (vm.rs:376-385) stores events in a HashMap.
handle_io_events (vm.rs:397-403) looks up and signals them manually on
every MMIO trap. This is NOT hardware-accelerated like KVM's ioeventfd
which bypasses the VMM entirely. Every virtio doorbell write on HVF
causes a full VM exit → crosvm → evt.signal() → VM enter cycle.

### F-002: crosvm already decouples vCPU from FUSE processing
The current worker.rs architecture runs FUSE processing in dedicated
threads, separate from vCPU threads. The vCPU thread only routes the
virtio notification (via WaitContext) and re-enters the guest immediately.
Moving this to a separate process would add IPC overhead without reducing
the number of VM exits.

### F-003: vhost-user-fs backend code exists but macOS stubs are empty
crosvm has complete vhost-user-fs backend code in
devices/src/virtio/vhost_user_backend/fs.rs (FsBackend using PassthroughFs).
The macOS platform stubs (fs/sys/macos.rs, connection/sys/macos.rs) are
empty. Implementation would be possible but pointless given F-001.
