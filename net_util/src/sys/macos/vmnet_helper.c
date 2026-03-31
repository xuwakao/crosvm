// vmnet_helper.c — C helper for vmnet.framework on macOS.
// Provides synchronous wrappers around vmnet's block-based API.
//
// Do NOT compile with -fobjc-arc: ARC changes block lifetime semantics and
// causes dispatch_semaphore + vmnet_start_interface to deadlock.
// Manual memory management (MRC) is required.

#include <vmnet/vmnet.h>
#include <dispatch/dispatch.h>
#include <string.h>
#include <stdio.h>
#include <unistd.h>
#include <errno.h>

// Timeout for vmnet_start_interface completion callback (seconds).
// vmnet typically responds within ~200ms on a healthy system.
// 10 seconds accommodates heavily loaded hosts or slow I/O.
#define VMNET_START_TIMEOUT_SEC 10LL

// Timeout for vmnet_stop_interface completion callback (seconds).
// Stop is faster than start; 5 seconds is generous.
#define VMNET_STOP_TIMEOUT_SEC 5LL

// Sentinel value indicating a timeout in vmnet_helper_stop_interface.
// Must be distinct from VMNET_SUCCESS (1000) and valid vmnet error codes.
#define VMNET_HELPER_TIMEOUT 0xFFFFFFFFU

interface_ref vmnet_helper_start_interface(
    uint64_t mode,
    uint32_t *out_status,
    char *out_mac,
    size_t mac_buf_len,
    uint64_t *out_mtu,
    uint64_t *out_max_packet_size
) {
    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, mode);

    dispatch_queue_t queue = dispatch_queue_create(
        "com.aetheria.vmnet", DISPATCH_QUEUE_SERIAL);
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);

    __block vmnet_return_t cb_status = 0;
    __block xpc_object_t cb_params = NULL;

    interface_ref iface = vmnet_start_interface(desc, queue,
        ^(vmnet_return_t status, xpc_object_t interface_param) {
            cb_status = status;
            if (interface_param) {
                cb_params = interface_param;
                xpc_retain(interface_param);
            }
            dispatch_semaphore_signal(sem);
        });

    xpc_release(desc);

    // If vmnet_start_interface returns NULL, the callback will never fire.
    if (!iface) {
        if (out_status) *out_status = 0;
        dispatch_release(sem);
        dispatch_release(queue);
        return NULL;
    }

    long r = dispatch_semaphore_wait(sem,
        dispatch_time(DISPATCH_TIME_NOW, VMNET_START_TIMEOUT_SEC * NSEC_PER_SEC));
    if (r != 0) {
        if (out_status) *out_status = 0;
        vmnet_stop_interface(iface, queue, ^(vmnet_return_t s) {
            (void)s;
        });
        dispatch_release(sem);
        dispatch_release(queue);
        return NULL;
    }

    if (out_status) *out_status = cb_status;

    if (cb_status != VMNET_SUCCESS) {
        vmnet_stop_interface(iface, queue, ^(vmnet_return_t s) {
            (void)s;
        });
        if (cb_params) xpc_release(cb_params);
        dispatch_release(sem);
        dispatch_release(queue);
        return NULL;
    }

    if (cb_params) {
        if (out_mac && mac_buf_len > 0) {
            const char *mac = xpc_dictionary_get_string(
                cb_params, vmnet_mac_address_key);
            if (mac) {
                size_t mac_len = strlen(mac);
                if (mac_len >= mac_buf_len) {
                    // MAC string too long — signal error via empty string.
                    out_mac[0] = '\0';
                } else {
                    memcpy(out_mac, mac, mac_len + 1);
                }
            } else {
                out_mac[0] = '\0';
            }
        }
        if (out_mtu) {
            *out_mtu = xpc_dictionary_get_uint64(cb_params, vmnet_mtu_key);
        }
        if (out_max_packet_size) {
            *out_max_packet_size = xpc_dictionary_get_uint64(
                cb_params, vmnet_max_packet_size_key);
        }
        xpc_release(cb_params);
    }

    dispatch_release(sem);
    // Note: do NOT release queue here — vmnet retains it for the interface lifetime.
    // It will be released in vmnet_helper_stop_interface or by vmnet on teardown.

    return iface;
}

vmnet_return_t vmnet_helper_stop_interface(interface_ref iface) {
    if (!iface) return VMNET_SUCCESS;

    dispatch_queue_t queue = dispatch_queue_create(
        "com.aetheria.vmnet.stop", DISPATCH_QUEUE_SERIAL);
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);

    __block vmnet_return_t stop_status = 0;
    vmnet_stop_interface(iface, queue,
        ^(vmnet_return_t status) {
            stop_status = status;
            dispatch_semaphore_signal(sem);
        });

    long r = dispatch_semaphore_wait(sem,
        dispatch_time(DISPATCH_TIME_NOW, VMNET_STOP_TIMEOUT_SEC * NSEC_PER_SEC));

    dispatch_release(sem);
    dispatch_release(queue);

    if (r != 0) {
        fprintf(stderr, "[vmnet] stop_interface timed out after %llds\n",
                VMNET_STOP_TIMEOUT_SEC);
        return VMNET_HELPER_TIMEOUT;
    }
    return stop_status;
}

void vmnet_helper_set_event_callback(
    interface_ref iface,
    dispatch_queue_t queue,
    int write_fd
) {
    vmnet_interface_set_event_callback(iface,
        VMNET_INTERFACE_PACKETS_AVAILABLE, queue,
        ^(interface_event_t event_mask, xpc_object_t event) {
            (void)event_mask;
            (void)event;
            char c = 1;
            ssize_t ret;
            do {
                ret = write(write_fd, &c, 1);
            } while (ret < 0 && errno == EINTR);
            // EAGAIN/EPIPE: pipe full or closed — nothing we can do from
            // callback context. The event loop will pick up packets on the
            // next timer tick or queue notification regardless.
        });
}

ssize_t vmnet_helper_read(interface_ref iface, void *buf, size_t buf_len) {
    struct iovec iov;
    iov.iov_base = buf;
    iov.iov_len = buf_len;

    struct vmpktdesc pkt;
    pkt.vm_pkt_size = buf_len;
    pkt.vm_pkt_iov = &iov;
    pkt.vm_pkt_iovcnt = 1;
    pkt.vm_flags = 0;

    int pktcnt = 1;
    vmnet_return_t ret = vmnet_read(iface, &pkt, &pktcnt);
    if (ret != VMNET_SUCCESS) return -1;
    if (pktcnt == 0 || pkt.vm_pkt_size == 0) return 0;
    // Defensive: vmnet must not write beyond the iov buffer.
    if (pkt.vm_pkt_size > buf_len) return -1;
    return (ssize_t)pkt.vm_pkt_size;
}

ssize_t vmnet_helper_write(interface_ref iface, const void *buf, size_t len) {
    struct iovec iov;
    iov.iov_base = (void *)buf;
    iov.iov_len = len;

    struct vmpktdesc pkt;
    pkt.vm_pkt_size = len;
    pkt.vm_pkt_iov = &iov;
    pkt.vm_pkt_iovcnt = 1;
    pkt.vm_flags = 0;

    int pktcnt = 1;
    vmnet_return_t ret = vmnet_write(iface, &pkt, &pktcnt);
    if (ret != VMNET_SUCCESS) return -1;
    if (pktcnt == 0) return -1; // vmnet accepted call but dropped the packet.
    return (ssize_t)len;
}
