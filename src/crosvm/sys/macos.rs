// macOS platform module for crosvm.
// Provides ExitState, run_config with HVF backend.

pub mod cmdline;
pub mod config;

use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use arch::LinuxArch;
use arch::VmComponents;
use arch::VmImage;
use base::debug;
use base::open_file_or_duplicate;
use base::Event;
use base::SendTube;
use base::Tube;
use hypervisor::ProtectionType;
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
    debug!("run_config: starting HVF VM on macOS");

    let components = setup_vm_components(&cfg)?;

    #[cfg(target_arch = "aarch64")]
    {
        use hypervisor::hvf::Hvf;
        use hypervisor::hvf::vm::HvfVm;

        let hvf = Hvf::new().context("failed to create HVF hypervisor")?;

        let arch_memory_layout =
            Arch::arch_memory_layout(&components).context("failed to create arch memory layout")?;
        let guest_mem = create_guest_memory(&components, &arch_memory_layout, &hvf)?;

        let vm = HvfVm::new(hvf, guest_mem)
            .context("failed to create HVF VM")?;

        // For now, we've proven the HVF VM creation path works.
        // The full run_vm integration (device setup, vCPU loop, serial) is next.
        debug!("HVF VM created successfully. Memory mapped. IRQ chip ready.");
        debug!("Full vCPU execution loop not yet wired — exiting.");

        Ok(ExitState::Stop)
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        bail!("HVF is only supported on AArch64")
    }
}
