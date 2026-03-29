// macOS console stub
use base::Event;
pub(in crate::virtio::console) fn spawn_input_thread(
    _rx: Box<dyn std::io::Read + Send>,
    _in_avail_evt: &Event,
) {
    // No console input thread on macOS yet.
}
