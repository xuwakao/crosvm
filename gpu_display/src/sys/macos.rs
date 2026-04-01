// macOS GPU display backend stub.
// Currently provides a stub display (no visible window).
// Future: Metal-based display backend for framebuffer rendering.

use base::AsRawDescriptor;
use base::RawDescriptor;
use base::WaitContext;

use crate::DisplayT;
use crate::EventDevice;
use crate::GpuDisplay;
use crate::GpuDisplayExt;
use crate::GpuDisplayResult;
use crate::gpu_display_stub::DisplayStub;
use crate::DisplayEventToken;

pub(crate) trait MacosDisplayT: DisplayT {}

// DisplayStub implements MacosDisplayT via `impl SysDisplayT for DisplayStub` in gpu_display_stub.rs

pub trait MacosGpuDisplayExt {
    fn open_stub() -> GpuDisplayResult<GpuDisplay>;
}

impl MacosGpuDisplayExt for GpuDisplay {
    fn open_stub() -> GpuDisplayResult<GpuDisplay> {
        let display = DisplayStub::new()?;
        let wait_ctx = WaitContext::new()?;
        wait_ctx.add(&display, DisplayEventToken::Display)?;

        Ok(GpuDisplay {
            inner: Box::new(display),
            next_id: 1,
            event_devices: Default::default(),
            surfaces: Default::default(),
            wait_ctx,
        })
    }
}

impl AsRawDescriptor for GpuDisplay {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.inner.as_raw_descriptor()
    }
}

impl GpuDisplayExt for GpuDisplay {
    fn import_event_device(&mut self, _event_device: EventDevice) -> GpuDisplayResult<u32> {
        // macOS stub: no event device support yet.
        let id = self.next_id;
        self.next_id += 1;
        Ok(id)
    }

    fn handle_event_device(&mut self, _event_device_id: u32) {
        // macOS stub: no-op.
    }
}
