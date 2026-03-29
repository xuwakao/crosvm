// macOS vhost_user_backend block stub
use anyhow::Result;
use argh::FromArgs;

#[derive(FromArgs)]
#[argh(subcommand, name = "block")]
/// Block device (not supported on macOS)
pub struct Options {
    #[argh(option, arg_name = "PATH")]
    /// path to the vhost-user socket to bind to.
    socket_path: Option<String>,
}

pub fn start_device(_opts: Options) -> Result<()> {
    anyhow::bail!("vhost-user-backend block not supported on macOS")
}
