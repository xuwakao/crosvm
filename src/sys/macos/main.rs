// macOS main entry point helpers for crosvm.

use anyhow::anyhow;
use base::syslog;
use base::syslog::LogArgs;
use base::syslog::LogConfig;

use crate::crosvm::sys::cmdline::Commands;
use crate::crosvm::sys::cmdline::DeviceSubcommand;
use crate::CommandStatus;
use crate::Config;

pub(crate) fn start_device(_command: DeviceSubcommand) -> anyhow::Result<()> {
    anyhow::bail!("Device subcommands not yet supported on macOS")
}

pub(crate) fn cleanup() {
    // macOS: no child process reaping needed (no jail/sandbox).
}

pub(crate) fn run_command(_command: Commands, _log_args: LogArgs) -> anyhow::Result<()> {
    anyhow::bail!("Platform-specific commands not yet supported on macOS")
}

pub(crate) fn init_log(log_config: LogConfig, _cfg: &Config) -> anyhow::Result<()> {
    if let Err(e) = syslog::init_with(log_config) {
        eprintln!("failed to initialize syslog: {e}");
        return Err(anyhow!("failed to initialize syslog: {}", e));
    }
    Ok(())
}

pub(crate) fn error_to_exit_code(
    _res: &std::result::Result<CommandStatus, anyhow::Error>,
) -> i32 {
    1
}
