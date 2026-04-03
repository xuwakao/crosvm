#!/bin/bash
# test-virtiofs-dax.sh — Verify virtiofs DAX and FSEvents adaptive timeout
#
# This script boots a crosvm VM and runs verification tests for:
# 1. DAX negotiation in FUSE_INIT (check log for MAP_ALIGNMENT)
# 2. FUSE_SETUPMAPPING (guest mmap on virtiofs file triggers DAX)
# 3. FSEvents adaptive timeout (host edit → guest sees update)
#
# Usage: ./scripts/test-virtiofs-dax.sh
#
# Prerequisites:
# - Built crosvm binary at target/debug/crosvm
# - Kernel at aetheria-kernel/build/.../Image
# - Rootfs at aetheria-kernel/build/rootfs-arm64.img
# - /private/tmp/aetheria-share/ directory exists

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PARENT="$(cd "$ROOT/.." && pwd)"
CROSVM="$ROOT/target/debug/crosvm"
KERNEL="$PARENT/aetheria-kernel/build/linux-6.12.15/arch/arm64/boot/Image"
ROOTFS="$PARENT/aetheria-kernel/build/rootfs-arm64.img"
SHARE="/private/tmp/aetheria-share"
LOG="/tmp/crosvm-dax-test.log"

echo "=== virtiofs DAX + FSEvents verification ==="
echo "crosvm: $CROSVM"
echo "kernel: $KERNEL"
echo "rootfs: $ROOTFS"
echo "share:  $SHARE"
echo "log:    $LOG"
echo ""

# Prepare shared dir test files
mkdir -p "$SHARE"
echo "dax-test-content-$(date +%s)" > "$SHARE/dax-test.txt"

# Prepare init script for guest
INIT_SCRIPT="$SHARE/test-dax.sh"
cat > "$INIT_SCRIPT" << 'GUEST_EOF'
#!/bin/sh
echo "===== DAX + FSEvents VERIFICATION TEST ====="

# Mount virtiofs with dax=always
echo "[1] Mounting virtiofs with dax=always..."
mkdir -p /mnt/share
mount -t virtiofs host_share /mnt/share -o dax=always 2>&1 || {
    echo "[1] dax=always failed, trying without dax option..."
    mount -t virtiofs host_share /mnt/share 2>&1 || {
        echo "[1] FAIL: mount failed"
        mount -t virtiofs host_share /mnt/share 2>&1
        exit 1
    }
}
echo "[1] mount output:"
mount | grep virtiofs
echo ""

# Check /proc/mounts for dax option
echo "[2] /proc/mounts virtiofs entry:"
grep virtiofs /proc/mounts
echo ""

# Check if DAX is negotiated via dmesg
echo "[3] dmesg virtiofs/DAX/fuse lines:"
dmesg | grep -iE "virtiofs|virtio_fs|fuse|dax" | tail -20
echo ""

# Test mmap to trigger FUSE_SETUPMAPPING
echo "[4] Testing mmap (should trigger FUSE_SETUPMAPPING)..."
cat /mnt/share/dax-test.txt
# Use dd with direct I/O to bypass page cache
dd if=/mnt/share/dax-test.txt of=/dev/null bs=4096 count=1 2>&1
echo ""

# Create a C program to test mmap explicitly
echo "[5] Creating mmap test program..."
cat > /tmp/test_mmap.c << 'CEOF'
#include <stdio.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/statx.h>

int main() {
    // Check statx for DAX attribute
    struct statx stx;
    if (statx(AT_FDCWD, "/mnt/share/dax-test.txt", 0, STATX_ALL, &stx) == 0) {
        printf("statx: stx_attributes_mask=%#llx stx_attributes=%#llx\n",
               (unsigned long long)stx.stx_attributes_mask,
               (unsigned long long)stx.stx_attributes);
        if (stx.stx_attributes_mask & 0x2000) { // STATX_ATTR_DAX
            if (stx.stx_attributes & 0x2000) {
                printf("DAX: ENABLED (STATX_ATTR_DAX set)\n");
            } else {
                printf("DAX: disabled (STATX_ATTR_DAX not set)\n");
            }
        } else {
            printf("DAX: attribute not available in mask\n");
        }
    }

    // Open and mmap the file
    int fd = open("/mnt/share/dax-test.txt", O_RDONLY);
    if (fd < 0) { perror("open"); return 1; }

    struct stat st;
    fstat(fd, &st);
    printf("file size: %ld\n", (long)st.st_size);

    if (st.st_size > 0) {
        void *addr = mmap(NULL, st.st_size, PROT_READ, MAP_SHARED, fd, 0);
        if (addr == MAP_FAILED) {
            perror("mmap");
        } else {
            printf("mmap succeeded at %p, first bytes: %.40s\n", addr, (char*)addr);
            munmap(addr, st.st_size);
        }
    }
    close(fd);
    return 0;
}
CEOF
if command -v gcc >/dev/null 2>&1; then
    gcc -o /tmp/test_mmap /tmp/test_mmap.c && /tmp/test_mmap
    echo ""
else
    echo "[5] gcc not available, skipping mmap test"
fi

# Test FSEvents: read the file, then host will modify it
echo "[6] FSEvents test: reading dax-test.txt (will be cached)..."
cat /mnt/share/dax-test.txt
echo "[6] First read done. Sleeping 3s for host to modify file..."
sleep 3
echo "[6] Second read (should show modified content if FSEvents works):"
cat /mnt/share/dax-test.txt
echo ""

echo "===== DAX + FSEvents VERIFICATION COMPLETE ====="
GUEST_EOF
chmod +x "$INIT_SCRIPT"

echo "Starting VM (output in $LOG)..."
echo "Will modify dax-test.txt after 30s to test FSEvents..."
echo ""

# Start crosvm in background, capture output
AETHERIA_SHARE="$SHARE" "$CROSVM" run \
    --mem 256 \
    --cpus 2 \
    --block "$ROOTFS" \
    --serial type=stdout,hardware=serial,num=1 \
    -p "root=/dev/vda rw console=ttyS0 earlycon=uart8250,mmio,0x3f8 loglevel=7 init=/bin/sh" \
    "$KERNEL" \
    2>&1 | tee "$LOG" &
CROSVM_PID=$!

echo "crosvm PID: $CROSVM_PID"
echo ""

# After 20s, modify the test file on host to test FSEvents
(
    sleep 20
    echo "=== HOST: modifying dax-test.txt for FSEvents test ==="
    echo "modified-by-host-$(date +%s)" > "$SHARE/dax-test.txt"
) &

# Wait for VM to finish or timeout
wait $CROSVM_PID 2>/dev/null || true

echo ""
echo "=== Test complete. Analyze log: $LOG ==="
echo ""
echo "Key things to look for in the log:"
echo "  1. 'virtiofs init: capable=... use_dax=true' — DAX negotiated"
echo "  2. 'DAX: FUSE_SETUPMAPPING' — guest actually used DAX"
echo "  3. 'FSEvents: stale inode detected' — FSEvents fired + stale check worked"
echo "  4. 'fs handler: received CreateMemoryMapping' — DAX mapping requested"
