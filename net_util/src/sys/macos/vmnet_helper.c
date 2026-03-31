// vmnet_helper.c — C helper for vmnet.framework Objective-C block callbacks.
//
// Rust can't reliably create Objective-C blocks. This C file provides
// thin wrappers around vmnet functions that use blocks, converting them
// to standard C callbacks that Rust can call via FFI.

#include <vmnet/vmnet.h>
#include <dispatch/dispatch.h>
#include <string.h>
#include <stdio.h>

// Global state for synchronous vmnet_start_interface.
static vmnet_return_t g_start_status = 0;
static xpc_object_t g_start_params = NULL;
static dispatch_semaphore_t g_start_sem = NULL;

// Start a vmnet interface synchronously.
// Returns the interface ref, or NULL on failure.
// On success, *out_status is set to the vmnet status,
// *out_mac is set to the MAC address string (e.g. "aa:bb:cc:dd:ee:ff"),
// *out_mtu is set to the interface MTU.
interface_ref vmnet_helper_start_interface(
    uint64_t mode,
    vmnet_return_t *out_status,
    char *out_mac,      // must be >= 18 bytes
    size_t mac_buf_len,
    uint64_t *out_mtu,
    uint64_t *out_max_packet_size
) {
    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, mode);

    dispatch_queue_t queue = dispatch_queue_create(
        "com.aetheria.vmnet", DISPATCH_QUEUE_SERIAL);

    g_start_sem = dispatch_semaphore_create(0);
    g_start_status = 0;
    g_start_params = NULL;

    interface_ref iface = vmnet_start_interface(desc, queue,
        ^(vmnet_return_t status, xpc_object_t interface_param) {
            g_start_status = status;
            if (interface_param) {
                g_start_params = interface_param;
                // Retain so it survives after the block returns
                xpc_retain(interface_param);
            }
            dispatch_semaphore_signal(g_start_sem);
        });

    // Wait for the completion handler (timeout 5 seconds)
    dispatch_time_t timeout = dispatch_time(DISPATCH_TIME_NOW, 5LL * NSEC_PER_SEC);
    long result = dispatch_semaphore_wait(g_start_sem, timeout);

    if (result != 0) {
        // Timeout
        if (out_status) *out_status = 0; // failure
        if (iface) {
            vmnet_stop_interface(iface, queue, ^(vmnet_return_t s) {});
        }
        return NULL;
    }

    if (out_status) *out_status = g_start_status;

    if (g_start_status != VMNET_SUCCESS) {
        if (iface) {
            vmnet_stop_interface(iface, queue, ^(vmnet_return_t s) {});
        }
        return NULL;
    }

    // Extract interface parameters
    if (g_start_params) {
        if (out_mac) {
            const char *mac = xpc_dictionary_get_string(
                g_start_params, vmnet_mac_address_key);
            if (mac) {
                strncpy(out_mac, mac, mac_buf_len - 1);
                out_mac[mac_buf_len - 1] = '\0';
            }
        }
        if (out_mtu) {
            *out_mtu = xpc_dictionary_get_uint64(
                g_start_params, vmnet_mtu_key);
        }
        if (out_max_packet_size) {
            *out_max_packet_size = xpc_dictionary_get_uint64(
                g_start_params, vmnet_max_packet_size_key);
        }
        xpc_release(g_start_params);
        g_start_params = NULL;
    }

    return iface;
}

// Stop a vmnet interface synchronously.
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

    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 5LL * NSEC_PER_SEC));
    return stop_status;
}

// Set up event callback that writes to a pipe fd when packets are available.
void vmnet_helper_set_event_callback(
    interface_ref iface,
    dispatch_queue_t queue,
    int write_fd
) {
    vmnet_interface_set_event_callback(iface,
        VMNET_INTERFACE_PACKETS_AVAILABLE, queue,
        ^(interface_event_t event_mask, xpc_object_t event) {
            // Write a byte to the pipe to signal packet availability.
            char c = 1;
            write(write_fd, &c, 1);
        });
}

// Read one packet from vmnet interface.
// Returns the number of bytes read, or -1 on error, or 0 if no packets.
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
    return (ssize_t)pkt.vm_pkt_size;
}

// Write one packet to vmnet interface.
// Returns bytes written, or -1 on error.
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
    return (ssize_t)len;
}
