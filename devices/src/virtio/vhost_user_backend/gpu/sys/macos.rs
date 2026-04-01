// macOS vhost-user GPU backend stub.
// Provides the required type exports (Options, run_gpu_device) but does not
// implement a functional vhost-user GPU backend. The in-process GPU device
// path is used instead on macOS.

use anyhow::anyhow;
use argh::FromArgs;

/// GPU device options (macOS stub — vhost-user GPU not implemented).
#[derive(FromArgs)]
#[argh(subcommand, name = "gpu")]
pub struct Options {
    #[argh(option, description = "stub")]
    _stub: Option<String>,
}

/// Run the GPU device (macOS stub — not implemented).
pub fn run_gpu_device(_opts: Options) -> anyhow::Result<()> {
    Err(anyhow!("vhost-user GPU backend is not available on macOS"))
}
