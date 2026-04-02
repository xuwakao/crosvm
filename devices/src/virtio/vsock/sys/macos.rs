// macOS virtio-vsock — userspace implementation using Unix domain sockets.
//
// On Linux, vsock delegates to the kernel vhost-vsock module. macOS has no
// vhost subsystem, so we implement the full vsock device in userspace,
// similar to the Windows crosvm implementation.
//
// Architecture:
//   Guest AF_VSOCK socket ←→ virtio queues ←→ Worker thread ←→ Unix domain sockets
//
// The worker thread handles three virtqueues (RX, TX, event) and routes
// packets between the guest and host-side Unix sockets.

use std::collections::BTreeMap;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;
use anyhow::Context;
use base::error;
use base::info;
use base::warn;
use base::AsRawDescriptor;
use base::Event;
use base::RawDescriptor;
use base::WorkerThread;
use data_model::Le16;
use data_model::Le32;
use data_model::Le64;
use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;
use vm_memory::GuestMemory;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

use crate::virtio::copy_config;
use crate::virtio::DeviceType;
use crate::virtio::Interrupt;
use crate::virtio::Queue;
use crate::virtio::VirtioDevice;

// ============================================================================
// Protocol types (from virtio spec, shared with Windows implementation)
// ============================================================================

pub const TYPE_STREAM_SOCKET: u16 = 1;

/// Host CID is always 2 per the virtio-vsock spec.
const HOST_CID: u64 = 2;

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_vsock_config {
    pub guest_cid: Le64,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C, packed)]
#[allow(non_camel_case_types)]
pub struct virtio_vsock_hdr {
    pub src_cid: Le64,
    pub dst_cid: Le64,
    pub src_port: Le32,
    pub dst_port: Le32,
    pub len: Le32,
    pub type_: Le16,
    pub op: Le16,
    pub flags: Le32,
    pub buf_alloc: Le32,
    pub fwd_cnt: Le32,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_vsock_event {
    pub id: Le32,
}

pub mod vsock_op {
    pub const VIRTIO_VSOCK_OP_INVALID: u16 = 0;
    pub const VIRTIO_VSOCK_OP_REQUEST: u16 = 1;
    pub const VIRTIO_VSOCK_OP_RESPONSE: u16 = 2;
    pub const VIRTIO_VSOCK_OP_RST: u16 = 3;
    pub const VIRTIO_VSOCK_OP_SHUTDOWN: u16 = 4;
    pub const VIRTIO_VSOCK_OP_RW: u16 = 5;
    pub const VIRTIO_VSOCK_OP_CREDIT_UPDATE: u16 = 6;
    pub const VIRTIO_VSOCK_OP_CREDIT_REQUEST: u16 = 7;
}

// ============================================================================
// Configuration
// ============================================================================

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, FromKeyValues)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct VsockConfig {
    pub cid: u64,
}

impl VsockConfig {
    pub fn new(cid: u64) -> Self {
        Self { cid }
    }
}

// ============================================================================
// Queue constants
// ============================================================================

const QUEUE_SIZE: u16 = 256;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE, QUEUE_SIZE, QUEUE_SIZE];

/// Default buffer allocation reported to guest for flow control.
const DEFAULT_BUF_ALLOC: u32 = 128 * 1024;

/// Temp read buffer size for reading from host Unix sockets.
const READ_BUF_SIZE: usize = 4096;

/// Minimum free buffer percentage before sending credit update.
const MIN_FREE_BUFFER_PCT: f64 = 0.1;

// ============================================================================
// Connection state
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PortPair {
    host: u32,
    guest: u32,
}

impl std::fmt::Display for PortPair {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "host:{}<->guest:{}", self.host, self.guest)
    }
}

struct VsockConnection {
    stream: UnixStream,
    guest_port: u32,
    host_port: u32,
    buf_alloc: u32,
    recv_cnt: u32,
    prev_recv_cnt: u32,
    peer_buf_alloc: u32,
    peer_recv_cnt: u32,
    tx_cnt: u32,
}

impl VsockConnection {
    fn peer_free_space(&self) -> u32 {
        self.peer_buf_alloc.saturating_sub(self.tx_cnt.wrapping_sub(self.peer_recv_cnt))
    }
}

type ConnectionMap = HashMap<PortPair, VsockConnection>;

// ============================================================================
// Worker
// ============================================================================

struct Worker {
    mem: GuestMemory,
    guest_cid: u64,
    rx_queue: Queue,
    tx_queue: Queue,
    event_queue: Queue,
    connections: ConnectionMap,
    socket_dir: PathBuf,
    listeners: HashMap<u32, UnixListener>, // host port → listener
}

impl Worker {
    fn new(
        mem: GuestMemory,
        guest_cid: u64,
        rx_queue: Queue,
        tx_queue: Queue,
        event_queue: Queue,
    ) -> Self {
        let socket_dir = PathBuf::from(format!("/tmp/aetheria-vsock-{}", guest_cid));
        Worker {
            mem,
            guest_cid,
            rx_queue,
            tx_queue,
            event_queue,
            connections: HashMap::new(),
            socket_dir,
            listeners: HashMap::new(),
        }
    }

    /// Main event loop: poll TX queue for guest packets, poll Unix sockets for host data.
    fn run(&mut self, kill_evt: &Event) {
        use base::WaitContext;
        use base::EventToken;

        #[derive(EventToken, Debug, Clone)]
        enum Token {
            TxQueue,
            Kill,
        }

        let wait_ctx = match WaitContext::build_with(&[
            (self.tx_queue.event(), Token::TxQueue),
            (kill_evt, Token::Kill),
        ]) {
            Ok(ctx) => ctx,
            Err(e) => {
                error!("vsock: failed to create wait context: {}", e);
                return;
            }
        };

        // Create socket directory for host-initiated connections.
        if let Err(e) = std::fs::create_dir_all(&self.socket_dir) {
            error!("vsock: failed to create socket dir {:?}: {}", self.socket_dir, e);
        }

        info!("vsock: worker started, cid={}, socket_dir={:?}", self.guest_cid, self.socket_dir);

        'wait: loop {
            // Use a short timeout so we can poll Unix sockets for incoming data.
            // This is simpler than dynamically adding socket fds to the WaitContext.
            let events = match wait_ctx.wait_timeout(std::time::Duration::from_millis(10)) {
                Ok(v) => v,
                Err(e) => {
                    error!("vsock: wait error: {}", e);
                    break;
                }
            };

            for event in events.iter().filter(|e| e.is_readable) {
                match event.token {
                    Token::TxQueue => {
                        if let Err(e) = self.tx_queue.event().wait() {
                            error!("vsock: tx queue event error: {}", e);
                            break 'wait;
                        }
                        self.process_tx_queue();
                    }
                    Token::Kill => {
                        let _ = kill_evt.wait();
                        break 'wait;
                    }
                }
            }

            // Poll all active connections for incoming host data → guest RX queue.
            self.poll_host_sockets();
        }

        // Cleanup socket directory.
        let _ = std::fs::remove_dir_all(&self.socket_dir);
        info!("vsock: worker exiting");
    }

    /// Poll all connected Unix sockets for readable data and forward to guest RX queue.
    fn poll_host_sockets(&mut self) {
        let port_pairs: Vec<PortPair> = self.connections.keys().cloned().collect();
        for port_pair in port_pairs {
            let mut buf = [0u8; READ_BUF_SIZE];
            let read_result = {
                if let Some(conn) = self.connections.get_mut(&port_pair) {
                    match conn.stream.read(&mut buf) {
                        Ok(0) => Err(io::ErrorKind::ConnectionReset),
                        Ok(n) => Ok(n),
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                        Err(e) => Err(e.kind()),
                    }
                } else {
                    continue;
                }
            };

            match read_result {
                Ok(n) => {
                    // Build RW packet: header + data → guest RX queue.
                    let conn = self.connections.get_mut(&port_pair).unwrap();
                    conn.tx_cnt = conn.tx_cnt.wrapping_add(n as u32);

                    let hdr = virtio_vsock_hdr {
                        src_cid: Le64::from(HOST_CID),
                        dst_cid: Le64::from(self.guest_cid),
                        src_port: Le32::from(port_pair.host),
                        dst_port: Le32::from(port_pair.guest),
                        len: Le32::from(n as u32),
                        type_: Le16::from(TYPE_STREAM_SOCKET),
                        op: Le16::from(vsock_op::VIRTIO_VSOCK_OP_RW),
                        flags: Le32::from(0u32),
                        buf_alloc: Le32::from(conn.buf_alloc),
                        fwd_cnt: Le32::from(conn.recv_cnt),
                    };
                    self.write_to_rx_queue(hdr.as_bytes(), &buf[..n]);
                }
                Err(_) => {
                    // Connection closed or error — send RST to guest.
                    info!("vsock: host socket closed for {}", port_pair);
                    self.connections.remove(&port_pair);
                    self.send_response(port_pair.guest, port_pair.host, vsock_op::VIRTIO_VSOCK_OP_RST);
                }
            }
        }
    }

    /// Process all pending packets in the TX queue (guest → host).
    fn process_tx_queue(&mut self) {
        while let Some(mut desc_chain) = self.tx_queue.pop() {
            let reader = &mut desc_chain.reader;

            // Read the vsock header.
            if reader.available_bytes() < std::mem::size_of::<virtio_vsock_hdr>() {
                self.tx_queue.add_used(desc_chain);
                continue;
            }

            let hdr: virtio_vsock_hdr = match reader.read_obj() {
                Ok(h) => h,
                Err(e) => {
                    error!("vsock: tx: failed to read header: {}", e);
                    self.tx_queue.add_used(desc_chain);
                    continue;
                }
            };

            let payload_len = reader.available_bytes();
            let op = hdr.op.to_native();

            match op {
                vsock_op::VIRTIO_VSOCK_OP_REQUEST => {
                    self.handle_op_request(&hdr);
                }
                vsock_op::VIRTIO_VSOCK_OP_RW => {
                    if payload_len > 0 {
                        let mut buf = vec![0u8; payload_len];
                        let copied = reader.read_to_volatile_slice(
                            base::VolatileSlice::new(&mut buf[..payload_len]),
                        );
                        if copied > 0 {
                            self.handle_op_rw(&hdr, &buf[..copied]);
                        }
                    }
                }
                vsock_op::VIRTIO_VSOCK_OP_RST => {
                    self.handle_op_shutdown(&hdr);
                }
                vsock_op::VIRTIO_VSOCK_OP_SHUTDOWN => {
                    self.handle_op_shutdown(&hdr);
                }
                vsock_op::VIRTIO_VSOCK_OP_CREDIT_UPDATE => {
                    self.handle_op_credit_update(&hdr);
                }
                vsock_op::VIRTIO_VSOCK_OP_CREDIT_REQUEST => {
                    self.handle_op_credit_request(&hdr);
                }
                _ => {
                    warn!("vsock: tx: unknown op {}", op);
                }
            }

            self.tx_queue.add_used(desc_chain);
        }
        self.tx_queue.trigger_interrupt();
    }

    /// Guest requests a connection to a host port.
    fn handle_op_request(&mut self, hdr: &virtio_vsock_hdr) {
        let guest_port = hdr.src_port.to_native();
        let host_port = hdr.dst_port.to_native();
        let port_pair = PortPair { host: host_port, guest: guest_port };

        info!("vsock: OP_REQUEST {}", port_pair);

        // Try connecting to the host-side Unix socket.
        let socket_path = self.socket_dir.join(format!("port-{}", host_port));
        match UnixStream::connect(&socket_path) {
            Ok(stream) => {
                stream.set_nonblocking(true).ok();
                let conn = VsockConnection {
                    stream,
                    guest_port,
                    host_port,
                    buf_alloc: DEFAULT_BUF_ALLOC,
                    recv_cnt: 0,
                    prev_recv_cnt: 0,
                    peer_buf_alloc: hdr.buf_alloc.to_native(),
                    peer_recv_cnt: hdr.fwd_cnt.to_native(),
                    tx_cnt: 0,
                };
                self.connections.insert(port_pair, conn);
                self.send_response(guest_port, host_port, vsock_op::VIRTIO_VSOCK_OP_RESPONSE);
                info!("vsock: connected {}", port_pair);
            }
            Err(e) => {
                warn!("vsock: connect failed for {}: {} (path: {:?})", port_pair, e, socket_path);
                self.send_response(guest_port, host_port, vsock_op::VIRTIO_VSOCK_OP_RST);
            }
        }
    }

    /// Guest sends data to host.
    fn handle_op_rw(&mut self, hdr: &virtio_vsock_hdr, data: &[u8]) {
        let port_pair = PortPair {
            host: hdr.dst_port.to_native(),
            guest: hdr.src_port.to_native(),
        };

        if let Some(conn) = self.connections.get_mut(&port_pair) {
            conn.peer_recv_cnt = hdr.fwd_cnt.to_native();
            conn.peer_buf_alloc = hdr.buf_alloc.to_native();
            conn.recv_cnt = conn.recv_cnt.wrapping_add(data.len() as u32);

            // Write guest data to the host Unix socket. The socket is non-blocking,
            // so write_all may fail with WouldBlock/EAGAIN if the buffer is full.
            // This is NOT a fatal error — retry with backoff instead of killing
            // the connection.
            let mut written = 0;
            while written < data.len() {
                match conn.stream.write(&data[written..]) {
                    Ok(n) => written += n,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Buffer full — brief sleep then retry.
                        // This happens during heavy virtiofs I/O when the daemon
                        // can't drain the socket fast enough.
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(e) => {
                        error!("vsock: write to {} failed: {}", port_pair, e);
                        self.connections.remove(&port_pair);
                        self.send_response(
                            hdr.src_port.to_native(),
                            hdr.dst_port.to_native(),
                            vsock_op::VIRTIO_VSOCK_OP_RST,
                        );
                        return;
                    }
                }
            }

            // Check if we need to send a credit update.
            let threshold = (MIN_FREE_BUFFER_PCT * conn.buf_alloc as f64) as u32;
            let consumed_since_last = conn.recv_cnt.wrapping_sub(conn.prev_recv_cnt);
            if consumed_since_last > conn.buf_alloc.saturating_sub(threshold) {
                conn.prev_recv_cnt = conn.recv_cnt;
                self.send_credit_update(port_pair);
            }
        } else {
            warn!("vsock: OP_RW for unknown connection {}", port_pair);
            self.send_response(
                hdr.src_port.to_native(),
                hdr.dst_port.to_native(),
                vsock_op::VIRTIO_VSOCK_OP_RST,
            );
        }
    }

    /// Guest shuts down a connection.
    fn handle_op_shutdown(&mut self, hdr: &virtio_vsock_hdr) {
        let port_pair = PortPair {
            host: hdr.dst_port.to_native(),
            guest: hdr.src_port.to_native(),
        };
        info!("vsock: OP_SHUTDOWN {}", port_pair);
        self.connections.remove(&port_pair);
        self.send_response(
            hdr.src_port.to_native(),
            hdr.dst_port.to_native(),
            vsock_op::VIRTIO_VSOCK_OP_RST,
        );
    }

    /// Guest reports its credit state.
    fn handle_op_credit_update(&mut self, hdr: &virtio_vsock_hdr) {
        let port_pair = PortPair {
            host: hdr.dst_port.to_native(),
            guest: hdr.src_port.to_native(),
        };
        if let Some(conn) = self.connections.get_mut(&port_pair) {
            conn.peer_recv_cnt = hdr.fwd_cnt.to_native();
            conn.peer_buf_alloc = hdr.buf_alloc.to_native();
        }
    }

    /// Guest requests our credit state.
    fn handle_op_credit_request(&mut self, hdr: &virtio_vsock_hdr) {
        let port_pair = PortPair {
            host: hdr.dst_port.to_native(),
            guest: hdr.src_port.to_native(),
        };
        self.send_credit_update(port_pair);
    }

    /// Write a response/control packet to the RX queue (host → guest).
    fn send_response(&mut self, guest_port: u32, host_port: u32, op: u16) {
        let conn = self.connections.get(&PortPair { host: host_port, guest: guest_port });
        let (buf_alloc, fwd_cnt) = conn
            .map(|c| (c.buf_alloc, c.recv_cnt))
            .unwrap_or((DEFAULT_BUF_ALLOC, 0));

        let hdr = virtio_vsock_hdr {
            src_cid: Le64::from(HOST_CID),
            dst_cid: Le64::from(self.guest_cid),
            src_port: Le32::from(host_port),
            dst_port: Le32::from(guest_port),
            len: Le32::from(0u32),
            type_: Le16::from(TYPE_STREAM_SOCKET),
            op: Le16::from(op),
            flags: Le32::from(0u32),
            buf_alloc: Le32::from(buf_alloc),
            fwd_cnt: Le32::from(fwd_cnt),
        };

        self.write_to_rx_queue(hdr.as_bytes(), &[]);
    }

    /// Send credit update for a connection.
    fn send_credit_update(&mut self, port_pair: PortPair) {
        if let Some(conn) = self.connections.get(&port_pair) {
            let hdr = virtio_vsock_hdr {
                src_cid: Le64::from(HOST_CID),
                dst_cid: Le64::from(self.guest_cid),
                src_port: Le32::from(port_pair.host),
                dst_port: Le32::from(port_pair.guest),
                len: Le32::from(0u32),
                type_: Le16::from(TYPE_STREAM_SOCKET),
                op: Le16::from(vsock_op::VIRTIO_VSOCK_OP_CREDIT_UPDATE),
                flags: Le32::from(0u32),
                buf_alloc: Le32::from(conn.buf_alloc),
                fwd_cnt: Le32::from(conn.recv_cnt),
            };
            self.write_to_rx_queue(hdr.as_bytes(), &[]);
        }
    }

    /// Write a header + optional payload to the guest RX queue.
    fn write_to_rx_queue(&mut self, hdr_bytes: &[u8], payload: &[u8]) {
        if let Some(mut desc_chain) = self.rx_queue.peek() {
            let writer = &mut desc_chain.writer;
            let needed = hdr_bytes.len() + payload.len();
            if writer.available_bytes() < needed {
                warn!("vsock: rx descriptor too small ({} < {})", writer.available_bytes(), needed);
                let desc_chain = desc_chain.pop();
                self.rx_queue.add_used(desc_chain);
                return;
            }
            if let Err(e) = writer.write_all(hdr_bytes) {
                error!("vsock: rx write header failed: {}", e);
                let desc_chain = desc_chain.pop();
                self.rx_queue.add_used(desc_chain);
                return;
            }
            if !payload.is_empty() {
                if let Err(e) = writer.write_all(payload) {
                    error!("vsock: rx write payload failed: {}", e);
                }
            }
            let desc_chain = desc_chain.pop();
            self.rx_queue.add_used(desc_chain);
            self.rx_queue.trigger_interrupt();
        } else {
            warn!("vsock: rx queue empty, dropping packet");
        }
    }
}

// ============================================================================
// Vsock VirtioDevice
// ============================================================================

pub struct Vsock {
    guest_cid: u64,
    features: u64,
    acked_features: u64,
    worker_thread: Option<WorkerThread<()>>,
}

impl Vsock {
    pub fn new(guest_cid: u64, base_features: u64) -> anyhow::Result<Self> {
        Ok(Vsock {
            guest_cid,
            features: base_features,
            acked_features: 0,
            worker_thread: None,
        })
    }

    fn get_config(&self) -> virtio_vsock_config {
        virtio_vsock_config {
            guest_cid: Le64::from(self.guest_cid),
        }
    }
}

impl VirtioDevice for Vsock {
    fn keep_rds(&self) -> Vec<RawDescriptor> {
        Vec::new()
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        copy_config(data, 0, self.get_config().as_bytes(), offset);
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Vsock
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, value: u64) {
        self.acked_features |= value;
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        _interrupt: Interrupt,
        mut queues: BTreeMap<usize, Queue>,
    ) -> anyhow::Result<()> {
        if queues.len() != QUEUE_SIZES.len() {
            return Err(anyhow!(
                "vsock: expected {} queues, got {}",
                QUEUE_SIZES.len(),
                queues.len(),
            ));
        }

        let rx_queue = queues.remove(&0).unwrap();
        let tx_queue = queues.remove(&1).unwrap();
        let event_queue = queues.remove(&2).unwrap();

        let guest_cid = self.guest_cid;
        self.worker_thread = Some(WorkerThread::start(
            "virtio_vsock",
            move |kill_evt| {
                let mut worker = Worker::new(
                    mem,
                    guest_cid,
                    rx_queue,
                    tx_queue,
                    event_queue,
                );
                worker.run(&kill_evt);
            },
        ));

        Ok(())
    }
}
