// macOS console input stub
use std::collections::VecDeque;
use std::sync::Arc;

use base::Event;
use sync::Mutex;

use crate::serial::sys::InStreamType;
use base::WorkerThread;

pub(in crate::virtio::console) fn spawn_input_thread(
    input: InStreamType,
    in_avail_evt: Event,
    input_buffer: Arc<Mutex<VecDeque<u8>>>,
) -> WorkerThread<InStreamType> {
    WorkerThread::start("v_console_input", move |_kill_evt| {
        // macOS: minimal input thread that just signals availability
        if !input_buffer.lock().is_empty() {
            in_avail_evt.signal().unwrap();
        }
        input
    })
}
