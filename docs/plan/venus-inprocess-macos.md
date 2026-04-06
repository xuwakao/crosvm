# Plan: Venus In-Process Integration for macOS

Created: 2026-04-06T04:00:00Z
Status: ACTIVE
Source: User request — "能不能好好研究问题再解决" (Venus 3D acceleration on macOS)

## Task Description

Fix Venus Vulkan forwarding on macOS by creating a proper in-process Venus integration.
The proxy-based approach (fork+exec render_server) fails on macOS because:
1. `SOCK_SEQPACKET` not available (patched to DGRAM)
2. Forked render_server can't create Metal/Vulkan device (no GPU access in child under sudo)
3. The current `#ifdef __APPLE__` in-process path has a **critical context registration gap**

## Root Cause Analysis

The in-process Venus path calls `vkr_renderer_create_context()` which creates a `vkr_context`
and registers it in `vkr_state.contexts`. But it does NOT create a `virgl_context` or register
one in the global `virgl_context_table`. The subsequent `virgl_context_lookup(ctx_id)` returns
NULL, causing context creation to fail with ENOMEM.

Evidence:
- `vkr_renderer.c:121` — `vkr_context_create()` returns `struct vkr_context *` (not `virgl_context`)
- `vkr_renderer.c:125` — `list_addtail(&ctx->head, &vkr_state.contexts)` — only internal list
- `virglrenderer.c:278` — `virgl_context_lookup(ctx_id)` returns NULL
- `virglrenderer.c:294-295` — `if (!ctx) return ENOMEM;`

The proxy path works because `proxy_context_create()` returns `&ctx->base` which IS a `virgl_context`
with proper callbacks (`submit_cmd`, `destroy`, etc.) registered.

## Alternatives & Trade-offs

| Approach | Pros | Cons | Verdict |
|----------|------|------|---------|
| A: Create virgl_context wrapper in virglrenderer.c | Self-contained, no vkr_context changes | Needs to delegate all 9 callbacks; duplicates proxy_context pattern | **Selected** |
| B: Modify vkr_context to embed virgl_context base | Cleaner long-term; single structure | Invasive change to Venus internals; affects all platforms | Rejected — too risky |
| C: Fix proxy fork to work on macOS | Uses existing architecture | MoltenVK can't init Vulkan in forked child under sudo; fundamental | Rejected — infeasible |

## Phases

### Phase 1: Create vkr_virgl_context wrapper

**Objective**: Create a `struct vkr_virgl_context` that embeds a `virgl_context` as its base
and delegates all callbacks to `vkr_renderer_*` functions. The macOS VENUS context creation
path in `virglrenderer.c` should use this wrapper instead of the broken `vkr_renderer_create_context`
+ `virgl_context_lookup` pattern.

**Expected Results**:
- [ ] New wrapper struct defined in virglrenderer.c (macOS-only) with all 9 virgl_context callbacks
- [ ] `submit_cmd` delegates to `vkr_renderer_submit_cmd(ctx_id, buffer, size)`
- [ ] `submit_fence` delegates to `vkr_renderer_submit_fence(ctx_id, flags, ring_idx, fence_id)`
- [ ] `get_blob` delegates to `vkr_renderer_create_resource` + virgl_resource_create
- [ ] `destroy` calls `vkr_renderer_destroy_context(ctx_id)` + free
- [ ] `attach_resource`/`detach_resource` delegate to `vkr_renderer_import_resource`/`vkr_renderer_destroy_resource`
- [ ] Context creation returns `&wrapper->base` (a valid `virgl_context *`)
- [ ] `meson compile -C build` succeeds with zero errors

**Dependencies**: None

**Status**: PENDING

### Phase 2: Build and runtime verification

**Objective**: Rebuild virglrenderer + crosvm, boot VM, verify Venus capset advertised and
`vulkaninfo` shows the host Apple GPU (via MoltenVK) instead of only llvmpipe.

**Expected Results**:
- [ ] `meson compile -C build && meson install -C build` succeeds
- [ ] `cargo build --release` succeeds
- [ ] VM boots with `cap set 0: id 4` in dmesg
- [ ] No "failed to pre-initialize context" errors
- [ ] `vulkaninfo --summary` shows a physical device with `deviceType = PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU` or similar (not just llvmpipe)
- [ ] `[venus] vkCreateInstance result: 0` in crosvm stderr (debug output)

**Dependencies**: Phase 1

**Status**: IN-PROGRESS — ring coherency failure (F-012)

### Phase 3: Cleanup debug logging

**Objective**: Remove temporary fprintf debug statements from vkr_instance.c.

**Expected Results**:
- [ ] Debug fprintf removed from vkr_instance.c
- [ ] Clean build with no warnings

**Dependencies**: Phase 2 (only after verified working)

**Status**: PENDING

## Findings

### F-001: virgl_context registration gap in in-process Venus

The in-process Venus path (`#ifdef __APPLE__` in virglrenderer.c) calls `vkr_renderer_create_context()`
which only creates a `vkr_context` registered in `vkr_state.contexts`. No `virgl_context` is
created or registered in the global table. All subsequent commands fail because
`virgl_context_lookup(ctx_id)` returns NULL.

### F-002: SOCK_SEQPACKET not available on macOS

`socketpair(AF_UNIX, SOCK_SEQPACKET, 0, fds)` returns `EPROTONOSUPPORT` on macOS.
Patched to `SOCK_DGRAM` which also preserves message boundaries for Unix domain sockets.

### F-003: MoltenVK requires portability enumeration

MoltenVK returns `VK_ERROR_INCOMPATIBLE_DRIVER` from `vkCreateInstance` unless
`VK_KHR_portability_enumeration` extension is enabled and
`VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR` flag is set. Patched in vkr_instance.c.

### F-004: Render server fork fails on macOS under sudo

The forked render_server child process cannot create a Metal device / Vulkan instance
because macOS restricts GPU access for processes forked from a sudo'd parent.
This is why the in-process approach is necessary on macOS.

### F-005: XDG_RUNTIME_DIR required for proxy shared memory

`os_create_anonymous_file()` falls through to an XDG_RUNTIME_DIR-based path on macOS
(no memfd_create). sudo strips this env var. Fixed by passing `XDG_RUNTIME_DIR=/tmp`
through the CLI's sudo invocation.
