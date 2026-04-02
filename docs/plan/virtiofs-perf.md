# Plan: virtiofs performance optimization

Created: 2026-04-02
Status: ACTIVE
Source: Benchmark results — virtiofs at 2.8-25% native, target ≥70%

## Task Description

Optimize virtiofs performance on macOS/HVF to approach the ≥70% native target.
Benchmark baseline (2026-04-02):
- Seq write 100MB: 1.7 GB/s (16% native)
- Seq read 100MB first: 4.6 GB/s (26% native)
- Seq read 100MB cached: 32.1 GB/s (180% native) — DAX working
- 4K write: 29.9 MB/s (2.8% native)
- 4K read: 2.9 GB/s (61% native)
- tmpfs 4K write (VM-only overhead): 3.4 GB/s — VM itself is fast

Root cause analysis identified three optimizable bottlenecks:
1. `dax=always` forces all writes through DAX page fault (SETUPMAPPING per page), bypassing writeback coalescing
2. `num_workers=1` — single-threaded FUSE request processing
3. DAX window size 8GB but allocated arena costs host memory

## Alternatives & Trade-offs

| Approach | Pros | Cons | Verdict |
|----------|------|------|---------|
| A: Remove dax=always, use cache=auto only | Writes use FUSE_WRITE + writeback (coalesced); reads use page cache | Loses DAX zero-copy read (32 GB/s → ~4.6 GB/s) | Rejected: sacrifices best feature |
| B: Guest mount dax=inode (server decides per-file) | Best of both: reads via DAX, writes via FUSE writeback | Requires FUSE_INIT negotiation check; kernel may not support dax=inode | **Selected** |
| C: Increase workers only | Parallel FUSE processing | virtiofsd data shows single-thread often faster (lock contention) | Selected (with measurement) |
| D: Batch interrupts | Reduce guest interrupt overhead | Complex, moderate gain | Deferred |

## Phases

### Phase 1: Benchmark dax=always vs no-dax to quantify tradeoff

**Objective**: Mount virtiofs without dax option and measure. This forces all I/O through FUSE read/write with writeback coalescing, quantifying the DAX vs writeback tradeoff. No code changes needed — mount option only.

**Expected Results**:
- [ ] Mount without dax: `mount -t virtiofs host_share /mnt -o relatime`
- [ ] 4K write improves significantly (writeback coalesces → fewer FUSE_WRITE)
- [ ] Sequential read slower (no DAX, FUSE_READ per request)
- [ ] Numbers documented for both configurations side by side

**Dependencies**: None

**Risks**: None (test only, no code change)

**Status**: PENDING

### Phase 1b: Implement FUSE_ATTR_DAX for dax=inode (if warranted)

**Objective**: If Phase 1 shows no-dax writes are much faster, implement `FUSE_ATTR_DAX` in passthrough.rs so `dax=inode` works — the kernel uses DAX for reads (mmap) and FUSE_WRITE for writes (writeback).

**Expected Results**:
- [ ] passthrough.rs LOOKUP/GETATTR sets FUSE_ATTR_DAX on regular files
- [ ] Guest mounts with `dax=inode`
- [ ] Reads use DAX (32 GB/s cached), writes use FUSE writeback
- [ ] Build passes

**Dependencies**: Phase 1 results show significant write improvement without dax

**Risks**:
- FUSE_ATTR_DAX flag value needs to be determined from kernel headers
- dax=inode mount option requires kernel 5.14+ (our 6.12.15 is fine)

**Status**: PENDING

### Phase 2: Increase worker threads and measure

**Objective**: Increase num_workers from 1 to match vCPU count. Measure whether parallel FUSE request processing improves throughput or causes lock contention.

**Expected Results**:
- [ ] `num_workers` set to `vcpu_count` (currently 2)
- [ ] Build passes
- [ ] Benchmark comparison: num_workers=1 vs num_workers=2
- [ ] If contention detected (no improvement or regression), document and revert

**Dependencies**: Phase 1

**Status**: PENDING

### Phase 3: Final benchmark and comparison

**Objective**: Run full benchmark suite after optimizations and compare against baseline, Docker Desktop, and native.

**Expected Results**:
- [ ] Full benchmark: seq read, seq write, 4K read, 4K write, metadata
- [ ] Comparison table: before vs after vs Docker Desktop published numbers
- [ ] Document remaining gaps and architectural limitations

**Dependencies**: Phase 2

**Status**: PENDING

## Findings
