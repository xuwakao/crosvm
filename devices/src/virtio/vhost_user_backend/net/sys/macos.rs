// macOS vhost-user net backend stubs.
// vhost-user net backend is not used on macOS (we use VmnetTap directly).

pub struct Options;

pub fn start_device(_opts: Options) -> anyhow::Result<()> {
    anyhow::bail!("vhost-user-net backend not supported on macOS")
}

pub(in crate::virtio::vhost_user_backend::net) fn start_queue(
) -> anyhow::Result<()> {
    anyhow::bail!("vhost-user-net backend not supported on macOS")
}
