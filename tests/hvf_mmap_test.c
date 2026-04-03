// hvf_mmap_test.c — Test whether hv_vm_map accepts file-backed MAP_SHARED
// with and without mlock.
//
// Build: clang -framework Hypervisor -o hvf_mmap_test tests/hvf_mmap_test.c
// Run:   ./hvf_mmap_test
//
// Must be signed with com.apple.security.hypervisor entitlement.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <signal.h>
#include <sys/mman.h>
#include <Hypervisor/Hypervisor.h>

static void sighandler(int sig) {
    const char msg[] = "\nSIGNAL received, aborting\n";
    write(2, msg, sizeof(msg)-1);
    _exit(128 + sig);
}

#define PAGE_SIZE   16384   // ARM64 macOS page size
#define MAP_SIZE    (2 * 1024 * 1024)  // 2MB (same as FUSE_SETUPMAPPING)
#define GUEST_BASE  0x200000000ULL     // Same address as DAX window

static const char *hv_strerror(hv_return_t ret) {
    switch (ret) {
        case HV_SUCCESS:       return "HV_SUCCESS";
        case HV_ERROR:         return "HV_ERROR";
        case HV_BUSY:          return "HV_BUSY";
        case HV_BAD_ARGUMENT:  return "HV_BAD_ARGUMENT";
        case HV_NO_RESOURCES:  return "HV_NO_RESOURCES";
        case HV_NO_DEVICE:     return "HV_NO_DEVICE";
        case HV_DENIED:        return "HV_DENIED";
        case HV_UNSUPPORTED:   return "HV_UNSUPPORTED";
        default:               return "UNKNOWN";
    }
}

typedef struct {
    const char *name;
    void *addr;
    size_t size;
} test_case;

int main(void) {
    signal(SIGBUS, sighandler);
    signal(SIGSEGV, sighandler);
    setbuf(stdout, NULL);  // unbuffered for crash debugging

    printf("=== HVF hv_vm_map MAP_SHARED test ===\n\n");

    // Create HVF VM
    hv_return_t ret = hv_vm_create(NULL);
    if (ret != HV_SUCCESS) {
        fprintf(stderr, "hv_vm_create failed: %s (%#x)\n", hv_strerror(ret), ret);
        return 1;
    }
    printf("VM created OK\n\n");

    // Create a temp file with known content
    char tmppath[] = "/tmp/hvf_mmap_test_XXXXXX";
    int fd = mkstemp(tmppath);
    if (fd < 0) { perror("mkstemp"); return 1; }

    // Extend file to MAP_SIZE
    if (ftruncate(fd, MAP_SIZE) < 0) { perror("ftruncate"); return 1; }

    // Write test pattern
    const char *pattern = "HVF-DAX-TEST-PATTERN";
    pwrite(fd, pattern, strlen(pattern), 0);

    printf("Test file: %s (%d bytes)\n\n", tmppath, MAP_SIZE);

    // ── Test cases ──
    // Build test mappings one at a time with logging.

    test_case tests[6];
    int n = 0;

    printf("Preparing test mappings...\n");

    // 1. Control: MAP_PRIVATE|ANONYMOUS (known to work with hv_vm_map)
    printf("  [1] MAP_PRIVATE|ANON...\n");
    void *priv_anon = mmap(NULL, MAP_SIZE, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (priv_anon != MAP_FAILED) {
        pread(fd, priv_anon, MAP_SIZE, 0);
        tests[n++] = (test_case){"MAP_PRIVATE|ANON + pread (control)", priv_anon, MAP_SIZE};
    } else { perror("  mmap priv_anon"); }

    // 2. MAP_SHARED, no preparation
    printf("  [2] MAP_SHARED raw...\n");
    void *shared_raw = mmap(NULL, MAP_SIZE, PROT_READ, MAP_SHARED, fd, 0);
    if (shared_raw != MAP_FAILED) {
        tests[n++] = (test_case){"MAP_SHARED (raw, no mlock)", shared_raw, MAP_SIZE};
    } else { perror("  mmap shared_raw"); }

    // 3. MAP_SHARED + mlock
    printf("  [3] MAP_SHARED + mlock...\n");
    void *shared_locked = mmap(NULL, MAP_SIZE, PROT_READ, MAP_SHARED, fd, 0);
    if (shared_locked != MAP_FAILED) {
        int ml = mlock(shared_locked, MAP_SIZE);
        printf("      mlock ret=%d\n", ml);
        if (ml == 0) {
            tests[n++] = (test_case){"MAP_SHARED + mlock", shared_locked, MAP_SIZE};
        } else {
            perror("      mlock");
            tests[n++] = (test_case){"MAP_SHARED (mlock FAILED)", shared_locked, MAP_SIZE};
        }
    } else { perror("  mmap shared_locked"); }

    // 4. MAP_SHARED + fault all pages + mlock
    printf("  [4] MAP_SHARED + fault + mlock...\n");
    void *shared_faulted = mmap(NULL, MAP_SIZE, PROT_READ, MAP_SHARED, fd, 0);
    if (shared_faulted != MAP_FAILED) {
        volatile char sum = 0;
        for (size_t i = 0; i < MAP_SIZE; i += PAGE_SIZE) {
            sum += ((volatile char *)shared_faulted)[i];
        }
        mlock(shared_faulted, MAP_SIZE);
        tests[n++] = (test_case){"MAP_SHARED + fault-all + mlock", shared_faulted, MAP_SIZE};
    } else { perror("  mmap shared_faulted"); }

    // 5. MAP_SHARED RW + mlock
    printf("  [5] MAP_SHARED RW + mlock...\n");
    void *shared_rw = mmap(NULL, MAP_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (shared_rw != MAP_FAILED) {
        mlock(shared_rw, MAP_SIZE);
        tests[n++] = (test_case){"MAP_SHARED RW + mlock", shared_rw, MAP_SIZE};
    } else { perror("  mmap shared_rw"); }

    // 6. MAP_PRIVATE (file-backed, not anonymous) + mlock
    printf("  [6] MAP_PRIVATE file-backed + mlock...\n");
    void *priv_file = mmap(NULL, MAP_SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
    if (priv_file != MAP_FAILED) {
        mlock(priv_file, MAP_SIZE);
        tests[n++] = (test_case){"MAP_PRIVATE file-backed + mlock", priv_file, MAP_SIZE};
    } else { perror("  mmap priv_file"); }

    // Run tests
    printf("Running %d test cases...\n\n", n);
    uint64_t guest_addr = GUEST_BASE;

    for (int i = 0; i < n; i++) {
        printf("Test %d: %-45s → ", i + 1, tests[i].name);
        fflush(stdout);

        ret = hv_vm_map(tests[i].addr, guest_addr, tests[i].size, HV_MEMORY_READ);
        printf("%s (%#x)\n", hv_strerror(ret), ret);

        if (ret == HV_SUCCESS) {
            // Verify we can read the pattern back (would need a vCPU, skip for now)
            printf("       Mapped at guest %#llx, verifying unmap...\n", guest_addr);
            hv_return_t uret = hv_vm_unmap(guest_addr, tests[i].size);
            printf("       Unmap: %s\n", hv_strerror(uret));
        }

        guest_addr += MAP_SIZE;  // different guest addr for each test
    }

    // Cleanup
    printf("\nCleaning up...\n");
    for (int i = 0; i < n; i++) {
        munmap(tests[i].addr, tests[i].size);
    }
    close(fd);
    unlink(tmppath);
    hv_vm_destroy();

    printf("Done.\n");
    return 0;
}
