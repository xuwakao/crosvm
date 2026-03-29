// macOS serial stub
use crate::serial_device::SerialInput;
pub(crate) type InStreamType = Box<dyn SerialInput>;
