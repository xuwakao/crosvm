// macOS networking — vmnet.framework backend for virtio-net.

mod vmnet_tap;

use base::FileReadWriteVolatile;
use base::ReadNotifier;

use crate::TapTCommon;

pub use vmnet_tap::VmnetTap;

/// macOS TAP trait — includes FileReadWriteVolatile and ReadNotifier
/// for compatibility with crosvm's virtio-net Worker.
pub trait TapT: TapTCommon + FileReadWriteVolatile + ReadNotifier {}
