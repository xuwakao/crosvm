// VmnetTap — vmnet.framework backend for virtio-net on macOS.
//
// Uses vmnet shared mode (NAT) via a C helper (vmnet_helper.c) that handles
// Objective-C block callbacks. The C helper provides synchronous wrappers
// around vmnet_start_interface, vmnet_read, vmnet_write, etc.
//
// Requires root privileges for vmnet shared mode.

use std::io;
use std::io::{Read, Write};
use std::net;
use std::os::unix::io::{AsRawFd, RawFd};

use base::AsRawDescriptor;
use base::FileReadWriteVolatile;
use base::RawDescriptor;
use base::ReadNotifier;

use crate::MacAddress;
use crate::TapTCommon;

// FFI declarations for vmnet_helper.c functions.
extern "C" {
    fn vmnet_helper_start_interface(
        mode: u64,
        out_status: *mut u32,
        out_mac: *mut u8,
        mac_buf_len: usize,
        out_mtu: *mut u64,
        out_max_packet_size: *mut u64,
    ) -> *mut std::ffi::c_void; // interface_ref

    fn vmnet_helper_stop_interface(
        iface: *mut std::ffi::c_void,
    ) -> u32;

    fn vmnet_helper_set_event_callback(
        iface: *mut std::ffi::c_void,
        queue: *mut std::ffi::c_void,
        write_fd: i32,
    );

    fn vmnet_helper_read(
        iface: *mut std::ffi::c_void,
        buf: *mut u8,
        buf_len: usize,
    ) -> isize;

    fn vmnet_helper_write(
        iface: *mut std::ffi::c_void,
        buf: *const u8,
        len: usize,
    ) -> isize;

    fn dispatch_queue_create(
        label: *const i8,
        attr: *const std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
}

const VMNET_SHARED_MODE: u64 = 1;
const VMNET_SUCCESS: u32 = 1000;

pub struct VmnetTap {
    interface: *mut std::ffi::c_void,
    queue: *mut std::ffi::c_void,
    notify_read: RawFd,
    notify_write: RawFd,
    mac: MacAddress,
    mtu: u16,
}

unsafe impl Send for VmnetTap {}

impl VmnetTap {
    pub fn new_shared() -> io::Result<Self> {
        unsafe {
            // Create notification pipe
            let mut pipe_fds = [0i32; 2];
            if libc::pipe(pipe_fds.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::fcntl(pipe_fds[0], libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(pipe_fds[1], libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(pipe_fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(pipe_fds[1], libc::F_SETFD, libc::FD_CLOEXEC);

            // Start vmnet interface via C helper
            let mut status: u32 = 0;
            let mut mac_buf = [0u8; 18]; // "aa:bb:cc:dd:ee:ff\0"
            let mut mtu: u64 = 0;
            let mut max_pkt: u64 = 0;

            let interface = vmnet_helper_start_interface(
                VMNET_SHARED_MODE,
                &mut status,
                mac_buf.as_mut_ptr(),
                mac_buf.len(),
                &mut mtu,
                &mut max_pkt,
            );

            if interface.is_null() {
                libc::close(pipe_fds[0]);
                libc::close(pipe_fds[1]);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("vmnet_start_interface failed (status={}, requires root)", status),
                ));
            }

            // Parse MAC address from "aa:bb:cc:dd:ee:ff" string
            let mac_str = std::ffi::CStr::from_ptr(mac_buf.as_ptr() as *const i8)
                .to_str()
                .unwrap_or("02:00:00:00:00:01");
            let mac_bytes: Vec<u8> = mac_str
                .split(':')
                .filter_map(|s| u8::from_str_radix(s, 16).ok())
                .collect();
            let mac = if mac_bytes.len() == 6 {
                MacAddress {
                    addr: [mac_bytes[0], mac_bytes[1], mac_bytes[2],
                           mac_bytes[3], mac_bytes[4], mac_bytes[5]],
                }
            } else {
                MacAddress { addr: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01] }
            };

            let mtu = if mtu > 0 { mtu as u16 } else { 1500 };

            // Create dispatch queue for event callbacks
            let queue = dispatch_queue_create(
                b"com.aetheria.vmnet.events\0".as_ptr() as *const i8,
                std::ptr::null(),
            );

            // Set up event callback: writes to pipe_fds[1] when packets arrive
            vmnet_helper_set_event_callback(interface, queue, pipe_fds[1]);

            base::info!(
                "vmnet: started shared interface mac={} mtu={} max_pkt={}",
                mac_str, mtu, max_pkt
            );

            Ok(VmnetTap {
                interface,
                queue,
                notify_read: pipe_fds[0],
                notify_write: pipe_fds[1],
                mac,
                mtu,
            })
        }
    }
}

impl Read for VmnetTap {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { vmnet_helper_read(self.interface, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "vmnet_read failed"));
        }
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        // Drain notification pipe
        let mut drain = [0u8; 64];
        unsafe { libc::read(self.notify_read, drain.as_mut_ptr() as *mut _, drain.len()) };
        Ok(n as usize)
    }
}

impl Write for VmnetTap {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { vmnet_helper_write(self.interface, buf.as_ptr(), buf.len()) };
        if n < 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "vmnet_write failed"));
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsRawDescriptor for VmnetTap {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.notify_read
    }
}

impl AsRawFd for VmnetTap {
    fn as_raw_fd(&self) -> RawFd {
        self.notify_read
    }
}

impl ReadNotifier for VmnetTap {
    fn get_read_notifier(&self) -> &dyn AsRawDescriptor {
        self
    }
}

// Generate FileReadWriteVolatile from AsRawDescriptor (for virtqueue I/O).
base::volatile_impl!(VmnetTap);

impl TapTCommon for VmnetTap {
    fn new_with_name(_name: &[u8], _vnet_hdr: bool, _multi_vq: bool) -> crate::Result<Self> {
        Self::new_shared().map_err(|e| crate::Error::CreateTap(base::Error::new(e.raw_os_error().unwrap_or(libc::EIO))))
    }

    fn new(_vnet_hdr: bool, _multi_vq: bool) -> crate::Result<Self> {
        Self::new_shared().map_err(|e| crate::Error::CreateTap(base::Error::new(e.raw_os_error().unwrap_or(libc::EIO))))
    }

    fn into_mq_taps(self, _vq_pairs: u16) -> crate::Result<Vec<Self>> {
        Ok(vec![self])
    }

    fn ip_addr(&self) -> crate::Result<net::Ipv4Addr> {
        Ok(net::Ipv4Addr::UNSPECIFIED)
    }

    fn set_ip_addr(&self, _ip_addr: net::Ipv4Addr) -> crate::Result<()> {
        Ok(())
    }

    fn netmask(&self) -> crate::Result<net::Ipv4Addr> {
        Ok(net::Ipv4Addr::new(255, 255, 255, 0))
    }

    fn set_netmask(&self, _netmask: net::Ipv4Addr) -> crate::Result<()> {
        Ok(())
    }

    fn mtu(&self) -> crate::Result<u16> {
        Ok(self.mtu)
    }

    fn set_mtu(&self, _mtu: u16) -> crate::Result<()> {
        Ok(())
    }

    fn mac_address(&self) -> crate::Result<MacAddress> {
        Ok(self.mac)
    }

    fn set_mac_address(&self, _mac_addr: MacAddress) -> crate::Result<()> {
        Ok(())
    }

    fn set_offload(&self, _flags: std::os::raw::c_uint) -> crate::Result<()> {
        Ok(())
    }

    fn enable(&self) -> crate::Result<()> {
        Ok(())
    }

    fn try_clone(&self) -> crate::Result<Self> {
        Err(crate::Error::CloneTap(base::Error::new(libc::ENOTSUP)))
    }

    unsafe fn from_raw_descriptor(_descriptor: RawDescriptor) -> crate::Result<Self> {
        Err(crate::Error::CreateTap(base::Error::new(libc::ENOTSUP)))
    }
}

impl super::TapT for VmnetTap {}

impl Drop for VmnetTap {
    fn drop(&mut self) {
        unsafe {
            vmnet_helper_stop_interface(self.interface);
            libc::close(self.notify_read);
            libc::close(self.notify_write);
        }
    }
}
