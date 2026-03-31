// macOS networking — vmnet.framework backend for virtio-net.

mod vmnet_tap;

use crate::TapTCommon;

pub use vmnet_tap::VmnetTap;

/// macOS TAP trait — implemented by VmnetTap.
pub trait TapT: TapTCommon {}
