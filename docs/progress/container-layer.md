# Progress: Container Layer

Created: 2026-04-02
Source: [plan/container-layer]

## Log

### [2026-04-02T03:00] META-PHASE A — Planning
Researched container implementation approaches. Key findings:
- Alpine host has no systemd → systemd-nspawn not available
- Custom Go runtime using unshare+pivot_root is the lightest option
- OrbStack/WSL2 pattern: single kernel + namespaced containers
- Agent already has vsock JSON-RPC; extend with container.* methods
- Kernel already has CONFIG_OVERLAY_FS=y, cgroups v2, all namespaces

### Plan Review

| # | Item | Verdict | Evidence |
|---|------|---------|----------|
| 1 | Dependency graph | PASS | Phase 1→2→3→4 linear, no cycles |
| 2 | Phase 1 precision | PASS | Verifiable: go build, container.create RPC, /bin/sh runs in namespace |
| 3 | Phase 1 feasibility | RISK | Go+unshare requires runtime.LockOSThread, pivot_root setup is tricky. Mitigated by well-documented patterns (DEV community, containerd source) |
| 4 | Phase 2 precision | PASS | Verifiable: download URL, extract, overlay mount, container uses it |
| 5 | Phase 2 feasibility | PASS | wget available in VM, overlay mount confirmed in kernel config |
| 6 | Phase 3 precision | PASS | Verifiable: ping from container through NAT |
| 7 | Phase 3 feasibility | RISK | Needs iptables + iproute2 in Alpine host. Alpine has both in apk. Need to add to rootfs build. |
| 8 | Phase 4 precision | PASS | CLI commands are concrete and testable |
| 9 | Alternatives | PASS | systemd-nspawn, LXC, containerd properly evaluated and rejected with rationale |

Risks mitigated:
- Phase 1 Go+unshare: will use runtime.LockOSThread pattern from containerd
- Phase 3 iptables: will add to rootfs build script if missing

## Plan Corrections

## Findings
