# HVF DAX Constraints (Undocumented)

Empirically determined constraints for Apple Hypervisor.framework's
`hv_vm_map()` when used for virtiofs DAX (file-backed shared memory
mapped into guest IPA space).

## 1. File descriptor must be opened with O_RDWR

`hv_vm_map()` rejects MAP_SHARED mappings where the underlying fd was
opened with `O_RDONLY`, returning `HV_BAD_PARAMETER` (0xfae94001).

This applies even when the guest mapping is read-only (`HV_MEMORY_READ`
only, no `HV_MEMORY_WRITE`).

**Verified**: 2026-04-02 via isolated C test (`tests/hvf_mmap_test.c`).

| fd open flags | mmap prot | hv_vm_map flags | Result |
|---------------|-----------|-----------------|--------|
| O_RDONLY | PROT_READ | HV_MEMORY_READ | **FAIL** (HV_BAD_PARAMETER) |
| O_RDWR | PROT_READ | HV_MEMORY_READ | OK |
| O_RDWR | PROT_READ\|PROT_WRITE | HV_MEMORY_READ | OK |
| O_RDWR | PROT_READ\|PROT_WRITE | HV_MEMORY_READ\|HV_MEMORY_WRITE | OK |

**Fix in crosvm**: `passthrough.rs:set_up_mapping()` forces `O_RDWR` for
DAX fd opens on macOS. `vm.rs:add_fd_mapping()` uses `PROT_READ|PROT_WRITE`
for the MAP_SHARED mmap.

## 2. Cannot partially remap within an existing mapping

If a large region (e.g., 8GB DAX window) is mapped via a single
`hv_vm_map(host, guest, 8GB, flags)` call, subsequent attempts to:

1. `hv_vm_unmap(guest + offset, 2MB)` — **succeeds**
2. `hv_vm_map(new_host, guest + offset, 2MB, flags)` — **FAILS** (HV_BAD_PARAMETER)

This differs from KVM, where `KVM_SET_USER_MEMORY_REGION` can replace
the host backing of arbitrary sub-regions within an existing memory slot.

**Verified**: 2026-04-02 via crosvm runtime test. The DAX window arena
(allocated by `prepare_shared_memory_region`) was mapped as a single 8GB
anonymous region. Subsequent `hv_vm_unmap` of 2MB sub-regions succeeded,
but `hv_vm_map` of file-backed memory at those addresses failed.

**Fix in crosvm**: `prepare_shared_memory_region()` on macOS passes
`MemCacheType::CacheNonCoherent` to `add_memory_region()`, which signals
`HvfVm` to register the region in `mem_regions` (for guest address
tracking) without calling `hv_vm_map`. DAX sub-regions are then mapped
on demand by `add_fd_mapping()`.

## 3. MAP_SHARED file-backed mappings work (with O_RDWR)

Contrary to initial hypothesis, `hv_vm_map` does accept file-backed
`MAP_SHARED` memory. The earlier failures were caused by constraint #1
(O_RDONLY fd) and constraint #2 (pre-mapped arena), not by MAP_SHARED
itself.

With both constraints addressed, true zero-copy DAX works:
- Guest load/store instructions directly access host file page cache
- Guest writes propagate to host filesystem immediately (MAP_SHARED)
- Performance matches QEMU/KVM DAX architecture

## Test reproduction

Build and run the standalone HVF test:

```bash
clang -framework Hypervisor -o hvf_mmap_test tests/hvf_mmap_test.c
codesign --sign - --entitlements entitlements.plist --force hvf_mmap_test
./hvf_mmap_test
```

Requires `com.apple.security.hypervisor` entitlement.
