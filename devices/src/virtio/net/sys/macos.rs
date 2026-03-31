// macOS virtio-net TX/RX processing.
// Adapted from Linux implementation. Uses the TapT trait which is backed
// by VmnetTap on macOS.

use std::io;
use std::result;

use base::error;
use base::warn;
use base::FileReadWriteVolatile;
use net_util::TapT;

use super::super::NetError;
use crate::virtio::Queue;

use super::PendingBuffer;

/// Validates and configures a tap device for use with virtio-net.
/// On macOS with vmnet, no special configuration is needed.
pub fn validate_and_configure_tap<T: TapT>(_tap: &T, _vq_pairs: u16) -> result::Result<(), NetError> {
    // vmnet handles configuration internally. No validation needed.
    Ok(())
}

/// Converts virtio net feature flags to tap offload flags.
/// On macOS with vmnet, offloading is handled internally.
pub fn virtio_features_to_tap_offload(_features: u64) -> u32 {
    0
}

/// Process received packets from the tap and write them to the RX queue.
pub fn process_rx<T: TapT + FileReadWriteVolatile>(rx_queue: &mut Queue, mut tap: &mut T) -> result::Result<(), NetError> {
    let mut needs_interrupt = false;
    let mut exhausted_queue = false;

    loop {
        let mut desc_chain = match rx_queue.peek() {
            Some(desc) => desc,
            None => {
                exhausted_queue = true;
                break;
            }
        };

        let writer = &mut desc_chain.writer;

        match writer.write_from(&mut tap, writer.available_bytes()) {
            Ok(_) => {}
            Err(ref e) if e.kind() == io::ErrorKind::WriteZero => {
                warn!("net: rx: buffer is too small to hold frame");
                break;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                break;
            }
            Err(e) => {
                warn!("net: rx: failed to write slice: {}", e);
                return Err(NetError::WriteBuffer(e));
            }
        };

        let bytes_written = writer.bytes_written() as u32;
        if bytes_written > 0 {
            let desc_chain = desc_chain.pop();
            rx_queue.add_used(desc_chain);
            needs_interrupt = true;
        }
    }

    if needs_interrupt {
        rx_queue.trigger_interrupt();
    }

    if exhausted_queue {
        Err(NetError::RxDescriptorsExhausted)
    } else {
        Ok(())
    }
}

/// Process merged RX buffers from the tap.
pub fn process_mrg_rx<T: TapT + FileReadWriteVolatile>(
    rx_queue: &mut Queue,
    mut tap: &mut T,
    pending_buffer: &mut PendingBuffer,
) -> result::Result<(), NetError> {
    // Simplified: delegate to non-merged RX for now.
    // Full merged RX support can be added when performance optimization is needed.
    process_rx(rx_queue, tap)
}

/// Process TX packets from the queue and write them to the tap.
pub fn process_tx<T: TapT + FileReadWriteVolatile>(tx_queue: &mut Queue, mut tap: &mut T) {
    while let Some(mut desc_chain) = tx_queue.pop() {
        let reader = &mut desc_chain.reader;
        let expected_count = reader.available_bytes();
        match reader.read_to(&mut tap, expected_count) {
            Ok(count) => {
                if count != expected_count {
                    error!(
                        "net: tx: wrote only {} bytes of {} byte frame",
                        count, expected_count
                    );
                }
            }
            Err(e) => error!("net: tx: failed to write frame to tap: {}", e),
        }

        tx_queue.add_used(desc_chain);
    }

    tx_queue.trigger_interrupt();
}
