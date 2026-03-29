// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
// macOS vm_control platform module. Adapted from Linux version.
// Differences: no madvise_pageout/madvise_remove (Linux-only Vm methods),
// should_prepare_memory_region always returns true on 64-bit macOS.

#[cfg(feature = "gpu")]
pub(crate) mod gpu;

use std::path::Path;
use std::time::Duration;

use base::error;
use base::AsRawDescriptor;
use base::Descriptor;
use base::Error as SysError;
use base::MemoryMappingArena;
use base::MmapError;
use base::Protection;
use base::SafeDescriptor;
use base::Tube;
use base::UnixSeqpacket;
use hypervisor::MemCacheType;
use hypervisor::MemSlot;
use hypervisor::Vm;
use libc::EINVAL;
use libc::ERANGE;
use resources::Alloc;
use resources::SystemAllocator;
use serde::Deserialize;
use serde::Serialize;
use vm_memory::GuestAddress;

use crate::client::HandleRequestResult;
use crate::VmMappedMemoryRegion;
use crate::VmRequest;
use crate::VmResponse;

pub fn handle_request<T: AsRef<Path> + std::fmt::Debug>(
    request: &VmRequest,
    socket_path: T,
) -> HandleRequestResult {
    handle_request_with_timeout(request, socket_path, None)
}

pub fn handle_request_with_timeout<T: AsRef<Path> + std::fmt::Debug>(
    request: &VmRequest,
    socket_path: T,
    timeout: Option<Duration>,
) -> HandleRequestResult {
    match UnixSeqpacket::connect(&socket_path) {
        Ok(s) => {
            let socket = Tube::try_from(s).map_err(|_| ())?;
            if timeout.is_some() {
                if let Err(e) = socket.set_recv_timeout(timeout) {
                    error!("failed to set recv timeout on socket at '{:?}': {}", socket_path, e);
                    return Err(());
                }
            }
            if let Err(e) = socket.send(request) {
                error!("failed to send request to socket at '{:?}': {}", socket_path, e);
                return Err(());
            }
            match socket.recv() {
                Ok(response) => Ok(response),
                Err(e) => {
                    error!("failed to recv response from socket at '{:?}': {}", socket_path, e);
                    Err(())
                }
            }
        }
        Err(e) => {
            error!("failed to connect to socket at '{:?}': {}", socket_path, e);
            Err(())
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmMemoryMappingRequest {
    MsyncArena {
        slot: MemSlot,
        offset: usize,
        size: usize,
    },
    // Note: MadvisePageout and MadviseRemove are not available on macOS.
    // They use Linux-only madvise flags (MADV_PAGEOUT, MADV_REMOVE).
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmMemoryMappingResponse {
    Ok,
    Err(SysError),
}

impl VmMemoryMappingRequest {
    pub fn execute(&self, vm: &mut impl Vm) -> VmMemoryMappingResponse {
        match *self {
            VmMemoryMappingRequest::MsyncArena { slot, offset, size } => {
                match vm.msync_memory_region(slot, offset, size) {
                    Ok(()) => VmMemoryMappingResponse::Ok,
                    Err(e) => VmMemoryMappingResponse::Err(e),
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum FsMappingRequest {
    AllocateSharedMemoryRegion(Alloc),
    CreateMemoryMapping {
        slot: u32,
        fd: SafeDescriptor,
        size: usize,
        file_offset: u64,
        prot: Protection,
        mem_offset: usize,
    },
    RemoveMemoryMapping {
        slot: u32,
        offset: usize,
        size: usize,
    },
}

pub fn prepare_shared_memory_region(
    vm: &mut dyn Vm,
    allocator: &mut SystemAllocator,
    alloc: Alloc,
    cache: MemCacheType,
) -> Result<VmMappedMemoryRegion, SysError> {
    if !matches!(alloc, Alloc::PciBar { .. }) {
        return Err(SysError::new(EINVAL));
    }
    match allocator.mmio_allocator_any().get(&alloc) {
        Some((range, _)) => {
            let size: usize = match range.len().and_then(|x| x.try_into().ok()) {
                Some(v) => v,
                None => return Err(SysError::new(ERANGE)),
            };
            let arena = match MemoryMappingArena::new(size) {
                Ok(a) => a,
                Err(MmapError::SystemCallFailed(e)) => return Err(e),
                _ => return Err(SysError::new(EINVAL)),
            };

            match vm.add_memory_region(
                GuestAddress(range.start),
                Box::new(arena),
                false,
                false,
                cache,
            ) {
                Ok(slot) => Ok(VmMappedMemoryRegion {
                    guest_address: GuestAddress(range.start),
                    slot,
                }),
                Err(e) => Err(e),
            }
        }
        None => Err(SysError::new(EINVAL)),
    }
}

pub fn should_prepare_memory_region() -> bool {
    // macOS is always 64-bit, no TDP MMU concerns.
    true
}

impl FsMappingRequest {
    pub fn execute(&self, vm: &mut dyn Vm, allocator: &mut SystemAllocator) -> VmResponse {
        use self::FsMappingRequest::*;
        match *self {
            AllocateSharedMemoryRegion(alloc) => {
                match prepare_shared_memory_region(
                    vm, allocator, alloc, MemCacheType::CacheCoherent,
                ) {
                    Ok(VmMappedMemoryRegion { slot, .. }) => VmResponse::RegisterMemory { slot },
                    Err(e) => VmResponse::Err(e),
                }
            }
            CreateMemoryMapping {
                slot, ref fd, size, file_offset, prot, mem_offset,
            } => {
                let raw_fd: Descriptor = Descriptor(fd.as_raw_descriptor());
                match vm.add_fd_mapping(slot, mem_offset, size, &raw_fd, file_offset, prot) {
                    Ok(()) => VmResponse::Ok,
                    Err(e) => VmResponse::Err(e),
                }
            }
            RemoveMemoryMapping { slot, offset, size } => {
                match vm.remove_mapping(slot, offset, size) {
                    Ok(()) => VmResponse::Ok,
                    Err(e) => VmResponse::Err(e),
                }
            }
        }
    }
}
