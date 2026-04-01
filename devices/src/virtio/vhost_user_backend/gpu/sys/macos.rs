// macOS vhost-user GPU backend stub.
// The vhost-user GPU device is not available on macOS. The in-process
// virtio-gpu device path is used instead.

use anyhow::anyhow;
use argh::FromArgs;

use crate::virtio::Interrupt;
use super::super::GpuBackend;

/// GPU device options (macOS — vhost-user GPU not available).
#[derive(FromArgs)]
#[argh(subcommand, name = "gpu")]
pub struct Options {
    #[argh(option, description = "stub")]
    _stub: Option<String>,
}

/// Run the GPU device (macOS — not implemented, use in-process GPU).
pub fn run_gpu_device(_opts: Options) -> anyhow::Result<()> {
    Err(anyhow!("vhost-user GPU backend is not available on macOS"))
}

impl GpuBackend {
    pub fn start_platform_workers(&mut self, _interrupt: Interrupt) -> anyhow::Result<()> {
        // macOS: no platform-specific workers needed for the stub backend.
        // Resource bridges are not used on macOS.
        Ok(())
    }
}
