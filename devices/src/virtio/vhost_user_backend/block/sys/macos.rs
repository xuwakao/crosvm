// macOS vhost_user_backend block stub
use anyhow::Result;
pub struct Options;
pub fn start_device(_opts: Options) -> Result<()> {
    anyhow::bail!("vhost-user-backend block not supported on macOS")
}
