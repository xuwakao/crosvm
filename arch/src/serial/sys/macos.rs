// macOS arch serial — no sandboxing/ProxyDevice
use std::sync::Arc;

use base::RawDescriptor;
use devices::serial_device::SerialParameters;
use devices::BusDevice;
use devices::Serial;
use jail::FakeMinijailStub as Minijail;
use sync::Mutex;

use crate::DeviceRegistrationError;

pub fn add_serial_device(
    com: Serial,
    _serial_parameters: &SerialParameters,
    _serial_jail: Option<Minijail>,
    _preserved_descriptors: Vec<RawDescriptor>,
    #[cfg(feature = "swap")] _swap_controller: &mut Option<swap::SwapController>,
) -> std::result::Result<Arc<Mutex<dyn BusDevice>>, DeviceRegistrationError> {
    Ok(Arc::new(Mutex::new(com)))
}
