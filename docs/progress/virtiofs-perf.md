# Progress: virtiofs performance optimization

Created: 2026-04-02
Source: [plan/virtiofs-perf]

## Log

### [2026-04-02T06:00] META-PHASE A — Planning
Analyzed benchmark data. Identified 3 optimizations: dax=inode, multi-worker, interrupt batching.
Selected dax=inode + multi-worker as first two phases.

### Plan Review

| # | Item | Verdict | Evidence |
|---|------|---------|----------|
| 1 | Dependency validation | PASS | Phase 1→2→3 linear, no cycles |
| 2 | Phase 1 expected results | PASS | Measurable: 4K write MB/s, seq read GB/s |
| 3 | Phase 1 feasibility | RISK | dax=inode requires kernel support + FUSE server cooperation. Need to verify FUSE_ATTR_DAX flag handling in passthrough.rs |
| 4 | Phase 2 expected results | PASS | Measurable: benchmark before/after |
| 5 | Phase 2 feasibility | PASS | num_workers is a config change, worker threads already spawn per-queue |
| 6 | Phase 3 expected results | PASS | Final measurement |
| 7 | Risk: dax=inode kernel support | RISK | CONFIG_FUSE_DAX=y in kernel config. dax=inode mount option available since Linux 5.14. Our kernel is 6.12.15 — should be fine. Need to verify FUSE protocol flag FUSE_ATTR_DAX (0x20000000). |
| 8 | Alternatives completeness | PASS | Rejected approaches documented with rationale |

Risk mitigation for Phase 1: if dax=inode fails, test without dax entirely (FUSE-only + writeback) as comparison point.

## Plan Corrections

## Findings
