// macOS platform module for crosvm.
// Provides ExitState, run_config with HVF backend.

pub mod cmdline;
pub mod config;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
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
use base::open_file_or_duplicate;
use base::Event;
use base::SendTube;
use base::Tube;
use devices::serial_device::SerialHardware;
use devices::BusDeviceObj;
use hypervisor::ProtectionType;
use hypervisor::IoOperation;
use hypervisor::IoParams;
use hypervisor::Vcpu;
use hypervisor::VcpuAArch64;
use hypervisor::VcpuExit;
use hypervisor::Vm;
use hypervisor::VmAArch64;
use jail::FakeMinijailStub as Minijail;
use resources::SystemAllocator;
use sync::Condvar;
use sync::Mutex;
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
        extra_kernel_params: {
            let mut params = cfg.params.clone();
            params.push("earlycon=uart8250,mmio32,0x3f8".to_string());
            params.push("panic=30".to_string());
            params.push("keep_bootcon".to_string());
            params.push("rdinit=/init".to_string());
            params
        },
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

        let vm = HvfVm::new(hvf, guest_mem)
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

        // No extra devices for minimal boot.
        let devices: Vec<(Box<dyn BusDeviceObj>, Option<Minijail>)> = Vec::new();
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
            Some(std::path::PathBuf::from("/tmp/crosvm_fdt.dtb")), // dump FDT for debugging
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
            const GIC_DIST_BASE: u64 = 0x3FFF0000;
            const GIC_DIST_SIZE: u64 = 0x10000;
            let gicd = GicDistributor::new(64);
            linux.mmio_bus.insert(
                Arc::new(Mutex::new(gicd)),
                GIC_DIST_BASE,
                GIC_DIST_SIZE,
            ).expect("failed to register GIC distributor");

            const GIC_REDIST_BASE: u64 = 0x3FFD0000;
            const GIC_REDIST_SIZE_PER_CPU: u64 = 0x20000;
            for cpu_id in 0..vcpu_count {
                let gicr = GicRedistributor::new(cpu_id as u32, vcpu_count as u32);
                let base = GIC_REDIST_BASE + (cpu_id as u64) * GIC_REDIST_SIZE_PER_CPU;
                linux.mmio_bus.insert(
                    Arc::new(Mutex::new(gicr)),
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

        // Get vCPU init data. build_vm created vCPUs on the main thread,
        // but HVF requires vCPUs to be used on the thread that created them.
        // We'll re-create them in dedicated threads.
        let vcpu_init_data = linux.vcpu_init.clone();
        let vm_for_vcpus = linux.vm.try_clone().context("failed to clone vm")?;

        // Keep the main-thread vCPUs alive — their GIC redistributor associations
        // must persist until the thread vCPUs are created.
        let main_thread_vcpus = linux.vcpus.take();

        // Debug: check redistributor base for each vCPU
        if let Some(ref vcpus) = main_thread_vcpus {
            for vcpu in vcpus.iter() {
                // Read MPIDR via HVF sys_reg API directly
                let mut mpidr: u64 = 0;
                let ret = unsafe {
                    hypervisor::hvf::ffi::hv_vcpu_get_sys_reg(
                        vcpu.hvf_handle(),
                        hypervisor::hvf::ffi::HV_SYS_REG_MPIDR_EL1,
                        &mut mpidr,
                    )
                };
                info!("vCPU {} MPIDR={:#x} (ret={})", vcpu.id(), mpidr, ret);

                // Query redistributor base via HVF
                if hypervisor::hvf::ffi::hvf_gic_is_available() {
                    type GetRedistBaseFn = unsafe extern "C" fn(u64, *mut u64) -> i32;
                    let func: Option<GetRedistBaseFn> = unsafe {
                        let sym = libc::dlsym(libc::RTLD_DEFAULT,
                            b"hv_gic_get_redistributor_base\0".as_ptr() as *const _);
                        if sym.is_null() { None } else { Some(std::mem::transmute(sym)) }
                    };
                    if let Some(f) = func {
                        let mut redist_base: u64 = 0;
                        let ret = unsafe { f(vcpu.hvf_handle(), &mut redist_base) };
                        info!("  hv_gic_get_redistributor_base: ret={} base={:#x}", ret, redist_base);
                    }
                }
            }
        }
        let _main_thread_vcpus = main_thread_vcpus;

        let io_bus = linux.io_bus.clone();
        let mmio_bus = linux.mmio_bus.clone();
        let hypercall_bus = linux.hypercall_bus.clone();

        let mut vcpu_handles = Vec::new();
        for vcpu_id in 0..vcpu_count {
            let io_bus = io_bus.clone();
            let mmio_bus = mmio_bus.clone();
            let hypercall_bus = hypercall_bus.clone();
            let mut irq_chip_clone = IrqChip::try_clone(&irq_chip)
                .context("failed to clone irq chip")?;
            let init = vcpu_init_data[vcpu_id].clone();
            let vm_thread = vm_for_vcpus.try_clone().context("failed to clone vm")?;

            let handle = thread::Builder::new()
                .name(format!("crosvm_vcpu{}", vcpu_id))
                .spawn(move || -> Result<ExitState> {
                    // Create vCPU on this thread (HVF requirement).
                    let mut vcpu = HvfVcpu::new(vcpu_id)
                        .context("failed to create HVF vCPU")?;

                    // Register vCPU handle for cross-thread IRQ injection.
                    irq_chip_clone.set_vcpu_handle(vcpu_id, vcpu.hvf_handle());

                    // Configure vCPU with arch-specific init registers.
                    Arch::configure_vcpu(
                        &vm_thread,
                        vm_thread.get_hypervisor(),
                        &mut irq_chip_clone,
                        &mut vcpu,
                        init,
                        vcpu_id,
                        vcpu_count,
                        None, // cpu_config
                    )
                    .context("failed to configure vcpu")?;

                    vcpu_loop(&mut vcpu, &io_bus, &mmio_bus, &hypercall_bus, &irq_chip_clone)
                })
                .context("failed to spawn vcpu thread")?;
            vcpu_handles.push(handle);
        }

        // Wait for all vCPU threads to finish.
        let mut exit_state = ExitState::Stop;
        for handle in vcpu_handles {
            match handle.join() {
                Ok(Ok(state)) => {
                    info!("vCPU exited with {:?}", state);
                    exit_state = state;
                }
                Ok(Err(e)) => {
                    error!("vCPU thread error: {:#}", e);
                    exit_state = ExitState::Crash;
                }
                Err(_) => {
                    error!("vCPU thread panicked");
                    exit_state = ExitState::Crash;
                }
            }
        }

        // Shut down IRQ handler thread.
        if let Err(e) = irq_handler_control.send(&vm_control::IrqHandlerRequest::Exit) {
            error!("failed to send Exit to IRQ handler: {}", e);
        }
        if let Err(e) = irq_handler_join.join() {
            error!("IRQ handler thread panicked: {:?}", e);
        }

        Ok(exit_state)
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        bail!("HVF is only supported on AArch64")
    }
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
                    info!("IRQ event {} fired!", index);
                    if let Err(e) = irq_chip_mut.service_irq_event(index) {
                        error!("failed to service IRQ event {}: {}", index, e);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Minimal vCPU run loop.
#[cfg(target_arch = "aarch64")]
fn vcpu_loop(
    vcpu: &mut impl VcpuAArch64,
    io_bus: &devices::Bus,
    mmio_bus: &devices::Bus,
    hypercall_bus: &devices::Bus,
    irq_chip: &devices::HvfGicChip,
) -> Result<ExitState> {
    use devices::IrqChip;

    info!("vCPU {} starting execution", vcpu.id());

    let mut exit_count: u64 = 0;
    let mut mmio_count: u64 = 0;
    let mut hlt_count: u64 = 0;
    let mut msr_count: u64 = 0;
    let mut intr_count: u64 = 0;
    let mut hyper_count: u64 = 0;
    let mut other_count: u64 = 0;

    loop {
        // Inject pending interrupts before running.
        irq_chip.inject_interrupts(vcpu as &dyn Vcpu)?;

        // Run the vCPU until an exit.
        exit_count += 1;
        if exit_count <= 100 || exit_count % 100000 == 0 {
            let pc = vcpu.get_one_reg(hypervisor::VcpuRegAArch64::Pc).unwrap_or(0);
            info!(
                "vCPU {} exit #{}: mmio={} hlt={} intr={} hyper={} msr={} other={} PC={:#x}",
                vcpu.id(), exit_count, mmio_count, hlt_count, intr_count, hyper_count, msr_count, other_count, pc
            );
        }

        match vcpu.run() {
            Ok(exit) => match exit {
                VcpuExit::Mmio => {
                    mmio_count += 1;
                    if let Err(e) = vcpu.handle_mmio(&mut |IoParams { address, operation }| {
                        match operation {
                            IoOperation::Read(data) => {
                                mmio_bus.read(address, data);
                                // Log ALL reads with returned data
                                if mmio_count <= 200 {
                                    let val = match data.len() {
                                        1 => data[0] as u64,
                                        2 => u16::from_le_bytes([data[0], data[1]]) as u64,
                                        4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64,
                                        8 => u64::from_le_bytes([data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7]]),
                                        _ => 0,
                                    };
                                    info!("  MMIO read  @ {:#x} [{}B] → {:#x}", address, data.len(), val);
                                }
                                Ok(())
                            }
                            IoOperation::Write(data) => {
                                // Detect serial writes at 0x3f8 (THR register)
                                if address >= 0x3f8 && address < 0x400 {
                                    if data.len() >= 1 {
                                        let ch = data[0];
                                        if ch >= 0x20 && ch < 0x7f {
                                            error!("SERIAL OUTPUT: '{}'", ch as char);
                                        } else {
                                            error!("SERIAL OUTPUT: byte {:#x}", ch);
                                        }
                                    }
                                }
                                if mmio_count <= 200 {
                                    let val = match data.len() {
                                        1 => data[0] as u64,
                                        2 => u16::from_le_bytes([data[0], data[1]]) as u64,
                                        4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64,
                                        8 => u64::from_le_bytes([data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7]]),
                                        _ => 0,
                                    };
                                    info!("  MMIO write @ {:#x} [{}B] ← {:#x}", address, data.len(), val);
                                }
                                mmio_bus.write(address, data);
                                Ok(())
                            }
                        }
                    }) {
                        error!("vCPU {} MMIO error: {}", vcpu.id(), e);
                    }
                }
                VcpuExit::Io => {
                    // x86-only, shouldn't happen on aarch64
                }
                VcpuExit::Hlt => {
                    hlt_count += 1;
                    // WFI — CPU is idle, continue (will block in next run)
                }
                VcpuExit::Shutdown(_) => {
                    info!("vCPU {} received shutdown", vcpu.id());
                    return Ok(ExitState::Stop);
                }
                VcpuExit::Intr => {
                    intr_count += 1;
                    if intr_count <= 5 || intr_count % 10000 == 0 {
                        let pc = vcpu.get_one_reg(hypervisor::VcpuRegAArch64::Pc).unwrap_or(0);
                        info!("  Intr #{} PC={:#x}", intr_count, pc);
                    }
                }
                VcpuExit::SystemEventShutdown => {
                    info!("vCPU {} system event shutdown", vcpu.id());
                    return Ok(ExitState::Stop);
                }
                VcpuExit::SystemEventReset => {
                    info!("vCPU {} system event reset", vcpu.id());
                    return Ok(ExitState::Reset);
                }
                VcpuExit::SystemEventCrash => {
                    error!("vCPU {} system event crash", vcpu.id());
                    return Ok(ExitState::Crash);
                }
                VcpuExit::MsrAccess => {
                    msr_count += 1;
                    if msr_count <= 5 {
                        info!("vCPU {} MsrAccess (system register trap)", vcpu.id());
                    }
                }
                VcpuExit::Hypercall => {
                    hyper_count += 1;
                    // Handle PSCI calls directly, dispatch others to hypercall bus.
                    if let Err(e) = vcpu.handle_hypercall(&mut |abi| {
                        let fid = abi.hypercall_id();
                        match fid {
                            // PSCI_VERSION (returns PSCI 1.0 = 0x10000)
                            0x84000000 => {
                                abi.set_results(&[0x10000, 0, 0, 0]);
                                Ok(())
                            }
                            // PSCI_MIGRATE_INFO_TYPE (returns TOS_NOT_PRESENT_MP)
                            0x84000006 => {
                                abi.set_results(&[2, 0, 0, 0]);
                                Ok(())
                            }
                            // PSCI_FEATURES
                            0x8400000a => {
                                let feature_id = abi.get_argument(0).copied().unwrap_or(0);
                                match feature_id {
                                    0x84000000 | 0x84000001 | 0x84000002 | 0xc4000003 |
                                    0x84000008 | 0x84000009 => {
                                        abi.set_results(&[0, 0, 0, 0]); // Supported
                                    }
                                    _ => {
                                        abi.set_results(&[u64::MAX as usize, 0, 0, 0]); // NOT_SUPPORTED
                                    }
                                }
                                Ok(())
                            }
                            // PSCI_CPU_OFF
                            0x84000002 => {
                                abi.set_results(&[0, 0, 0, 0]);
                                Ok(())
                            }
                            // PSCI_CPU_ON (64-bit)
                            0xc4000003 => {
                                abi.set_results(&[0, 0, 0, 0]);
                                Ok(())
                            }
                            // PSCI_SYSTEM_OFF
                            0x84000008 => {
                                info!("vCPU {} PSCI SYSTEM_OFF", vcpu.id());
                                abi.set_results(&[0, 0, 0, 0]);
                                Ok(())
                            }
                            // PSCI_SYSTEM_RESET
                            0x84000009 => {
                                // Full register dump before reset
                                use hypervisor::VcpuRegAArch64;
                                let pc = vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap_or(0);
                                let sp = vcpu.get_one_reg(VcpuRegAArch64::Sp).unwrap_or(0);
                                error!("=== PSCI SYSTEM_RESET (mmio={} exits={}) ===", mmio_count, exit_count);
                                error!("  PC={:#018x}  SP={:#018x}", pc, sp);
                                for i in (0..31).step_by(4) {
                                    let r = |n: u8| vcpu.get_one_reg(VcpuRegAArch64::X(n)).unwrap_or(0);
                                    if i + 3 < 31 {
                                        error!("  X{:02}={:#018x} X{:02}={:#018x} X{:02}={:#018x} X{:02}={:#018x}",
                                            i, r(i), i+1, r(i+1), i+2, r(i+2), i+3, r(i+3));
                                    }
                                }
                                abi.set_results(&[0, 0, 0, 0]);
                                Ok(())
                            }
                            _ => {
                                hypercall_bus.handle_hypercall(abi)
                            }
                        }
                    }) {
                        if exit_count <= 20 {
                            error!("vCPU {} hypercall error: {}", vcpu.id(), e);
                        }
                    }
                }
                other => {
                    other_count += 1;
                    if other_count <= 10 {
                        info!("vCPU {} unhandled exit: {:?}", vcpu.id(), other);
                    }
                }
            },
            Err(e) => {
                error!("vCPU {} run error: {}", vcpu.id(), e);
                return Ok(ExitState::Crash);
            }
        }
    }
}
