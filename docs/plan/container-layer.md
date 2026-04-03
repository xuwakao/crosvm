# Plan: Container Layer (P0)

Created: 2026-04-02
Status: ACTIVE
Source: Architecture — "共享内核 + nspawn" model, P0 for product viability

## Task Description

Implement the container layer so users can run Linux distributions (Ubuntu,
Fedora, Alpine) as isolated containers inside the Aetheria VM. This is the
core product feature — without it, Aetheria is just a bare VM.

Target UX: `aetheria run ubuntu` → Ubuntu shell in seconds.

## Architecture

```
macOS Host
  aetheria CLI (Go)
    → "aetheria run ubuntu"
    → downloads rootfs if needed
    → tells agent to create container
    ↕ vsock

Linux VM (Alpine host, shared kernel 6.12.15)
  aetheria-agent (Go, PID in host namespace)
    → receives "container.create" RPC
    → downloads/extracts rootfs via virtiofs shared dir
    → unshare(PID|MNT|UTS|IPC|NET) + pivot_root
    → exec /sbin/init (systemd for Ubuntu, openrc for Alpine)
    → veth pair + bridge networking
    → cgroups v2 resource limits

  ┌─────────────────┐ ┌─────────────────┐
  │ Container: ubuntu│ │ Container: fedora│
  │ /sbin/init (syst)│ │ /sbin/init (syst)│
  │ own PID/NET/MNT  │ │ own PID/NET/MNT  │
  │ veth → br0        │ │ veth → br0        │
  │ 192.168.100.2     │ │ 192.168.100.3     │
  └─────────────────┘ └─────────────────┘
         ↕ br0 bridge ↕
    NAT → virtio-net → macOS → internet
```

## Alternatives & Trade-offs

| Approach | Pros | Cons | Verdict |
|----------|------|------|---------|
| systemd-nspawn | Battle-tested, OrbStack uses it | Alpine has no systemd, need host OS change | Deferred (future) |
| LXC | Native Alpine support | Heavy dependency, complex config | Rejected |
| Custom Go (unshare+pivot_root) | Lightweight, matches agent, no deps | More code, but well-documented | **Selected** |
| containerd | Industry standard | Massive dependency, overkill for MVP | Deferred |

## Phases

### Phase 1: Agent container.create — single container lifecycle

**Objective**: Agent can create, start, and stop a container using Linux
namespaces. No networking yet — just isolated rootfs with PID/MNT/UTS.

**Expected Results**:
- [ ] Agent supports `container.create` RPC (name, rootfs path)
- [ ] Agent calls unshare(CLONE_NEWPID|CLONE_NEWNS|CLONE_NEWUTS) + pivot_root
- [ ] Container runs /bin/sh as PID 1 inside isolated namespace
- [ ] Agent supports `container.exec` to run commands inside container (via nsenter)
- [ ] Agent supports `container.stop` to kill the container process
- [ ] Agent supports `container.list` to show running containers
- [ ] `go build` succeeds for aetheria-agent

**Dependencies**: None (uses existing agent + vsock infrastructure)

**Risks**:
- Go's goroutine model conflicts with unshare (needs LockOSThread)
- pivot_root requires careful mount namespace setup
- Container rootfs must be available inside the VM (via virtiofs or disk)

**Status**: PENDING

### Phase 2: Rootfs image management

**Objective**: Download and manage distro rootfs images. User provides a
distro name, system downloads the cloud image, extracts, and stores it.

**Expected Results**:
- [ ] Agent supports `image.pull` RPC (distro name → download rootfs tarball)
- [ ] Ubuntu 22.04 cloud image downloads and extracts correctly
- [ ] Alpine minirootfs downloads and extracts correctly
- [ ] Images stored in /var/aetheria/images/ with overlay mount support
- [ ] container.create uses overlay mount (lower=base image, upper=container delta)

**Dependencies**: Phase 1

**Risks**:
- Need internet access from VM (virtio-net must work, confirmed)
- Cloud image URLs may change; need stable mirror list
- Overlay mount requires overlayfs kernel support (CONFIG_OVERLAY_FS=y, confirmed)

**Status**: PENDING

### Phase 3: Per-container networking

**Objective**: Each container gets its own network namespace with veth pair
connected to a bridge, NAT to the internet via the VM's eth0.

**Expected Results**:
- [ ] Bridge br0 created on VM startup (192.168.100.1/24)
- [ ] Each container gets a veth pair (veth-NAME ↔ eth0 inside container)
- [ ] Container gets IP via static assignment (192.168.100.N)
- [ ] iptables MASQUERADE for internet access
- [ ] Container can `ping 8.8.8.8` and `apt update`

**Dependencies**: Phase 1, Phase 2

**Risks**:
- iptables may not be available in Alpine initramfs (need to add)
- Bridge creation requires iproute2 tools
- DNS forwarding needs /etc/resolv.conf setup in container

**Status**: PENDING

### Phase 4: Host CLI integration

**Objective**: macOS CLI `aetheria` commands control the full container lifecycle.

**Expected Results**:
- [ ] `aetheria create ubuntu` → pull image + create container
- [ ] `aetheria start ubuntu` → start container
- [ ] `aetheria exec ubuntu -- bash` → exec shell inside container
- [ ] `aetheria list` → show all containers with status
- [ ] `aetheria stop ubuntu` → stop container
- [ ] `aetheria rm ubuntu` → remove container and rootfs delta

**Dependencies**: Phase 1, 2, 3

**Status**: PENDING

## Findings
