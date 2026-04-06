# Progress: Venus In-Process Integration for macOS

Created: 2026-04-06T04:00:00Z
Source: [plan/venus-inprocess-macos]

## Log

### [2026-04-06T04:00:00Z] Planning complete
**Action**: Root cause analysis of Venus context registration gap
**Result**: PASS — root cause identified as missing virgl_context wrapper in in-process path
**Cross-ref**: [plan/venus-inprocess-macos#F-001]
**Notes**: Five prior findings accumulated from previous fix attempts documented in plan

### Plan Review

| Phase | Dependencies OK | Expected Results Testable | Feasibility | Risks Identified | Stub/Real Marked | Verdict |
|-------|----------------|--------------------------|-------------|-----------------|-----------------|---------|
| 1 | [trace: no deps] | Verify: `meson compile -C build` exit 0; all 9 callbacks present in source | Confirmed: vkr_renderer.h exports all needed functions (submit_cmd, submit_fence, create_resource, import_resource, destroy_resource, create_context, destroy_context) | Risk: `get_blob` callback requires mapping vkr_renderer_create_resource output to virgl_context_blob; `retire_fences` has no direct vkr equivalent — may need no-op | All real | PASS |
| 2 | Phase 1 produces compilable virglrenderer → Phase 2 rebuilds + tests → OK | Verify: `vulkaninfo --summary` output contains non-llvmpipe device; `[venus] vkCreateInstance result: 0` in stderr | Confirmed: MoltenVK works on host (`test_vk` returned 0 + 1 device); capset already advertised correctly | Risk: `retire_fences` callback may need eventfd or polling integration; `get_fencing_fd` may need to return -1 | All real | RISK |
| 3 | Phase 2 confirms Venus works → Phase 3 cleanup → OK | Verify: `grep fprintf vkr_instance.c` returns 0 matches; `meson compile` exit 0 | Trivial | None | All real | PASS |

**Dependency graph**: Phase 1 → Phase 2 → Phase 3. Linear, no cycles.

**Alternatives completeness**:
- Approach B (embed virgl_context in vkr_context): Rejected with evidence — vkr_context.h defines its own structure without a virgl_context base field; modifying it affects the render_server path on Linux.
- Approach C (fix proxy fork): Rejected with evidence [F-004] — MoltenVK requires Metal device which is not accessible in forked child under sudo root on macOS.

**Phase 2 RISK mitigation**: The `retire_fences` callback is called by the GPU worker to poll for completed fences. In the proxy path, it reads from shared memory. For in-process Venus, `vkr_renderer_submit_fence` is async and the retirement happens via the `apple_vkr_retire_fence` callback (already registered). The `retire_fences` virgl_context callback can be a no-op if async fence callbacks are used. This is how the proxy path works with `VIRGL_RENDERER_ASYNC_FENCE_CB`. Downgrading to PASS.

## Plan Corrections

### [2026-04-06T04:10:00Z] Starting Phase 1
**Expected Results**: vkr_virgl_context wrapper struct with 9 callbacks delegating to vkr_renderer_* API; meson compile succeeds.

### Review: Phase 1

| # | Expected Result | Actual Result | Evidence | Verdict |
|---|-----------------|---------------|----------|---------|
| 1 | Wrapper struct with 9 callbacks | `apple_vkr_context` struct + 9 callback functions | `nm libvirglrenderer.1.dylib | grep apple_vkr` shows all 10 symbols | PASS |
| 2 | submit_cmd delegates to vkr_renderer_submit_cmd | Implemented at virglrenderer.c apple_vkr_ctx_submit_cmd | Source inspection | PASS |
| 3 | submit_fence delegates to vkr_renderer_submit_fence | Implemented at virglrenderer.c apple_vkr_ctx_submit_fence | Source inspection | PASS |
| 4 | get_blob delegates to vkr_renderer_create_resource | Implemented at virglrenderer.c apple_vkr_ctx_get_blob | Source inspection | PASS |
| 5 | destroy calls vkr_renderer_destroy_context + free | Implemented at virglrenderer.c apple_vkr_ctx_destroy | Source inspection | PASS |
| 6 | attach/detach delegate to import/destroy_resource | Implemented | Source inspection | PASS |
| 7 | Returns &wrapper->base (valid virgl_context *) | apple_vkr_context_create returns &actx->base | Source inspection | PASS |
| 8 | meson compile succeeds | 1 warning (unused proxy_cbs — expected on Apple) | Build output: "1 warning generated" | PASS |

**Overall Verdict**: PASS
**Findings this phase**: 0

### [2026-04-06T04:20:00Z] Starting Phase 2
**Expected Results**: VM boots, Venus capset advertised, vulkaninfo shows host GPU via MoltenVK.

### [2026-04-06T06:00:00Z] Phase 2 — Diagnosis In Progress

**Current state**: Venus protocol partially working:
- VirglRenderer init OK, Venus capset advertised (id=4, max-version=0, max-size=160) ✓
- Venus context creation works (`apple_vkr_context_create` OK) ✓
- Blob creation + mapping works (fixed_blob_mapping + 16KB alignment fix) ✓
- VmMemory handler thread processes RegisterMemory/UnregisterMemory ✓
- Ring thread starts and processes 2 commands (EnumerateInstanceVersion + EnumerateExtensions) ✓
- MoltenVK reports Vulkan 1.4.341 (valid) ✓
- **BUT**: Guest mesa-vulkan-virtio driver returns `-3` (VK_ERROR_INITIALIZATION_FAILED) after these 2 commands, never sends vkCreateInstance

**Hypothesis**: Ring shared memory coherency issue — the host writes responses to the ring extra buffer (via `res->u.data`), but the guest might not see these writes because the HVF mapping (`add_fd_mapping`) creates a SEPARATE mmap from `res->u.data`. Although both are MAP_SHARED of the same fd, the guest's view goes through `hv_vm_map` which might not pick up the host's writes immediately (cache coherency).

**Next step**: Verify whether the guest can read ring responses by adding a known pattern write.

## Findings

### F-006: Apple Silicon 16KB page alignment for hv_vm_map

`hv_vm_map` on Apple Silicon returns `HV_BAD_ARGUMENT` if size is not aligned to 16KB (the host page size). Guest virtio-gpu allocates blob offsets at 4KB granularity. Fix: `fixed_blob_mapping=true` pre-allocates the BAR, then `add_fd_mapping` maps individual blobs (with aligned mmap/hv_vm_map).

### F-007: VmMemoryClient requires active host handler thread

`VmMemoryClient::register_memory` sends `VmMemoryRequest::RegisterMemory` through a Tube and blocks waiting for response. On macOS there's no event loop to handle these. Solution: spawn `gpu_shmem_handler_thread` with a cloned VM + shared SystemAllocator.

### F-008: Venus ring buffer uses shared memory for host-guest communication

Venus commands after the initial `vkCreateRingMESA` go through a shared memory ring buffer, not `VIRTIO_GPU_CMD_SUBMIT_3D`. The ring thread in `vkr_ring_thread` polls the ring buffer and dispatches commands. This is a different path from `submit_cmd`.

### F-010: Venus vk_xml_version protocol mismatch

virglrenderer 1.1.1 had `vk_xml_version = 1.4.307`. Guest mesa 26.0.3 requires >= `1.4.334`.
Fixed by updating `src/venus/venus-protocol/` files from virglrenderer main (1.4.343).
After fix, the protocol version check passes (EnumerateInstanceVersion returns 1.4.341 from MoltenVK, capped to 1.4.341). But guest still fails — suspect ring shared memory coherency.

### F-011: Ring shared memory host-guest coherency (suspected)

After protocol update, Venus ring thread processes 2 commands (EnumerateInstanceVersion + EnumerateExtensionProperties) successfully on host. But guest never sends vkCreateInstance — appears to not see ring responses. Both host and guest access the same fd via separate MAP_SHARED mmaps. HVF mapping should be cache-coherent. Investigating whether the `add_fd_mapping` correctly maps the blob or if there's a page table attribute issue.

### F-012: Ring tail not visible to host — confirmed coherency failure

Ring thread starts, processes no commands. `ring dispatch cmd_type` debug never fires. The host's `vkr_ring_load_tail(ring)` always returns 0. The guest writes to the ring tail via HVF, but the host's mmap (`res->u.data`) never sees the update. This is confirmed by: ring thread started OK, submit_cmd for ring setup succeeds, but ring stays idle.

The failure is in the stage-2 → host mmap coherency. Both the guest (HVF) and host (direct mmap) access the same underlying file via MAP_SHARED. On Apple Silicon with HVF, guest writes to the hv_vm_map'd region should be visible to the host's separate mmap of the same fd. If they aren't, this is a fundamental platform limitation.

**Potential cause**: The `add_fd_mapping` mmaps the fd with `aligned_fd_offset` which might differ from the original mmap offset (0). Or the HVF mapping isn't pointing to the same physical pages as the host mmap.

### F-013: 16KB alignment complete fix — kernel drm_mm + vram mmap check

Three coordinated changes required for 16KB page alignment:
1. Guest kernel: `drm_mm_insert_node_generic` with `ALIGN(size, SZ_16K)` and `SZ_16K` alignment
2. Guest kernel: `virtio_gpu_vram_mmap` — change `vm_size != vram_node.size` to `vm_size > vram_node.size` (allow partial mappings since vram_node is padded)
3. Host crosvm: `resource_create_blob` pads `resource.size` to 16KB (so `VmMemorySource::Descriptor` carries the padded size to `add_fd_mapping`)

With all three, blob offsets are 16KB-aligned and adjacent blobs don't share 16KB HVF pages.

### F-014: Venus vkCreateInstance SUCCESS on macOS!

After all alignment fixes, Venus protocol fully operational:
- Ring shared memory coherent (host reads tail updates from guest) ✓
- `vkCreateInstance` with `VK_KHR_portability_enumeration` returns `VK_SUCCESS` ✓
- MoltenVK creates Vulkan 1.4.341 instance ✓
- Multiple ring commands dispatching (cmd_type=0,1 repeating = EnumerateInstanceVersion, EnumerateExtensionProperties) ✓

### F-015: Venus protocol FULLY WORKING — MoltenVK device filtered by mesa

Venus ring fully operational. Complete command sequence confirmed:
- cmd=137 (EnumerateInstanceVersion) ✓
- cmd=0 (CreateInstance) → VK_SUCCESS ✓
- cmd=2 (EnumeratePhysicalDevices) → device found ✓
- cmd=6 (GetPhysicalDeviceProperties) ✓
- cmd=14 (EnumerateDeviceExtensionProperties) ✓
- cmd=1 (DestroyInstance) → cleanup ✓

The Apple GPU IS enumerated by the host MoltenVK. But mesa's `filter_physical_devices` rejects it because `vn_physical_device_init_renderer_version` or `vn_physical_device_init_renderer_extensions` fails — MoltenVK doesn't meet mesa Venus's minimum Vulkan 1.1 requirements (likely missing a required KHR extension).

### F-016: MoltenVK lacks external_memory_fd — Venus device filtered by mesa

Venus protocol is fully operational. Host enumerates Apple M4 Pro GPU (Vulkan 1.3.334, 131 extensions → 101 after Venus filter). Guest receives device properties and extensions. But `vn_physical_device_init` fails because:

1. `VK_KHR_external_memory_fd = NO` (macOS has no DMA-BUF)
2. `VK_EXT_external_memory_dma_buf = NO`

Venus requires external memory fd for blob resource sharing between host and guest. Without it, the guest can't import GPU memory allocations from the host. This is a fundamental macOS limitation — not a bug.

**Possible solutions (ranked)**:
1. Implement `VK_KHR_external_memory_fd` emulation in virglrenderer using macOS IOSurface or shared memory — complex but correct
2. Use `VK_KHR_external_memory_win32`-style approach with Mach ports — platform-specific
3. Bypass external memory entirely by using shared memory blobs (the Venus `supports_blob_id_0` flag) — already partially done
4. Contribute a macOS backend to mesa's Venus driver that uses IOSurface instead of DMA-BUF

**This is no longer a Venus plumbing issue.** It's a MoltenVK/macOS platform limitation. Possible solutions:
1. Patch virglrenderer to report additional extensions
2. Use a newer MoltenVK that supports more extensions
3. Patch mesa to relax the filter requirements

**Next investigation**: Write a standalone HVF test that:
1. Creates anonymous shared memory file
2. mmap #1 (host "ring thread" side)
3. mmap #2 (for hv_vm_map)
4. hv_vm_map(mmap2_addr, guest_addr, size, RW)
5. Run guest code that writes to guest_addr
6. Check if mmap #1 sees the write

### F-009: virgl_context registration gap (root cause of Phase 1)

The in-process Venus path was missing a `virgl_context` wrapper. Without it, `virgl_context_lookup` returned NULL and context creation failed with ENOMEM. Fixed by creating `apple_vkr_context` struct that delegates to `vkr_renderer_*` API.
