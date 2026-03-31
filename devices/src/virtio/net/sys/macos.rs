// macOS virtio-net TX/RX processing.
// Adapted from Linux implementation. Uses the TapT trait which is backed
// by VmnetTap on macOS.

use std::io;
use std::io::Write;
use std::result;

use base::error;
use base::warn;
use net_util::TapT;
use virtio_sys::virtio_net::virtio_net_hdr_v1;

use super::super::NetError;
use crate::virtio::Queue;

use base::EventType;
use base::ReadNotifier;
use base::WaitContext;

use super::super::Token;
use super::super::Worker;
use super::PendingBuffer;

/// Size of the virtio_net_hdr_v1 that the guest prepends to every packet.
/// vmnet expects raw Ethernet frames, so TX must strip this and RX must prepend it.
const VNET_HDR_SIZE: usize = std::mem::size_of::<virtio_net_hdr_v1>();

/// Maximum frame size: vmnet's default max_packet_size is 1514 (Ethernet MTU 1500 + 14 header).
/// Use 9014 for jumbo frame support (9000 payload + 14 header). Sized to avoid heap allocation.
const MAX_FRAME_SIZE: usize = 9014;

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
/// On macOS, vmnet returns raw Ethernet frames. The guest expects
/// virtio_net_hdr_v1 (12 bytes) prepended, so we write a zeroed header first.
pub fn process_rx<T: TapT>(rx_queue: &mut Queue, tap: &mut T) -> result::Result<(), NetError> {
    use std::io::Read;

    let mut needs_interrupt = false;
    let mut exhausted_queue = false;

    loop {
        // Check queue availability BEFORE reading from tap to avoid consuming
        // a packet that cannot be delivered to the guest.
        let mut desc_chain = match rx_queue.peek() {
            Some(desc) => desc,
            None => {
                exhausted_queue = true;
                break;
            }
        };

        // Read raw Ethernet frame from vmnet into a stack buffer.
        let mut frame_buf = [0u8; MAX_FRAME_SIZE];
        let frame_len = match tap.read(&mut frame_buf) {
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                break;
            }
            Err(e) => {
                warn!("net: rx: failed to read from tap: {}", e);
                return Err(NetError::WriteBuffer(e));
            }
        };

        // Validate frame: minimum Ethernet header is 14 bytes.
        if frame_len < 14 {
            if frame_len == 0 { break; }
            warn!("net: rx: dropping runt frame ({} bytes)", frame_len);
            let desc_chain = desc_chain.pop();
            rx_queue.add_used(desc_chain);
            continue;
        }

        let writer = &mut desc_chain.writer;
        let needed = VNET_HDR_SIZE + frame_len;
        if writer.available_bytes() < needed {
            warn!("net: rx: descriptor too small ({} < {}), dropping packet", writer.available_bytes(), needed);
            let desc_chain = desc_chain.pop();
            rx_queue.add_used(desc_chain);
            continue;
        }

        // Write zeroed virtio_net_hdr_v1 (no offload hints from vmnet).
        let hdr = [0u8; VNET_HDR_SIZE];
        if let Err(e) = writer.write_all(&hdr) {
            warn!("net: rx: failed to write vnet_hdr: {}", e);
            break;
        }

        // Write the Ethernet frame.
        if let Err(e) = writer.write_all(&frame_buf[..frame_len]) {
            warn!("net: rx: failed to write frame: {}", e);
            break;
        }

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
///
/// Currently delegates to non-merged process_rx. VIRTIO_NET_F_MRG_RXBUF is not
/// offered on macOS, so this path is only reached if a future change enables it.
/// Full merged RX (splitting large frames across multiple descriptors with
/// num_buffers in the header) can be added when jumbo frame performance requires it.
pub fn process_mrg_rx<T: TapT>(
    rx_queue: &mut Queue,
    tap: &mut T,
    _pending_buffer: &mut PendingBuffer,
) -> result::Result<(), NetError> {
    process_rx(rx_queue, tap)
}

/// Process TX packets from the queue and write them to the tap.
/// On macOS, the guest sends virtio_net_hdr_v1 (12 bytes) + Ethernet frame.
/// vmnet expects raw Ethernet frames, so we strip the header before writing.
pub fn process_tx<T: TapT + Write>(tx_queue: &mut Queue, tap: &mut T) {
    while let Some(mut desc_chain) = tx_queue.pop() {
        let reader = &mut desc_chain.reader;
        let available = reader.available_bytes();

        if available <= VNET_HDR_SIZE {
            tx_queue.add_used(desc_chain);
            continue;
        }

        // Skip the virtio_net_hdr_v1 — vmnet doesn't understand it.
        reader.consume(VNET_HDR_SIZE);

        // Read the Ethernet frame into a stack buffer and write to vmnet.
        let frame_len = reader.available_bytes();
        if frame_len > MAX_FRAME_SIZE {
            error!("net: tx: frame too large ({} > {}), dropping", frame_len, MAX_FRAME_SIZE);
            tx_queue.add_used(desc_chain);
            continue;
        }
        let mut frame_buf = [0u8; MAX_FRAME_SIZE];
        let copied = reader.read_to_volatile_slice(base::VolatileSlice::new(&mut frame_buf[..frame_len]));
        if copied != frame_len {
            // Scatter-gather descriptor chain returned fewer bytes than expected.
            // This should not happen with a well-behaved guest, but send what we got.
            warn!("net: tx: partial read {}/{} bytes", copied, frame_len);
        }
        if copied > 0 {
            if let Err(e) = tap.write_all(&frame_buf[..copied]) {
                // Log and continue — individual packet loss is recoverable via TCP retransmit
                // or application-level retry. Halting TX on transient errors would stall the guest.
                error!("net: tx: vmnet write failed: {}", e);
            }
        }

        tx_queue.add_used(desc_chain);
    }

    tx_queue.trigger_interrupt();
}

// Worker methods for macOS — equivalent to Linux's handle_rx_token/handle_rx_queue.
impl<T> Worker<T>
where
    T: TapT + ReadNotifier,
{
    pub(in crate::virtio) fn handle_rx_token(
        &mut self,
        wait_ctx: &WaitContext<Token>,
        pending_buffer: &mut PendingBuffer,
    ) -> result::Result<(), NetError> {
        match self.process_rx(pending_buffer) {
            Ok(()) => Ok(()),
            Err(NetError::RxDescriptorsExhausted) => {
                // Guest RX ring is full — stop polling the tap until the guest
                // refills descriptors. Re-enabled by handle_rx_queue when the
                // guest signals via Token::RxQueue.
                wait_ctx
                    .modify(&self.tap, EventType::None, Token::RxTap)
                    .map_err(NetError::WaitContextDisableTap)?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Called when the guest makes new RX descriptors available (Token::RxQueue).
    /// Re-enables tap polling so that buffered packets in vmnet can be delivered.
    pub(in crate::virtio) fn handle_rx_queue(
        &mut self,
        wait_ctx: &WaitContext<Token>,
        tap_polling_enabled: bool,
    ) -> result::Result<(), NetError> {
        if !tap_polling_enabled {
            wait_ctx
                .modify(&self.tap, EventType::Read, Token::RxTap)
                .map_err(NetError::WaitContextEnableTap)?;
        }
        Ok(())
    }

    pub(super) fn process_rx(
        &mut self,
        pending_buffer: &mut PendingBuffer,
    ) -> result::Result<(), NetError> {
        if self.acked_features & 1 << virtio_sys::virtio_net::VIRTIO_NET_F_MRG_RXBUF == 0 {
            process_rx(&mut self.rx_queue, &mut self.tap)
        } else {
            process_mrg_rx(&mut self.rx_queue, &mut self.tap, pending_buffer)
        }
    }
}
