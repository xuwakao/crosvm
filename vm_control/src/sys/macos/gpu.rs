// macOS GPU display and mouse mode types for vm_control.
// Mirrors the Linux UnixDisplayMode/UnixMouseMode types.

use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;

use crate::gpu::DisplayModeTrait;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, FromKeyValues)]
#[serde(rename_all = "snake_case")]
pub enum MacosMouseMode {
    Touchscreen,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacosDisplayMode {
    Windowed(u32, u32),
}

impl DisplayModeTrait for MacosDisplayMode {
    fn get_window_size(&self) -> (u32, u32) {
        match self {
            Self::Windowed(width, height) => (*width, *height),
        }
    }

    fn get_virtual_display_size(&self) -> (u32, u32) {
        self.get_window_size()
    }

    fn get_virtual_display_size_4k_uhd(&self, _is_4k_uhd_enabled: bool) -> (u32, u32) {
        self.get_window_size()
    }
}
