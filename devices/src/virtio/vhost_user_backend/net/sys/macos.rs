// macOS vhost-user net backend stubs.
// vhost-user net backend is not used on macOS (we use VmnetTap directly).

use net_util::TapT;
use vm_memory::GuestMemory;

use crate::virtio;
use super::super::NetBackend;

pub struct Options;

pub fn start_device(_opts: Options) -> anyhow::Result<()> {
    anyhow::bail!("vhost-user-net backend not supported on macOS")
}

pub(in crate::virtio::vhost_user_backend::net) fn start_queue<T: 'static + TapT + cros_async::IntoAsync>(
    _backend: &mut NetBackend<T>,
    _idx: usize,
    _queue: virtio::Queue,
    _mem: GuestMemory,
) -> anyhow::Result<()> {
    anyhow::bail!("vhost-user-net backend not supported on macOS")
}
