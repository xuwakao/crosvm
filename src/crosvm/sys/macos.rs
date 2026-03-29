// macOS platform module for crosvm.
// Provides ExitState, run_config, and platform-specific submodules.

pub mod cmdline;
pub mod config;

use anyhow::Context;
use anyhow::Result;
use base::debug;

use crate::crosvm::config::Config;

/// Possible exit states for a VM.
pub enum ExitState {
    Stop,
    Reset,
    Crash,
    GuestPanic,
    WatchdogReset,
}

/// Run the VM with the given configuration.
/// On macOS, this uses the HVF (Hypervisor.framework) backend.
pub fn run_config(cfg: Config) -> Result<ExitState> {
    debug!("run_config: starting HVF VM on macOS");

    // TODO: Implement full HVF VM run loop.
    // For now, the binary compiles and can parse arguments.
    // The actual VM execution will be implemented when we wire up:
    // 1. Hvf hypervisor creation
    // 2. HvfVm memory setup
    // 3. HvfVcpu creation and run loop
    // 4. Device setup (serial, virtio-fs, virtio-vsock)
    anyhow::bail!(
        "HVF VM execution not yet implemented. \
         Use --help to verify the binary is functional."
    )
}
