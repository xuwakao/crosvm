// macOS platform module for crosvm.
// Provides ExitState, run_config with HVF backend.

pub mod cmdline;
pub mod config;
#[cfg(feature = "gpu")]
pub mod gpu;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use arch::serial::set_default_serial_parameters;
use arch::LinuxArch;
use arch::VmComponents;
use arch::VmImage;
use base::debug;
use base::error;
use base::info;
use base::macos::terminal::Terminal;
use base::open_file_or_duplicate;
use base::Event;
use base::SendTube;
use base::Tube;
use devices::serial_device::SerialHardware;
use devices::BusDeviceObj;
use devices::VirtioPciDevice;
use hypervisor::IoOperation;
use hypervisor::IoParams;
use hypervisor::ProtectionType;
use hypervisor::Vcpu;
use hypervisor::VcpuAArch64;
use hypervisor::VcpuExit;
use hypervisor::Vm;
use hypervisor::VmAArch64;
use jail::FakeMinijailStub as Minijail;
use resources::SystemAllocator;
use vm_control::BatteryType;
use vm_memory::GuestMemory;
use vm_memory::MemoryPolicy;

use crate::crosvm::config::Config;
use crate::crosvm::config::Executable;

#[cfg(target_arch = "aarch64")]
type Arch = aarch64::AArch64;

/// Possible exit states for a VM.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExitState {
    Stop,
    Reset,
    Crash,
    GuestPanic,
    WatchdogReset,
}

/// Build VmComponents from Config for macOS.
fn setup_vm_components(cfg: &Config) -> Result<VmComponents> {
    let initrd_image = if let Some(initrd_path) = &cfg.initrd_path {
        Some(
            open_file_or_duplicate(initrd_path, OpenOptions::new().read(true))
                .with_context(|| format!("failed to open initrd {}", initrd_path.display()))?,
        )
    } else {
        None
    };

    let vm_image = match cfg.executable_path {
        Some(Executable::Kernel(ref kernel_path)) => VmImage::Kernel(
            open_file_or_duplicate(kernel_path, OpenOptions::new().read(true)).with_context(
                || format!("failed to open kernel image {}", kernel_path.display()),
            )?,
        ),
        Some(Executable::Bios(ref bios_path)) => VmImage::Bios(
            open_file_or_duplicate(bios_path, OpenOptions::new().read(true))
                .with_context(|| format!("failed to open bios {}", bios_path.display()))?,
        ),
        _ => bail!("Executable is not specified"),
    };

    let (cpu_clusters, cpu_capacity) = if cfg.host_cpu_topology {
        (
            Arch::get_host_cpu_clusters()?,
            Arch::get_host_cpu_capacity()?,
        )
    } else {
        (cfg.cpu_clusters.clone(), cfg.cpu_capacity.clone())
    };

    Ok(VmComponents {
        memory_size: cfg
            .memory
            .unwrap_or(256)
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow!("requested memory size too large"))?,
        swiotlb: None,
        fw_cfg_enable: false,
        bootorder_fw_cfg_blob: Vec::new(),
        vcpu_count: cfg.vcpu_count.unwrap_or(1),
        vcpu_affinity: cfg.vcpu_affinity.clone(),
        fw_cfg_parameters: Vec::new(),
        cpu_clusters,
        cpu_capacity,
        dev_pm: None,
        no_smt: cfg.no_smt,
        hugepages: false,
        hv_cfg: hypervisor::Config {
            #[cfg(target_arch = "aarch64")]
            mte: false,
            protection_type: ProtectionType::Unprotected,
            #[cfg(all(target_os = "android", target_arch = "aarch64"))]
            ffa: false,
            force_disable_readonly_mem: false,
        },
        vm_image,
        android_fstab: None,
        pstore: None,
        pflash_block_size: 0,
        pflash_image: None,
        initrd_image,
        extra_kernel_params: cfg.params.clone(),
        acpi_sdts: Vec::new(),
        rt_cpus: cfg.rt_cpus.clone(),
        delay_rt: cfg.delay_rt,
        no_i8042: true,
        no_rtc: false,
        host_cpu_topology: cfg.host_cpu_topology,
        itmt: false,
        pvm_fw: None,
        pci_config: cfg.pci_config,
        dynamic_power_coefficient: BTreeMap::new(),
        boot_cpu: 0,
        #[cfg(any(target_os = "android", target_os = "linux"))]
        vfio_platform_pm: false,
        smccc_trng: false,
        #[cfg(target_arch = "aarch64")]
        sve_config: Default::default(),
        #[cfg(target_arch = "x86_64")]
        ac_adapter: false,
        #[cfg(target_arch = "x86_64")]
        break_linux_pci_config_io: false,
        #[cfg(target_arch = "x86_64")]
        smbios: Default::default(),
        #[cfg(target_arch = "x86_64")]
        force_s2idle: false,
        #[cfg(all(target_arch = "aarch64", any(target_os = "android", target_os = "linux")))]
        cpu_frequencies: BTreeMap::new(),
        #[cfg(all(target_arch = "aarch64", any(target_os = "android", target_os = "linux")))]
        normalized_cpu_ipc_ratios: BTreeMap::new(),
        #[cfg(all(target_arch = "aarch64", any(target_os = "android", target_os = "linux")))]
        vcpu_domains: BTreeMap::new(),
        #[cfg(all(target_arch = "aarch64", any(target_os = "android", target_os = "linux")))]
        vcpu_domain_paths: BTreeMap::new(),
        #[cfg(all(target_arch = "aarch64", any(target_os = "android", target_os = "linux")))]
        virt_cpufreq_v2: false,
    })
}

/// Create guest memory for the VM.
fn create_guest_memory(
    components: &VmComponents,
    arch_memory_layout: &<Arch as LinuxArch>::ArchMemoryLayout,
    hypervisor: &impl hypervisor::Hypervisor,
) -> Result<GuestMemory> {
    let guest_mem_layout = Arch::guest_memory_layout(components, arch_memory_layout, hypervisor)
        .context("failed to create guest memory layout")?;

    let guest_mem = GuestMemory::new_with_options(&guest_mem_layout)
        .context("failed to create guest memory")?;
    guest_mem.set_memory_policy(MemoryPolicy::empty());

    Ok(guest_mem)
}

/// Run the VM with the given configuration using HVF.
pub fn run_config(cfg: Config) -> Result<ExitState> {
    info!("run_config: starting HVF VM on macOS");

    let mut components = setup_vm_components(&cfg)?;

    #[cfg(target_arch = "aarch64")]
    {
        use devices::HvfGicChip;
        use hypervisor::hvf::Hvf;
        use hypervisor::hvf::vcpu::HvfVcpu;
        use hypervisor::hvf::vm::HvfVm;

        let hvf = Hvf::new().context("failed to create HVF hypervisor")?;

        let arch_memory_layout =
            Arch::arch_memory_layout(&components).context("failed to create arch memory layout")?;
        let guest_mem = create_guest_memory(&components, &arch_memory_layout, &hvf)?;

        // GIC distributor base address — must match aarch64::AARCH64_GIC_DIST_BASE.
        const GIC_DIST_BASE: u64 = 0x40000000 - 0x10000; // 0x3FFF0000
        let guest_mem_for_pci = guest_mem.clone();
        let vm = HvfVm::new(hvf, guest_mem, GIC_DIST_BASE)
            .context("failed to create HVF VM")?;

        // Set up default serial parameters (COM1 = stdout console with earlycon).
        let mut serial_parameters = cfg.serial_parameters.clone();
        set_default_serial_parameters(&mut serial_parameters, false);
        // Enable earlycon on COM1 for immediate serial output during boot.
        if let Some(params) = serial_parameters.get_mut(&(SerialHardware::Serial, 1)) {
            params.earlycon = true;
        }

        // Create system allocator.
        let pstore_size = components.pstore.as_ref().map(|p| p.size as u64);
        let mut sys_allocator = SystemAllocator::new(
            Arch::get_system_allocator_config(&vm, &arch_memory_layout),
            pstore_size,
            &cfg.mmio_address_ranges,
        )
        .context("failed to create system allocator")?;

        // Create VM event tube.
        let (vm_evt_wrtube, vm_evt_rdtube) =
            Tube::directional_pair().context("failed to create vm event tube")?;

        // Create IRQ chip.
        let vcpu_count = components.vcpu_count;
        let mut irq_chip = HvfGicChip::new(vcpu_count)
            .context("failed to create HVF GIC chip")?;

        // Create virtio devices from config and wrap in VirtioPciDevice for PCI bus.
        let mut devices: Vec<(Box<dyn BusDeviceObj>, Option<Minijail>)> = Vec::new();
        let mut ioevent_host_tubes: Vec<Tube> = Vec::new();
        let mut msi_host_tubes: Vec<Tube> = Vec::new();

        // Block devices: Config.disks → DiskOption.open() → BlockAsync → VirtioPciDevice.
        for disk in &cfg.disks {
            use devices::virtio;
            use vm_control::api::VmMemoryClient;

            info!("Creating virtio-blk device: {}", disk.path.display());
            let disk_image = disk.open().context("failed to open disk image")?;
            let base_features = virtio::base_features(ProtectionType::Unprotected);
            let block_dev = Box::new(
                virtio::BlockAsync::new(
                    base_features,
                    disk_image,
                    disk,
                    None, // control_tube — no runtime disk control until ISS-011
                    None, // queue_size — use default
                    None, // num_queues — use default
                )
                .context("failed to create virtio-blk device")?,
            );

            // Create tube pairs for VirtioPciDevice communication.
            // MSI-X tube: interrupt configuration between device and IRQ chip.
            let (msi_host_tube, msi_device_tube) =
                Tube::pair().context("failed to create MSI tube")?;
            msi_host_tubes.push(msi_host_tube);
            // Ioevent tube: VirtioPciDevice uses this to register ioevents.
            // On macOS, kqueue fds cannot be sent via SCM_RIGHTS (sendmsg
            // returns EINVAL), so ioevent registration via tube will fail.
            // We compensate by calling vm.handle_io_events() on every MMIO
            // write in the vCPU loop (the HAXM/WHPX approach).
            // The device-side tube still needs to exist for VirtioPciDevice
            // construction, but the host side just needs to respond "Ok".
            let (ioevent_host_tube, ioevent_device_tube) =
                Tube::pair().context("failed to create ioevent tube")?;
            ioevent_host_tubes.push(ioevent_host_tube);
            // VM control tube: device-to-VMM control messages.
            let (_vm_host_tube, vm_device_tube) =
                Tube::pair().context("failed to create vm control tube")?;

            let pci_dev = VirtioPciDevice::new(
                guest_mem_for_pci.clone(),
                block_dev,
                msi_device_tube,
                false, // disable_virtio_intx
                None,  // shared_memory_vm_memory_client — not needed for block
                VmMemoryClient::new_noop_ioevent(ioevent_device_tube),
                vm_device_tube,
            )
            .context("failed to create virtio-pci block device")?;

            devices.push((Box::new(pci_dev) as Box<dyn BusDeviceObj>, None));
            info!("virtio-blk device created for {}", disk.path.display());
        }

        // Network device: create VmnetTap-backed virtio-net if not disabled.
        // Requires root privileges for vmnet shared mode.
        if std::env::var("AETHERIA_NO_NET").is_err() {
            match create_net_device(guest_mem_for_pci.clone()) {
                Ok(pci_dev) => {
                    devices.push((Box::new(pci_dev) as Box<dyn BusDeviceObj>, None));
                    info!("virtio-net device created (vmnet shared mode)");
                }
                Err(e) => {
                    info!("virtio-net not available: {:#} (set AETHERIA_NO_NET=1 to suppress)", e);
                }
            }
        }

        // Vsock device: enables host↔guest communication via AF_VSOCK sockets.
        // CID 3 is conventional for the first guest VM (2 = host, 0/1 = reserved).
        {
            use devices::virtio;
            use devices::virtio::vsock::{Vsock as VsockDevice, VsockConfig};
            use vm_control::api::VmMemoryClient;
            let vsock_cid: u64 = 3;
            match VsockDevice::new(vsock_cid, virtio::base_features(ProtectionType::Unprotected)) {
                Ok(vsock_dev) => {
                    let (_msi_tube, msi_device_tube) =
                        Tube::pair().context("vsock MSI tube")?;
                    let (ioevent_tube, ioevent_device_tube) =
                        Tube::pair().context("vsock ioevent tube")?;
                    let (_vm_tube, vm_device_tube) =
                        Tube::pair().context("vsock vm tube")?;
                    std::mem::forget(_msi_tube);
                    std::mem::forget(ioevent_tube);
                    std::mem::forget(_vm_tube);

                    match VirtioPciDevice::new(
                        guest_mem_for_pci.clone(),
                        Box::new(vsock_dev),
                        msi_device_tube,
                        false,
                        None,
                        VmMemoryClient::new_noop_ioevent(ioevent_device_tube),
                        vm_device_tube,
                    ) {
                        Ok(pci_dev) => {
                            devices.push((Box::new(pci_dev) as Box<dyn BusDeviceObj>, None));
                            info!("virtio-vsock device created (cid={})", vsock_cid);
                        }
                        Err(e) => {
                            info!("virtio-vsock PCI wrap failed: {:#}", e);
                        }
                    }
                }
                Err(e) => {
                    info!("virtio-vsock creation failed: {:#}", e);
                }
            }
        }

        // Filesystem sharing: virtiofs (primary) or 9P (fallback).
        // Guest mounts: mount -t virtiofs host_share /mnt
        let share_path = std::env::var("AETHERIA_SHARE")
            .unwrap_or_else(|_| "/private/tmp/aetheria-share".to_string());
        let share_path = std::fs::canonicalize(&share_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(share_path);
        if std::path::Path::new(&share_path).is_dir() {
            use devices::virtio;
            use devices::virtio::fs::{Fs, Config as FsConfig, CachePolicy};
            use vm_control::api::VmMemoryClient;

            let tag = "host_share";
            let mut fs_cfg = FsConfig::default();
            // cache=always for best performance on macOS.
            fs_cfg.cache_policy = CachePolicy::Always;

            let (fs_tube_host, fs_tube_device) =
                Tube::pair().context("virtiofs tube")?;
            std::mem::forget(fs_tube_host);

            match Fs::new(
                virtio::base_features(ProtectionType::Unprotected),
                tag,
                1, // num_workers
                fs_cfg,
                fs_tube_device,
            ) {
                Ok(fs_dev) => {
                    let (_msi_tube, msi_device_tube) =
                        Tube::pair().context("fs MSI tube")?;
                    let (ioevent_tube, ioevent_device_tube) =
                        Tube::pair().context("fs ioevent tube")?;
                    let (_vm_tube, vm_device_tube) =
                        Tube::pair().context("fs vm tube")?;
                    std::mem::forget(ioevent_tube);

                    match VirtioPciDevice::new(
                        guest_mem_for_pci.clone(),
                        Box::new(fs_dev),
                        msi_device_tube,
                        false,
                        None,
                        VmMemoryClient::new_noop_ioevent(ioevent_device_tube),
                        vm_device_tube,
                    ) {
                        Ok(pci_dev) => {
                            devices.push((Box::new(pci_dev) as Box<dyn BusDeviceObj>, None));
                            info!("virtiofs device created, sharing '{}' as '{}'", share_path, tag);
                        }
                        Err(e) => {
                            info!("virtiofs PCI wrap failed: {:#}", e);
                        }
                    }
                }
                Err(e) => {
                    info!("virtiofs creation failed: {:?}", e);
                }
            }
        }

        // GPU device: virtio-gpu with Rutabaga2D software backend.
        // Provides /dev/dri/card0 in guest for Mesa software rendering.
        #[cfg(feature = "gpu")]
        {
            use devices::virtio;
            use devices::virtio::gpu::{DisplayBackend, Gpu, GpuParameters};
            use vm_control::api::VmMemoryClient;

            let gpu_params = GpuParameters::default();
            // Host-side tubes kept alive via _prefix — device-side tubes passed to GPU.
            // Dropping (not forgetting) is safe: the device tube remains valid as long
            // as the Tube pair's internal fd is not closed, but Rust's Drop on Tube
            // DOES close. Use forget to keep device-side connected.
            let (_gpu_ctrl_host, gpu_ctrl_device) =
                Tube::pair().context("gpu control tube")?;
            let (_msi_tube, msi_device_tube) =
                Tube::pair().context("gpu MSI tube")?;
            let (ioevent_tube, ioevent_device_tube) =
                Tube::pair().context("gpu ioevent tube")?;
            let (_vm_tube, vm_device_tube) =
                Tube::pair().context("gpu vm tube")?;
            // ioevent host tube must outlive device (same as blk/net).
            std::mem::forget(ioevent_tube);

            let gpu_dev = Gpu::new(
                vm_evt_wrtube.try_clone().context("clone vm_evt for gpu")?,
                gpu_ctrl_device,
                Vec::new(),       // resource_bridges
                vec![DisplayBackend::Stub],
                &gpu_params,
                None,             // rutabaga_server_descriptor
                Vec::new(),       // event_devices
                virtio::base_features(ProtectionType::Unprotected),
                &BTreeMap::new(), // paths
            );

            // GPU has shared memory regions — need a real VmMemoryClient.
            let (_shmem_host_tube, shmem_device_tube) =
                Tube::pair().context("gpu shmem tube")?;

            match VirtioPciDevice::new(
                guest_mem_for_pci.clone(),
                Box::new(gpu_dev),
                msi_device_tube,
                false,
                Some(VmMemoryClient::new(shmem_device_tube)),
                VmMemoryClient::new_noop_ioevent(ioevent_device_tube),
                vm_device_tube,
            ) {
                Ok(pci_dev) => {
                    devices.push((Box::new(pci_dev) as Box<dyn BusDeviceObj>, None));
                    info!("virtio-gpu device created (Rutabaga2D stub display)");
                }
                Err(e) => {
                    info!("virtio-gpu PCI wrap failed: {:#}", e);
                }
            }
        }

        let mut vcpu_ids: Vec<usize> = (0..vcpu_count).collect();

        info!("Building VM with Arch::build_vm...");

        // Build the VM — this creates serial devices, FDT, loads kernel, etc.
        let mut linux = Arch::build_vm::<HvfVm, HvfVcpu>(
            components,
            &arch_memory_layout,
            &vm_evt_wrtube,
            &mut sys_allocator,
            &serial_parameters,
            None, // serial_jail (no jailing on macOS)
            (None, None), // battery (type, jail)
            vm,
            None, // ramoops_region
            devices,
            &mut irq_chip,
            &mut vcpu_ids,
            cfg.dump_device_tree_blob.clone(),
            None, // debugcon_jail
            #[cfg(feature = "swap")]
            &mut None,
            None, // guest_suspended_cvar
            Vec::new(), // device_tree_overlays
            None, // fdt_position
            false, // no_pmu
        )
        .context("Arch::build_vm failed")?;

        info!("VM built successfully.");

        // Register GIC MMIO emulation only if native HVF GIC is NOT available.
        // On macOS 15+, hv_gic_create() handles GICD/GICR MMIO natively.
        if !hypervisor::hvf::ffi::hvf_gic_is_available() {
            use devices::GicDistributor;
            use devices::GicRedistributor;

            info!("Registering software GIC MMIO emulation (macOS <15 fallback)...");
            const GIC_DIST_SIZE: u64 = 0x10000;
            let gicd = GicDistributor::new(64);
            linux.mmio_bus.insert(
                Arc::new(sync::Mutex::new(gicd)),
                GIC_DIST_BASE,
                GIC_DIST_SIZE,
            ).expect("failed to register GIC distributor");

            const GIC_REDIST_BASE: u64 = 0x3FFD0000;
            const GIC_REDIST_SIZE_PER_CPU: u64 = 0x20000;
            for cpu_id in 0..vcpu_count {
                let gicr = GicRedistributor::new(cpu_id as u32, vcpu_count as u32);
                let base = GIC_REDIST_BASE + (cpu_id as u64) * GIC_REDIST_SIZE_PER_CPU;
                linux.mmio_bus.insert(
                    Arc::new(sync::Mutex::new(gicr)),
                    base,
                    GIC_REDIST_SIZE_PER_CPU,
                ).expect("failed to register GIC redistributor");
            }
            info!("GIC MMIO emulation registered");
        }

        // Finalize IRQ chip after all devices registered.
        use devices::IrqChip;
        use devices::IrqChipAArch64;
        irq_chip.finalize_devices(&mut sys_allocator, &linux.io_bus, &linux.mmio_bus)?;
        irq_chip.finalize()?;

        // Keep ioevent host tubes alive (noop_ioevent mode — write_bar fallback).
        let _ioevent_tubes_keepalive = ioevent_host_tubes;

        // Spawn MSI handler thread — processes VmIrqRequest from VirtioPciDevice's
        // MsixConfig to allocate MSI-X interrupt vectors. Without this, the device
        // cannot deliver completion interrupts and the kernel hangs waiting for I/O.
        let mut irq_chip_for_msi = irq_chip.try_box_clone()
            .context("failed to clone irq chip for MSI handler")?;
        // Use first MSI tube (single disk). For multi-disk, would need per-device handlers.
        let msi_tube = msi_host_tubes.into_iter().next();
        let msi_handler_join = if let Some(tube) = msi_tube {
            Some(thread::Builder::new()
                .name("msi_handler".into())
                .spawn(move || {
                    msi_handler_thread(irq_chip_for_msi, sys_allocator, tube);
                })
                .context("failed to spawn MSI handler thread")?)
        } else {
            None
        };

        // Start IRQ handler thread — polls device IRQ eventfds and calls
        // service_irq_event to mark interrupts pending for vCPU injection.
        // This is the macOS equivalent of Linux's irq_handler_thread.
        let (irq_handler_control, irq_handler_control_for_thread) =
            Tube::pair().context("failed to create irq handler control tube")?;
        let irq_chip_for_irq_thread = irq_chip
            .try_box_clone()
            .context("failed to clone irq chip for IRQ handler")?;

        let irq_handler_join = thread::Builder::new()
            .name("irq_handler".into())
            .spawn(move || {
                if let Err(e) = irq_handler_thread(
                    irq_chip_for_irq_thread,
                    irq_handler_control_for_thread,
                ) {
                    error!("IRQ handler thread error: {:#}", e);
                }
            })
            .context("failed to spawn IRQ handler thread")?;

        // --- vCPU lifecycle management ---
        //
        // Per-vCPU state: startup signal (condvar), running flag, HVF handle.
        // Secondary vCPUs wait for PSCI CPU_ON before entering vcpu_loop.
        // SYSTEM_OFF cancels all running vCPUs via hv_vcpu_cancel.

        // Shared vCPU handles for cross-thread cancel (PSCI SYSTEM_OFF).
        let vcpu_hvf_handles: Arc<std::sync::Mutex<Vec<u64>>> =
            Arc::new(std::sync::Mutex::new(vec![0u64; vcpu_count]));

        // Per-vCPU startup signal: None = waiting, Some = CPU_ON received.
        // Once set to Some, the secondary vCPU starts running and cannot be
        // re-started (PSCI_ALREADY_ON).
        let vcpu_start_signals: Vec<Arc<(std::sync::Mutex<Option<(u64, u64)>>, std::sync::Condvar)>> =
            (0..vcpu_count)
                .map(|_| Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new())))
                .collect();

        // Track which vCPUs are running (for ALREADY_ON detection).
        let vcpu_running: Arc<std::sync::Mutex<Vec<bool>>> =
            Arc::new(std::sync::Mutex::new(vec![false; vcpu_count]));

        // --- PSCI device setup ---
        let psci_exit_request = Arc::new(AtomicU8::new(devices::PSCI_EXIT_NONE));

        // CPU_ON callback: signal the target secondary vCPU.
        let signals_for_cpu_on = vcpu_start_signals.clone();
        let running_for_cpu_on = vcpu_running.clone();
        let cpu_on_cb: devices::CpuOnCallback = Arc::new(move |target_mpidr, entry_point, context_id| {
            let target_id = (target_mpidr & 0xff) as usize;
            let running = running_for_cpu_on.lock().unwrap();
            if target_id >= running.len() {
                return devices::CpuOnResult::InvalidParameters;
            }
            if running[target_id] {
                return devices::CpuOnResult::AlreadyOn;
            }
            drop(running);
            if let Some(signal) = signals_for_cpu_on.get(target_id) {
                let (lock, cvar) = &**signal;
                let mut guard = lock.lock().unwrap();
                if guard.is_some() {
                    return devices::CpuOnResult::AlreadyOn;
                }
                *guard = Some((entry_point, context_id));
                cvar.notify_one();
                devices::CpuOnResult::Success
            } else {
                devices::CpuOnResult::InvalidParameters
            }
        });

        // SYSTEM_OFF callback: cancel all vCPUs so threads exit.
        let handles_for_off = vcpu_hvf_handles.clone();
        let system_off_cb: devices::SystemOffCallback = Arc::new(move || {
            let handles = handles_for_off.lock().unwrap();
            for &h in handles.iter() {
                if h != 0 {
                    unsafe { hypervisor::hvf::ffi::hv_vcpu_cancel(h) };
                }
            }
        });

        let psci_device = Arc::new(
            devices::PsciDevice::new(psci_exit_request.clone())
                .with_cpu_on_callback(cpu_on_cb)
                .with_system_off_callback(system_off_cb),
        );
        for fid_range in [
            devices::PsciDevice::HVC32_FID_RANGE,
            devices::PsciDevice::HVC64_FID_RANGE,
        ] {
            let base: u64 = fid_range.start.into();
            let count = fid_range.len();
            linux
                .hypercall_bus
                .insert_sync(psci_device.clone(), base, count.try_into().unwrap())
                .expect("failed to register PSCI device on hypercall bus");
        }

        let io_bus = linux.io_bus.clone();
        let mmio_bus = linux.mmio_bus.clone();
        let hypercall_bus = linux.hypercall_bus.clone();
        let vcpu_init_data = linux.vcpu_init.clone();

        let _ = std::io::stdin().set_raw_mode();

        // --- Spawn per-vCPU worker threads ---
        let mut vcpu_join_handles: Vec<thread::JoinHandle<ExitState>> = Vec::new();

        for vcpu_id in 0..vcpu_count {
            let vm_clone = linux.vm.try_clone()
                .context("failed to clone VM for vCPU thread")?;
            let io_bus = io_bus.clone();
            let mmio_bus = mmio_bus.clone();
            let hypercall_bus = hypercall_bus.clone();
            let mut irq_chip_clone = irq_chip.try_box_clone()
                .context("failed to clone irq chip for vCPU thread")?;
            let psci_exit = psci_exit_request.clone();
            let start_signal = vcpu_start_signals[vcpu_id].clone();
            let hvf_handles = vcpu_hvf_handles.clone();
            let running = vcpu_running.clone();
            // Only boot CPU (id=0) gets init_data for configure_vcpu.
            let init_data = if vcpu_id == 0 {
                Some(vcpu_init_data[vcpu_id].clone())
            } else {
                None
            };

            let handle = thread::Builder::new()
                .name(format!("crosvm_vcpu{}", vcpu_id))
                .spawn(move || {
                    // Create HVF vCPU on this thread (thread affinity).
                    let mut vcpu = match vm_clone.create_vcpu(vcpu_id) {
                        Ok(v) => match v.downcast::<hypervisor::hvf::vcpu::HvfVcpu>() {
                            Ok(v) => *v,
                            Err(_) => {
                                error!("vCPU {}: downcast to HvfVcpu failed", vcpu_id);
                                return ExitState::Crash;
                            }
                        },
                        Err(e) => {
                            error!("vCPU {}: creation failed: {}", vcpu_id, e);
                            return ExitState::Crash;
                        }
                    };

                    // Set MPIDR_EL1 for GIC redistributor binding.
                    let mpidr_val = hypervisor::hvf::ffi::MPIDR_RES1 | (vcpu_id as u64);
                    if let Err(e) = vcpu.set_one_reg(
                        hypervisor::VcpuRegAArch64::System(aarch64_sys_reg::MPIDR_EL1),
                        mpidr_val,
                    ) {
                        error!("vCPU {}: MPIDR set failed: {}", vcpu_id, e);
                    }

                    // Initialize vCPU (HVF handles features natively).
                    if let Err(e) = vcpu.init(&[]) {
                        error!("vCPU {}: init failed: {}", vcpu_id, e);
                        return ExitState::Crash;
                    }

                    // Only boot CPU gets configure_vcpu (sets kernel entry point, X0=FDT, etc).
                    // Secondary CPUs get their entry point from PSCI CPU_ON.
                    if let Some(data) = init_data {
                        if let Err(e) = Arch::configure_vcpu(
                            &vm_clone,
                            vm_clone.get_hypervisor(),
                            &mut *irq_chip_clone,
                            &mut vcpu,
                            data,
                            vcpu_id,
                            vcpu_count,
                            None,
                        ) {
                            error!("vCPU {}: configure failed: {}", vcpu_id, e);
                            return ExitState::Crash;
                        }
                    }

                    // Register vCPU handle for IRQ injection and cross-thread cancel.
                    if let Err(e) = irq_chip_clone.as_irq_chip_mut().add_vcpu(vcpu_id, &vcpu as &dyn Vcpu) {
                        error!("vCPU {}: add_vcpu failed: {}", vcpu_id, e);
                    }
                    hvf_handles.lock().unwrap()[vcpu_id] = vcpu.hvf_handle();

                    info!("vCPU {} ready on thread {:?}", vcpu_id, thread::current().id());

                    // Secondary vCPUs wait for PSCI CPU_ON.
                    if vcpu_id > 0 {
                        let (lock, cvar) = &*start_signal;
                        let mut guard = lock.lock().unwrap();
                        while guard.is_none() {
                            // Check if we should exit (SYSTEM_OFF while waiting).
                            if psci_exit.load(Ordering::Acquire) != devices::PSCI_EXIT_NONE {
                                return ExitState::Stop;
                            }
                            guard = cvar.wait(guard).unwrap();
                        }
                        let (entry_point, context_id) = guard.unwrap();
                        info!("vCPU {}: CPU_ON entry={:#x} ctx={:#x}", vcpu_id, entry_point, context_id);
                        // Set up secondary CPU initial state per ARM64 boot protocol:
                        // - PC = entry point from CPU_ON
                        // - X0 = context_id
                        // - PSTATE = EL1h with DAIF masked (interrupts disabled)
                        // Interrupts must be masked because VBAR_EL1=0 until the kernel
                        // sets it up — any exception before that would jump to unmapped
                        // address 0x400 causing an instruction abort loop.
                        let _ = vcpu.set_one_reg(hypervisor::VcpuRegAArch64::Pc, entry_point);
                        let _ = vcpu.set_one_reg(hypervisor::VcpuRegAArch64::X(0), context_id);
                        // PSTATE: EL1h (0x5) | DAIF mask (0x3C0) = 0x3C5
                        let _ = vcpu.set_one_reg(hypervisor::VcpuRegAArch64::Pstate, 0x3C5);
                    }

                    // Mark as running.
                    running.lock().unwrap()[vcpu_id] = true;

                    // Run the vCPU loop.
                    let result = vcpu_loop(
                        &mut vcpu, &vm_clone, &io_bus, &mmio_bus,
                        &hypercall_bus, irq_chip_clone.as_ref(), &psci_exit,
                    );

                    // Mark as stopped.
                    running.lock().unwrap()[vcpu_id] = false;

                    match result {
                        Ok(state) => {
                            info!("vCPU {} exited: {:?}", vcpu_id, state);
                            state
                        }
                        Err(e) => {
                            error!("vCPU {} error: {:#}", vcpu_id, e);
                            ExitState::Crash
                        }
                    }
                })
                .context(format!("failed to spawn vCPU {} thread", vcpu_id))?;

            vcpu_join_handles.push(handle);
        }

        // Wait for all vCPU threads to exit.
        // Priority: Crash > Reset > Stop.
        let mut exit_state = ExitState::Stop;
        for (i, handle) in vcpu_join_handles.into_iter().enumerate() {
            match handle.join() {
                Ok(state) => {
                    let dominated = match (&exit_state, &state) {
                        (ExitState::Crash, _) => false,
                        (_, ExitState::Crash) => true,
                        (ExitState::Reset, _) => false,
                        (_, ExitState::Reset) => true,
                        _ => false,
                    };
                    if dominated {
                        exit_state = state;
                    }
                }
                Err(_) => {
                    error!("vCPU {} thread panicked", i);
                    exit_state = ExitState::Crash;
                }
            }
        }

        // Restore stdin to canonical mode.
        let _ = std::io::stdin().set_canon_mode();

        // Shut down IRQ handler thread.
        if let Err(e) = irq_handler_control.send(&vm_control::IrqHandlerRequest::Exit) {
            error!("failed to send Exit to IRQ handler: {}", e);
        }
        if let Err(e) = irq_handler_join.join() {
            error!("IRQ handler thread panicked: {:?}", e);
        }
        // VM memory handler thread exits when all device-side tubes are dropped
        // (which happens when the vCPU loop exits and devices are cleaned up).
        drop(_ioevent_tubes_keepalive);
        if let Some(join) = msi_handler_join {
            let _ = join.join();
        }

        Ok(exit_state)
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        bail!("HVF is only supported on AArch64")
    }
}

/// MSI handler thread — processes VmIrqRequest from VirtioPciDevice's MsixConfig.
///
/// When the guest driver enables MSI-X vectors, MsixConfig sends AllocateOneMsi
/// requests through the MSI tube. This handler allocates IRQ numbers from the
/// system allocator and registers the irqfd with the IRQ chip, enabling the
/// device to deliver completion interrupts to the GIC.
/// Create a virtio-net device backed by vmnet.framework.
#[cfg(target_arch = "aarch64")]
fn create_net_device(
    guest_mem: GuestMemory,
) -> Result<VirtioPciDevice> {
    use devices::virtio;
    use net_util::sys::macos::VmnetTap;
    use net_util::TapTCommon;
    use vm_control::api::VmMemoryClient;

    let tap = VmnetTap::new_shared()
        .context("failed to create vmnet interface (requires root)")?;

    // Use the MAC address assigned by vmnet so the guest uses the correct L2 address
    // for DHCP and ARP. Without this, the guest picks a random MAC and vmnet's NAT
    // gateway won't route packets correctly.
    let vmnet_mac = tap.mac_address().ok();

    // Build features manually: vmnet does NOT support checksum/TSO/UFO offload.
    // If we offer VIRTIO_NET_F_CSUM, the guest sends UDP packets with partial
    // checksums that vmnet's NAT gateway silently drops (DNS fails).
    // Only offer MAC + MTU + CTRL_VQ on macOS.
    //
    // Feature bit values from virtio_net.h (stable since virtio 1.0):
    const VIRTIO_NET_F_MAC: u64 = 5;
    const VIRTIO_NET_F_MTU: u64 = 3;
    const VIRTIO_NET_F_CTRL_VQ: u64 = 17;

    let mut avail_features = virtio::base_features(ProtectionType::Unprotected)
        | 1 << VIRTIO_NET_F_MTU
        | 1 << VIRTIO_NET_F_CTRL_VQ;
    if vmnet_mac.is_some() {
        avail_features |= 1 << VIRTIO_NET_F_MAC;
    }

    let taps = tap.into_mq_taps(1)
        .map_err(|e| anyhow::anyhow!("into_mq_taps: {:?}", e))?;
    let mtu = taps[0].mtu().unwrap_or(1500);

    let net_dev = Box::new(
        virtio::Net::new_internal(
            taps,
            avail_features,
            mtu,
            vmnet_mac,
            None, // pci_address
        )
        .context("failed to create virtio-net device")?,
    );

    // Host-side tubes are leaked intentionally: they must outlive the device
    // for the paired device-side tubes to remain connected. Same pattern as blk.
    let (msi_host_tube, msi_device_tube) =
        Tube::pair().context("failed to create MSI tube for net")?;
    let (ioevent_host_tube, ioevent_device_tube) =
        Tube::pair().context("failed to create ioevent tube for net")?;
    let (vm_host_tube, vm_device_tube) =
        Tube::pair().context("failed to create vm control tube for net")?;
    std::mem::forget(msi_host_tube);
    std::mem::forget(ioevent_host_tube);
    std::mem::forget(vm_host_tube);

    VirtioPciDevice::new(
        guest_mem,
        net_dev,
        msi_device_tube,
        false,
        None,
        VmMemoryClient::new_noop_ioevent(ioevent_device_tube),
        vm_device_tube,
    )
    .context("failed to create virtio-pci net device")
}

#[cfg(target_arch = "aarch64")]
fn msi_handler_thread(
    mut irq_chip: Box<dyn devices::IrqChipAArch64>,
    mut sys_allocator: SystemAllocator,
    tube: Tube,
) {
    use devices::IrqChip;
    use devices::IrqEdgeEvent;
    use devices::IrqEventSource;
    use vm_control::IrqSetup;
    use vm_control::VmIrqRequest;

    info!("MSI handler thread started");

    loop {
        match tube.recv::<VmIrqRequest>() {
            Ok(request) => {
                let response = request.execute(
                    |setup| match setup {
                        IrqSetup::Event(irq_num, irqfd, device_id, queue_id, device_name) => {
                            let edge_evt = IrqEdgeEvent::from_event(
                                irqfd.try_clone().map_err(|_| base::Error::new(libc::EIO))?,
                            );
                            let source = IrqEventSource {
                                device_id,
                                queue_id,
                                device_name,
                            };
                            irq_chip.as_irq_chip_mut().register_edge_irq_event(
                                irq_num, &edge_evt, source,
                            )?;
                            info!("MSI: allocated IRQ {} for device", irq_num);
                            Ok(())
                        }
                        IrqSetup::Route(_) => Ok(()),
                        IrqSetup::UnRegister(irq_num, irqfd) => {
                            let edge_evt = IrqEdgeEvent::from_event(
                                irqfd.try_clone().map_err(|_| base::Error::new(libc::EIO))?,
                            );
                            irq_chip
                                .as_irq_chip_mut()
                                .unregister_edge_irq_event(irq_num, &edge_evt)?;
                            Ok(())
                        }
                    },
                    &mut sys_allocator,
                );
                if let Err(e) = tube.send(&response) {
                    error!("MSI handler: send response failed: {}", e);
                    break;
                }
            }
            Err(e) => {
                error!("MSI handler: tube recv failed: {}", e);
                break;
            }
        }
    }

    info!("MSI handler thread exiting");
}

/// IRQ handler thread — polls device IRQ eventfds and routes interrupts.
/// This is the macOS equivalent of Linux's `irq_handler_thread` in linux.rs.
/// It runs in a dedicated thread, blocking on WaitContext (kqueue) for
/// device IRQ events, and calls service_irq_event to mark them pending.
#[cfg(target_arch = "aarch64")]
fn irq_handler_thread(
    mut irq_chip: Box<dyn devices::IrqChipAArch64>,
    handler_control: Tube,
) -> anyhow::Result<()> {
    use base::EventToken;
    use base::ReadNotifier;
    use base::WaitContext;
    use devices::IrqChip;
    use devices::IrqEventIndex;
    use vm_control::IrqHandlerRequest;

    #[derive(EventToken)]
    enum Token {
        IrqFd { index: IrqEventIndex },
        HandlerControl,
    }

    let wait_ctx = WaitContext::build_with(&[(
        handler_control.get_read_notifier(),
        Token::HandlerControl,
    )])
    .context("failed to build IRQ handler wait context")?;

    // Register all IRQ event tokens from the IRQ chip.
    let irq_chip_mut = irq_chip.as_irq_chip_mut();
    let irq_event_tokens = irq_chip_mut
        .irq_event_tokens()
        .context("failed to get IRQ event tokens")?;

    for (index, _source, evt) in irq_event_tokens.iter() {
        wait_ctx
            .add(evt, Token::IrqFd { index: *index })
            .context("failed to add IRQ event to wait context")?;
    }

    info!(
        "IRQ handler thread started: {} IRQ event(s) registered",
        irq_event_tokens.len()
    );

    'wait: loop {
        let events = match wait_ctx.wait() {
            Ok(v) => v,
            Err(e) => {
                error!("IRQ handler poll error: {}", e);
                break 'wait;
            }
        };

        for event in events.iter().filter(|e| e.is_readable) {
            match event.token {
                Token::HandlerControl => {
                    match handler_control.recv::<IrqHandlerRequest>() {
                        Ok(IrqHandlerRequest::Exit) => {
                            info!("IRQ handler thread exiting");
                            break 'wait;
                        }
                        Ok(IrqHandlerRequest::RefreshIrqEventTokens) => {
                            // Remove old tokens, re-register new ones.
                            for (_index, _source, evt) in irq_event_tokens.iter() {
                                let _ = wait_ctx.delete(evt);
                            }
                            let new_tokens = irq_chip_mut
                                .irq_event_tokens()
                                .context("failed to refresh IRQ event tokens")?;
                            for (index, _source, evt) in new_tokens.iter() {
                                wait_ctx
                                    .add(evt, Token::IrqFd { index: *index })
                                    .context("failed to re-add IRQ event")?;
                            }
                            // Note: can't reassign irq_event_tokens here due to borrow
                            // issues with irq_chip_mut — tokens are refreshed in place.
                        }
                        Ok(_) => {} // Ignore other requests
                        Err(e) => {
                            error!("IRQ handler control recv error: {}", e);
                            break 'wait;
                        }
                    }
                }
                Token::IrqFd { index } => {
                    if let Err(e) = irq_chip_mut.service_irq_event(index) {
                        error!("failed to service IRQ event {}: {}", index, e);
                    }
                }
            }
        }
    }

    Ok(())
}

/// vCPU run loop — executes guest code and handles VM exits.
#[cfg(target_arch = "aarch64")]
fn vcpu_loop(
    vcpu: &mut impl VcpuAArch64,
    vm: &impl Vm,
    io_bus: &devices::Bus,
    mmio_bus: &devices::Bus,
    hypercall_bus: &devices::Bus,
    irq_chip: &dyn devices::IrqChipAArch64,
    psci_exit_request: &Arc<AtomicU8>,
) -> Result<ExitState> {
    use devices::IrqChip;
    let irq_chip = irq_chip.as_irq_chip();

    loop {
        // Inject pending interrupts before running.
        irq_chip.inject_interrupts(vcpu as &dyn Vcpu)?;

        match vcpu.run() {
            Ok(exit) => match exit {
                VcpuExit::Mmio => {
                    if let Err(e) = vcpu.handle_mmio(&mut |IoParams { address, operation }| {
                        match operation {
                            IoOperation::Read(data) => {
                                mmio_bus.read(address, data);
                                Ok(())
                            }
                            IoOperation::Write(data) => {
                                mmio_bus.write(address, data);
                                // Signal ioevents for this address.
                                let _ = vm.handle_io_events(
                                    hypervisor::IoEventAddress::Mmio(address),
                                    data,
                                );
                                Ok(())
                            }
                        }
                    }) {
                        error!("vCPU MMIO error: {}", e);
                    }
                }
                VcpuExit::Io => {}
                VcpuExit::Hlt => {}
                VcpuExit::Intr => {}
                VcpuExit::Shutdown(_) => return Ok(ExitState::Stop),
                VcpuExit::SystemEventShutdown => return Ok(ExitState::Stop),
                VcpuExit::SystemEventReset => return Ok(ExitState::Reset),
                VcpuExit::SystemEventCrash => return Ok(ExitState::Crash),
                VcpuExit::Hypercall => {
                    if let Err(e) = vcpu.handle_hypercall(&mut |abi| {
                        hypercall_bus.handle_hypercall(abi)
                    }) {
                        error!("hypercall error: {}", e);
                    }
                    match psci_exit_request.load(Ordering::Acquire) {
                        devices::PSCI_EXIT_SHUTDOWN => return Ok(ExitState::Stop),
                        devices::PSCI_EXIT_RESET => return Ok(ExitState::Reset),
                        _ => {}
                    }
                }
                VcpuExit::Exception => {
                    // Guest exception. Check if we should exit.
                    match psci_exit_request.load(Ordering::Acquire) {
                        devices::PSCI_EXIT_SHUTDOWN => return Ok(ExitState::Stop),
                        devices::PSCI_EXIT_RESET => return Ok(ExitState::Reset),
                        _ => {
                            // Continue — the guest may handle the exception.
                            // The exception is already logged by HvfVcpu::run().
                        }
                    }
                }
                VcpuExit::Canceled => {
                    // hv_vcpu_cancel was called (SYSTEM_OFF/RESET).
                    match psci_exit_request.load(Ordering::Acquire) {
                        devices::PSCI_EXIT_SHUTDOWN => return Ok(ExitState::Stop),
                        devices::PSCI_EXIT_RESET => return Ok(ExitState::Reset),
                        _ => return Ok(ExitState::Stop),
                    }
                }
                _ => {} // Ignore other exit types
            },
            Err(e) => {
                error!("vCPU run error: {}", e);
                return Ok(ExitState::Crash);
            }
        }
    }
}
