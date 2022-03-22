#![allow(dead_code)]

use crate::portalnet::discovery::Discovery;
use anyhow::anyhow;
use async_recursion::async_recursion;
use discv5::enr::NodeId;
use discv5::{Enr, TalkRequest};
use log::{debug, warn};
use rand::Rng;
use ssz::Decode;
use std::cmp::{max, min};
use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::time::timeout;

use crate::portalnet::types::messages::ProtocolId;
use crate::utp::packets::{ExtensionType, Packet, PacketType, HEADER_SIZE};
use crate::utp::time::{now_microseconds, Delay, Timestamp};
use crate::utp::trin_helpers::{UtpAccept, UtpMessageId};
use crate::utp::util::{abs_diff, ewma, generate_sequential_identifiers};
use std::time::Duration;

// For simplicity's sake, let us assume no packet will ever exceed the
// Ethernet maximum transfer unit of 1500 bytes.
const BUF_SIZE: usize = 1500;
const GAIN: f64 = 1.0;
const ALLOWED_INCREASE: u32 = 1;
const MIN_CWND: u32 = 2; // minimum congestion window size
const INIT_CWND: u32 = 2; // init congestion window size
const MIN_CONGESTION_TIMEOUT: u64 = 500; // 500 ms
const MAX_CONGESTION_TIMEOUT: u64 = 60_000; // one minute
const MAX_RETRANSMISSION_RETRIES: u32 = 5; // maximum retransmission retries
const WINDOW_SIZE: u32 = 1024 * 1024; // local receive window size

// Maximum time (in microseconds) to wait for incoming packets when the send window is full
const PRE_SEND_TIMEOUT: u32 = 500_000;

const MAX_DISCV5_PACKET_SIZE: u32 = 1280;
const MAX_DISCV5_HEADER_SIZE: usize = 80;
// Size of the payload length in uTP message
const PAYLOAD_LENGTH_SIZE: usize = 32;
// Buffering delay that the uTP accepts on the up-link. Currently the delay target is set to 100 ms.
const CCONTROL_TARGET: f64 = 100_000.0;

const BASE_HISTORY: usize = 10; // base delays history size
                                // Maximum age of base delay sample (60 seconds)
const MAX_BASE_DELAY_AGE: Delay = Delay(60_000_000);

pub fn rand() -> u16 {
    rand::thread_rng().gen()
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum SocketState {
    Uninitialized,
    SynSent,
    SynRecv,
    FinSent,
    Connected,
    Closed,
    ResetReceived,
}

#[derive(Clone)]
struct DelayDifferenceSample {
    received_at: Timestamp,
    difference: Delay,
}

#[derive(Hash, Eq, PartialEq, Copy, Clone, Debug)]
pub struct ConnectionKey {
    pub node_id: NodeId,
    pub conn_id_recv: u16,
}

impl ConnectionKey {
    fn new(node_id: NodeId, conn_id_recv: u16) -> Self {
        Self {
            node_id,
            conn_id_recv,
        }
    }
}

// Basically the same idea as in the official Bit Torrent library we will store all of the active connections data here
pub struct UtpListener {
    pub discovery: Arc<Discovery>,
    pub utp_connections: HashMap<ConnectionKey, UtpSocket>,
    // We only want to listen/handle packets of connections that were negotiated with
    pub listening: HashMap<u16, UtpMessageId>,
    tx: mpsc::UnboundedSender<Packet>,
    rx: Arc<RwLock<mpsc::UnboundedReceiver<Packet>>>,
}

impl UtpListener {
    pub fn new(discovery: Arc<Discovery>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<Packet>();

        UtpListener {
            discovery,
            utp_connections: HashMap::new(),
            listening: HashMap::new(),
            tx,
            rx: Arc::new(RwLock::new(rx)),
        }
    }

    pub async fn process_utp_request(&mut self, request: TalkRequest) {
        let payload = request.body();
        let node_id = request.node_id();

        match Packet::try_from(payload) {
            Ok(packet) => {
                self.tx.send(packet.clone()).unwrap();

                let connection_id = packet.connection_id();

                match packet.get_type() {
                    PacketType::Reset => {
                        let key_fn =
                            |offset| ConnectionKey::new(*node_id, connection_id - 1 + offset);
                        let f = |conn: &&mut UtpSocket| -> bool {
                            conn.sender_connection_id == connection_id
                        };

                        if let Some(conn) = self.utp_connections.get_mut(&key_fn(1)) {
                            conn.state = SocketState::Closed;
                        } else if let Some(conn) =
                            self.utp_connections.get_mut(&key_fn(2)).filter(f)
                        {
                            conn.state = SocketState::Closed;
                        } else if let Some(conn) =
                            self.utp_connections.get_mut(&key_fn(0)).filter(f)
                        {
                            conn.state = SocketState::Closed;
                        }
                    }
                    PacketType::Syn => {
                        if let Some(enr) = self.discovery.discv5.find_enr(node_id) {
                            // If neither of those cases happened handle this is a new request
                            let mut conn = UtpSocket::new(
                                Arc::clone(&self.discovery),
                                enr.clone(),
                                Arc::clone(&self.rx),
                            );

                            let mut buf = [0; BUF_SIZE];

                            if let Err(msg) = conn.recv(&mut buf).await {
                                debug!("Unable to receive SYN packet {msg}");
                                return;
                            }

                            self.utp_connections.insert(
                                ConnectionKey {
                                    node_id: *node_id,
                                    conn_id_recv: conn.receiver_connection_id,
                                },
                                conn.clone(),
                            );
                        } else {
                            debug!("Query requested an unknown ENR");
                        }
                    }
                    // Receive DATA and FIN packets
                    PacketType::Data => {
                        if let Some(conn) = self.utp_connections.get_mut(&ConnectionKey {
                            node_id: *node_id,
                            conn_id_recv: connection_id,
                        }) {
                            let mut buf = [0; BUF_SIZE];
                            if let Err(msg) = conn.recv(&mut buf).await {
                                warn!("Unable to receive uTP DATA packet: {msg}")
                            } else {
                                conn.recv_data_stream
                                    .append(&mut Vec::from(packet.payload()));
                            }
                        }
                    }
                    PacketType::Fin => {
                        if let Some(conn) = self.utp_connections.get_mut(&ConnectionKey {
                            node_id: *node_id,
                            conn_id_recv: connection_id,
                        }) {
                            let mut buf = [0; BUF_SIZE];
                            if let Err(msg) = conn.recv(&mut buf).await {
                                warn!("Unable to receive uTP FIN packet: {msg}")
                            }
                        }
                    }
                    // We don't handle STATE packets here, because the uTP client is handling them
                    // implicitly in the background when sending FIN packet with conn.close()
                    PacketType::State => {}
                }
            }
            Err(e) => {
                debug!("Failed to decode packet: {}", e);
            }
        }
    }

    // I am honestly not sure if I should init this with Enr or NodeId since we could use both
    pub async fn connect(
        &mut self,
        connection_id: u16,
        node_id: NodeId,
    ) -> anyhow::Result<UtpSocket> {
        if let Some(enr) = self.discovery.discv5.find_enr(&node_id) {
            let mut conn = UtpSocket::new(Arc::clone(&self.discovery), enr, Arc::clone(&self.rx));
            conn.make_connection(connection_id).await;
            self.utp_connections.insert(
                ConnectionKey {
                    node_id,
                    conn_id_recv: connection_id,
                },
                conn.clone(),
            );
            Ok(conn)
        } else {
            Err(anyhow!("Trying to connect to unknow Enr"))
        }
    }

    // https://github.com/ethereum/portal-network-specs/pull/98\
    // Currently the way to handle data over uTP isn't finalized yet, so we are going to use the
    // handle data on connection closed method, as that seems to be the accepted method for now.
    pub async fn process_utp_byte_stream(&mut self) {
        let mut utp_connections = self.utp_connections.clone();
        for (conn_key, conn) in self.utp_connections.iter_mut() {
            if conn.state == SocketState::Closed {
                let received_stream = conn.recv_data_stream.clone();
                debug!(
                    "Received data: {received_stream:?}, len: {}",
                    received_stream.len()
                );

                match self.listening.get(&conn.receiver_connection_id) {
                    Some(message_type) => match message_type {
                        UtpMessageId::OfferAcceptStream => {
                            match UtpAccept::from_ssz_bytes(&received_stream[..]) {
                                Ok(payload) => {
                                    for (key, content) in payload.message {
                                        // TODO: Implement this with overlay store
                                        debug!("Store {key:?}, {content:?}");
                                    }
                                }
                                Err(_) => debug!("Recv malformed data on handing UtpAccept"),
                            }
                        }
                    },
                    _ => warn!("uTP listening HashMap doesn't have uTP stream message type"),
                }
                utp_connections.remove(conn_key);
            }
        }
    }
}

// Used to be MicroTransportProtocol impl but it is basically just called UtpStream compared to the
// Rust Tcp Lib so I changed it
#[derive(Clone)]
pub struct UtpSocket {
    /// The wrapped discv5 protocol
    socket: Arc<Discovery>,

    /// Socket state
    pub state: SocketState,

    // Remote peer
    connected_to: Enr,

    /// Sequence number for the next packet
    seq_nr: u16,

    /// Sequence number of the latest acknowledged packet sent by the remote peer
    ack_nr: u16,

    /// Sender connection identifier
    sender_connection_id: u16,

    /// Receiver connection identifier
    pub receiver_connection_id: u16,

    /// Congestion window in bytes
    cwnd: u32,

    /// Received but not acknowledged packets
    incoming_buffer: Vec<Packet>,

    /// Packets not yet sent
    unsent_queue: VecDeque<Packet>,

    /// Bytes in flight
    cur_window: u32,

    /// Window size of the remote peer
    remote_wnd_size: u32,

    /// Sent but not yet acknowledged packets
    send_window: Vec<Packet>,

    /// How many ACKs did the socket receive for packet with sequence number equal to `ack_nr`
    duplicate_ack_count: u8,

    /// Sequence number of the latest packet the remote peer acknowledged
    last_acked: u16,

    /// Timestamp of the latest packet the remote peer acknowledged
    last_acked_timestamp: Timestamp,

    /// Sequence number of the last packet removed from the incoming buffer
    last_dropped: u16,

    /// Round-trip time to remote peer
    rtt: i32,

    /// Variance of the round-trip time to the remote peer
    rtt_variance: i32,

    /// Data from the latest packet not yet returned in `recv_from`
    pending_data: Vec<u8>,

    /// Rolling window of packet delay to remote peer
    base_delays: VecDeque<Delay>,

    /// Rolling window of the difference between sending a packet and receiving its acknowledgement
    current_delays: Vec<DelayDifferenceSample>,

    /// Difference between timestamp of the latest packet received and time of reception
    their_delay: Delay,

    /// Current congestion timeout in milliseconds
    congestion_timeout: u64,

    /// Start of the current minute for sampling purposes
    last_rollover: Timestamp,

    /// Maximum retransmission retries
    pub max_retransmission_retries: u32,

    /// Receive channel for discv5 socket
    rx: Arc<RwLock<mpsc::UnboundedReceiver<Packet>>>,

    pub recv_data_stream: Vec<u8>,
}

impl UtpSocket {
    fn new(
        socket: Arc<Discovery>,
        connected_to: Enr,
        rx_rcv: Arc<RwLock<mpsc::UnboundedReceiver<Packet>>>,
    ) -> Self {
        let (receiver_id, sender_id) = generate_sequential_identifiers();

        Self {
            state: SocketState::Uninitialized,
            seq_nr: 1,
            ack_nr: 0,
            receiver_connection_id: receiver_id,
            sender_connection_id: sender_id,
            cwnd: INIT_CWND * MAX_DISCV5_PACKET_SIZE,
            incoming_buffer: Default::default(),
            unsent_queue: VecDeque::new(),
            connected_to,
            socket,
            cur_window: 0,
            remote_wnd_size: 0,
            send_window: Vec::new(),
            duplicate_ack_count: 0,
            last_acked: 0,
            last_acked_timestamp: Timestamp::default(),
            last_dropped: 0,
            rtt: 0,
            rtt_variance: 0,
            pending_data: Vec::new(),
            base_delays: VecDeque::with_capacity(BASE_HISTORY),
            their_delay: Delay::default(),
            congestion_timeout: 1000,
            last_rollover: Timestamp::default(),
            current_delays: Vec::with_capacity(8),
            recv_data_stream: Vec::new(),
            max_retransmission_retries: MAX_RETRANSMISSION_RETRIES,
            rx: rx_rcv,
        }
    }

    /// Sends data on the socket to the remote peer. On success, returns the number of bytes
    /// written.
    //
    // # Implementation details
    //
    // This method inserts packets into the send buffer and keeps trying to
    // advance the send window until an ACK corresponding to the last packet is
    // received.
    //
    // Note that the buffer passed to `send_to` might exceed the maximum packet
    // size, which will result in the data being split over several packets.
    pub async fn send_to(&mut self, buf: &[u8]) -> anyhow::Result<usize> {
        if self.state == SocketState::Closed {
            return Err(anyhow!("The socket is closed"));
        }

        let total_length = buf.len();

        for chunk in buf.chunks(
            MAX_DISCV5_PACKET_SIZE as usize
                - MAX_DISCV5_HEADER_SIZE
                - PAYLOAD_LENGTH_SIZE
                - HEADER_SIZE,
        ) {
            let mut packet = Packet::with_payload(chunk);
            packet.set_seq_nr(self.seq_nr);
            packet.set_ack_nr(self.ack_nr);
            packet.set_connection_id(self.sender_connection_id);

            self.unsent_queue.push_back(packet);

            // Intentionally wrap around sequence number
            self.seq_nr = self.seq_nr.wrapping_add(1);
        }

        // Send every packet in the queue
        self.send_packets_in_queue().await;

        Ok(total_length)
    }

    pub async fn raw_receive(&mut self) -> anyhow::Result<Option<Packet>> {
        // Listen on a channel for discovery utp packet
        match timeout(Duration::from_millis(15), self.rx.write().await.recv()).await {
            Ok(val) => Ok(val),
            Err(msg) => Err(anyhow!("Discv5 socket timeout: {msg}")),
        }
    }

    async fn send_packets_in_queue(&mut self) {
        while let Some(mut packet) = self.unsent_queue.pop_front() {
            self.send_packet(&mut packet).await;
            self.cur_window += packet.len() as u32;
            self.send_window.push(packet);
        }
    }

    #[async_recursion]
    async fn resend_lost_packet(&mut self, lost_packet_nr: u16) {
        debug!("---> resend_lost_packet({}) <---", lost_packet_nr);
        match self
            .send_window
            .iter()
            .position(|pkt| pkt.seq_nr() == lost_packet_nr)
        {
            None => debug!("Packet {} not found", lost_packet_nr),
            Some(position) => {
                debug!("Send window len: {}", self.send_window.len());
                debug!("position: {}", position);
                let mut packet = self.send_window[position].clone();
                self.send_packet(&mut packet).await;

                // We intentionally don't increase `curr_window` because otherwise a packet's length
                // would be counted more than once
            }
        }
        debug!("---> END resend_lost_packet <---");
    }

    /// Send one packet.
    #[async_recursion]
    async fn send_packet(&mut self, packet: &mut Packet) {
        debug!("current window: {}", self.send_window.len());
        let max_inflight = min(self.cwnd, self.remote_wnd_size);
        let max_inflight = max(MIN_CWND * MAX_DISCV5_PACKET_SIZE, max_inflight);
        let now = now_microseconds();

        // Wait until enough in-flight packets are acknowledged for rate control purposes, but don't
        // wait more than 500 ms (PRE_SEND_TIMEOUT) before sending the packet
        while self.cur_window + packet.as_ref().len() as u32 > max_inflight as u32
            && now_microseconds() - now < PRE_SEND_TIMEOUT.into()
        {
            debug!("curr_window: {}", self.cur_window);
            debug!("max_inflight: {}", max_inflight);
            debug!("duplicate_ack_count: {}", self.duplicate_ack_count);
            debug!("now_microseconds() - now = {}", now_microseconds() - now);
            let mut buf = [0; BUF_SIZE];
            if let Err(msg) = self.recv(&mut buf).await {
                debug!("Unable to receive from uTP socket: {msg}");
            }
        }

        debug!(
            "out: now_microseconds() - now = {}",
            now_microseconds() - now
        );

        // Check if it still makes sense to send packet, as we might be trying to resend a lost
        // packet acknowledged in the receive loop above.
        // If there were no wrapping around of sequence numbers, we'd simply check if the packet's
        // sequence number is greater than `last_acked`.
        let distance_a = packet.seq_nr().wrapping_sub(self.last_acked);
        let distance_b = self.last_acked.wrapping_sub(packet.seq_nr());
        if distance_a > distance_b {
            debug!("Packet already acknowledged, skipping...");
            return;
        }

        let enr = self.connected_to.clone();
        let discovery = self.socket.clone();

        packet.set_timestamp(now_microseconds());
        packet.set_timestamp_difference(self.their_delay);

        let packet_to_send = packet.clone();

        // Handle talkreq/talkresp in the background
        tokio::spawn(async move {
            if let Err(response) = discovery
                .send_talk_req(enr, ProtocolId::Utp, Vec::from(packet_to_send.as_ref()))
                .await
            {
                debug!("Unable to send utp talk req: {response}")
            }
        });
        debug!("sent {:?}", packet);
    }

    // Insert a new sample in the base delay list.
    //
    // The base delay list contains at most `BASE_HISTORY` samples, each sample is the minimum
    // measured over a period of a minute (MAX_BASE_DELAY_AGE).
    fn update_base_delay(&mut self, base_delay: Delay, now: Timestamp) {
        if self.base_delays.is_empty() || now - self.last_rollover > MAX_BASE_DELAY_AGE {
            // Update last rollover
            self.last_rollover = now;

            // Drop the oldest sample, if need be
            if self.base_delays.len() == BASE_HISTORY {
                self.base_delays.pop_front();
            }

            // Insert new sample
            self.base_delays.push_back(base_delay);
        } else {
            // Replace sample for the current minute if the delay is lower
            let last_idx = self.base_delays.len() - 1;
            if base_delay < self.base_delays[last_idx] {
                self.base_delays[last_idx] = base_delay;
            }
        }
    }

    /// Inserts a new sample in the current delay list after removing samples older than one RTT, as
    /// specified in RFC6817.
    fn update_current_delay(&mut self, our_delay: Delay, now: Timestamp) {
        // Remove samples more than one RTT old
        let rtt = (self.rtt as i64 * 100).into();
        while !self.current_delays.is_empty() && now - self.current_delays[0].received_at > rtt {
            self.current_delays.remove(0);
        }

        // Insert new measurement
        self.current_delays.push(DelayDifferenceSample {
            received_at: now,
            difference: our_delay,
        });
    }

    fn filter(current_delay: &[u32]) -> u32 {
        let filt = (current_delay.len() as f64 / 3_f64).ceil() as usize;
        *current_delay[current_delay.len() - filt..]
            .iter()
            .min()
            .unwrap()
    }

    async fn make_connection(&mut self, connection_id: u16) {
        if self.state == SocketState::Uninitialized {
            self.receiver_connection_id = connection_id;
            self.sender_connection_id = self.receiver_connection_id + 1;

            let mut packet = Packet::new();
            packet.set_type(PacketType::Syn);
            packet.set_connection_id(self.receiver_connection_id);
            packet.set_seq_nr(self.seq_nr);

            self.send_packet(&mut packet).await;
            self.state = SocketState::SynSent;
        }
    }

    /// Builds the selective acknowledgement extension data for usage in packets.
    fn build_selective_ack(&self) -> Vec<u8> {
        let stashed = self
            .incoming_buffer
            .iter()
            .filter(|pkt| pkt.seq_nr() > self.ack_nr + 1)
            .map(|pkt| (pkt.seq_nr() - self.ack_nr - 2) as usize)
            .map(|diff| (diff / 8, diff % 8));

        let mut sack = Vec::new();
        for (byte, bit) in stashed {
            // Make sure the amount of elements in the SACK vector is a
            // multiple of 4 and enough to represent the lost packets
            while byte >= sack.len() || sack.len() % 4 != 0 {
                sack.push(0u8);
            }

            sack[byte] |= 1 << bit;
        }

        sack
    }

    pub async fn send_finalize(&mut self) {
        let mut packet = Packet::new();
        packet.set_type(PacketType::Fin);
        packet.set_connection_id(self.sender_connection_id);
        packet.set_seq_nr(self.seq_nr);
        packet.set_ack_nr(self.ack_nr);

        self.send_packet(&mut packet).await;
        self.state = SocketState::FinSent;
    }

    #[async_recursion]
    async fn handle_packet(&mut self, packet: &Packet, src: Enr) -> anyhow::Result<Option<Packet>> {
        debug!(
            "Handle packet: {:?}. Conn state: {:?}",
            packet.get_type(),
            self.state
        );

        // Only acknowledge this if this follows the last one, else do it when we advance the send
        // window
        if packet.seq_nr().wrapping_sub(self.ack_nr) == 1 {
            self.ack_nr = packet.seq_nr();
        }

        // Reset connection if connection id doesn't match and this isn't a SYN
        if packet.get_type() != PacketType::Syn
            && self.state != SocketState::SynSent
            && !(packet.connection_id() == self.sender_connection_id
                || packet.connection_id() == self.receiver_connection_id)
        {
            return Ok(Some(self.prepare_reply(packet, PacketType::Reset)));
        }

        // Update remote window size
        self.remote_wnd_size = packet.wnd_size();
        debug!("Remote window size: {}", self.remote_wnd_size);

        // Update remote peer's delay between them sending the packet and us receiving it
        let now = now_microseconds();
        self.their_delay = abs_diff(now, packet.timestamp());
        debug!("self.their_delay: {}", self.their_delay);

        match (self.state, packet.get_type()) {
            // New connection, when we receive SYN packet, respond with STATE packet
            (SocketState::Uninitialized, PacketType::Syn) => {
                self.connected_to = src;
                self.ack_nr = packet.seq_nr();
                self.seq_nr = rand::random();
                self.receiver_connection_id = packet.connection_id() + 1;
                self.sender_connection_id = packet.connection_id();
                self.state = SocketState::Connected;
                self.last_dropped = self.ack_nr;

                Ok(Some(self.prepare_reply(packet, PacketType::State)))
            }
            // When connection is already initialised and we receive SYN packet,
            // we want to forcibly terminate the connection
            (_, PacketType::Syn) => Ok(Some(self.prepare_reply(packet, PacketType::Reset))),
            // When SYN is send and we receive STATE, do not reply
            (SocketState::SynSent, PacketType::State) => {
                self.connected_to = src;
                self.ack_nr = packet.seq_nr();
                self.seq_nr += 1;
                self.state = SocketState::Connected;
                self.last_acked = packet.ack_nr();
                self.last_acked_timestamp = now_microseconds();
                Ok(None)
            }
            // Only STATE packet is expected when SYN is sent
            (SocketState::SynSent, _) => Err(anyhow!("The remote peer sent an invalid reply")),
            // Handle data packet if socket state is `Connected` or `FinSent` and packet type is DATA
            (SocketState::Connected, PacketType::Data)
            | (SocketState::FinSent, PacketType::Data) => Ok(self.handle_data_packet(packet)),
            // Handle state packet if socket state is `Connected` and packet type is STATE
            (SocketState::Connected, PacketType::State) => {
                self.handle_state_packet(packet).await;
                Ok(None)
            }
            // Handle FIN packet. Check if all send packets are acknowledged.
            (SocketState::Connected, PacketType::Fin) | (SocketState::FinSent, PacketType::Fin) => {
                if packet.ack_nr() < self.seq_nr {
                    debug!("FIN received but there are missing acknowledgements for sent packets");
                }
                let mut reply = self.prepare_reply(packet, PacketType::State);

                if packet.seq_nr().wrapping_sub(self.ack_nr) > 1 {
                    debug!(
                        "current ack_nr ({}) is behind received packet seq_nr ({})",
                        self.ack_nr,
                        packet.seq_nr()
                    );

                    // Set SACK extension payload if the packet is not in order
                    let sack = self.build_selective_ack();

                    if !sack.is_empty() {
                        reply.set_selective_ack(sack);
                    }
                }

                // Give up, the remote peer might not care about our missing packets
                self.state = SocketState::Closed;
                Ok(Some(reply))
            }
            // Confirm with STATE packet when socket state is `Closed` and we receive FIN packet
            (SocketState::Closed, PacketType::Fin) => {
                Ok(Some(self.prepare_reply(packet, PacketType::State)))
            }
            (SocketState::FinSent, PacketType::State) => {
                if packet.ack_nr() == self.seq_nr {
                    self.state = SocketState::Closed;
                } else {
                    self.handle_state_packet(packet).await;
                }
                Ok(None)
            }
            // Reset connection when receiving RESET packet
            (_, PacketType::Reset) => {
                self.state = SocketState::ResetReceived;
                Err(anyhow!("Connection reset by remote peer"))
            }
            (state, ty) => {
                let message = format!("Unimplemented handling for ({state:?},{ty:?})");
                debug!("{}", message);
                Err(anyhow!(message))
            }
        }
    }

    fn prepare_reply(&self, original: &Packet, t: PacketType) -> Packet {
        let mut resp = Packet::new();
        resp.set_type(t);
        let self_t_micro = now_microseconds();
        let other_t_micro = original.timestamp();
        let time_difference: Delay = abs_diff(self_t_micro, other_t_micro);
        resp.set_timestamp(self_t_micro);
        resp.set_timestamp_difference(time_difference);
        resp.set_connection_id(self.sender_connection_id);
        resp.set_seq_nr(self.seq_nr);
        resp.set_ack_nr(self.ack_nr);

        resp
    }

    fn handle_data_packet(&mut self, packet: &Packet) -> Option<Packet> {
        // If a FIN was previously sent, reply with a FIN packet acknowledging the received packet.
        let packet_type = match self.state {
            SocketState::FinSent => PacketType::Fin,
            _ => PacketType::State,
        };

        let mut reply = self.prepare_reply(packet, packet_type);

        if packet.seq_nr().wrapping_sub(self.ack_nr) > 1 {
            debug!(
                "current ack_nr ({}) is behind received packet seq_nr ({})",
                self.ack_nr,
                packet.seq_nr()
            );

            // Set SACK extension payload if the packet is not in order
            let sack_bitfield = self.build_selective_ack();

            if !sack_bitfield.is_empty() {
                reply.set_selective_ack(sack_bitfield);
            }
        }
        Some(reply)
    }

    #[async_recursion]
    async fn handle_state_packet(&mut self, packet: &Packet) {
        if self.last_acked == packet.ack_nr() {
            self.duplicate_ack_count += 1;
        } else {
            self.last_acked = packet.ack_nr();
            self.last_acked_timestamp = now_microseconds();
            self.duplicate_ack_count = 1;
        }

        // Update congestion window size
        if let Some(index) = self
            .send_window
            .iter()
            .position(|p| packet.ack_nr() == p.seq_nr())
        {
            // Calculate the sum of the size of every packet implicitly and explicitly acknowledged
            // by the inbound packet (i.e., every packet whose sequence number precedes the inbound
            // packet's acknowledgement number, plus the packet whose sequence number matches)
            let bytes_newly_acked = self
                .send_window
                .iter()
                .take(index + 1)
                .fold(0, |acc, p| acc + p.len());

            // Update base and current delay
            let now = now_microseconds();
            let our_delay = now - self.send_window[index].timestamp();
            debug!("our_delay: {}", our_delay);
            self.update_base_delay(our_delay, now);
            self.update_current_delay(our_delay, now);

            let off_target: f64 =
                (CCONTROL_TARGET - u32::from(self.queuing_delay()) as f64) / CCONTROL_TARGET;
            debug!("off_target: {}", off_target);

            self.update_congestion_window(off_target, bytes_newly_acked as u32);

            // Update congestion timeout
            let rtt = u32::from(our_delay - self.queuing_delay()) / 1000; // in milliseconds
            self.update_congestion_timeout(rtt as i32);
        }

        let mut packet_loss_detected: bool =
            !self.send_window.is_empty() && self.duplicate_ack_count == 3;

        // Process extensions, if any
        for extension in packet.extensions() {
            if extension.get_type() == ExtensionType::SelectiveAck {
                // If three or more packets are acknowledged past the implicit missing one,
                // assume it was lost.
                if extension.iter().count_ones() >= 3 {
                    self.resend_lost_packet(packet.ack_nr() + 1).await;
                    packet_loss_detected = true;
                }

                if let Some(last_seq_nr) = self.send_window.last().map(Packet::seq_nr) {
                    let lost_packets = extension
                        .iter()
                        .enumerate()
                        .filter(|&(_, received)| !received)
                        .map(|(idx, _)| packet.ack_nr() + 2 + idx as u16)
                        .take_while(|&seq_nr| seq_nr < last_seq_nr);

                    for seq_nr in lost_packets {
                        debug!("SACK: packet {} lost", seq_nr);
                        self.resend_lost_packet(seq_nr).await;
                        packet_loss_detected = true;
                    }
                }
            } else {
                debug!("Unknown extension {:?}, ignoring", extension.get_type());
            }
        }

        // Three duplicate ACKs mean a fast resend request. Resend the first unacknowledged packet
        // if the incoming packet doesn't have a SACK extension. If it does, the lost packets were
        // already resent.
        if !self.send_window.is_empty()
            && self.duplicate_ack_count == 3
            && !packet
                .extensions()
                .any(|ext| ext.get_type() == ExtensionType::SelectiveAck)
        {
            self.resend_lost_packet(packet.ack_nr() + 1).await;
        }

        // Packet lost, halve the congestion window
        if packet_loss_detected {
            debug!("packet loss detected, halving congestion window");
            self.cwnd = max(self.cwnd / 2, MIN_CWND * MAX_DISCV5_PACKET_SIZE);
            debug!("congestion window: {}", self.cwnd);
        }

        // Success, advance send window
        self.advance_send_window();
    }

    /// Forgets sent packets that were acknowledged by the remote peer.
    fn advance_send_window(&mut self) {
        // The reason we are not removing the first element in a loop while its sequence number is
        // smaller than `last_acked` is because of wrapping sequence numbers, which would create the
        // sequence [..., 65534, 65535, 0, 1, ...]. If `last_acked` is smaller than the first
        // packet's sequence number because of wraparound (for instance, 1), no packets would be
        // removed, as the condition `seq_nr < last_acked` would fail immediately.
        //
        // On the other hand, we can't keep removing the first packet in a loop until its sequence
        // number matches `last_acked` because it might never match, and in that case no packets
        // should be removed.
        if let Some(position) = self
            .send_window
            .iter()
            .position(|packet| packet.seq_nr() == self.last_acked)
        {
            for _ in 0..position + 1 {
                let packet = self.send_window.remove(0);
                self.cur_window -= packet.len() as u32;
            }
        }
        debug!("Bytes in flight: {}", self.cur_window);
    }

    fn queuing_delay(&self) -> Delay {
        let filtered_current_delay = self.filtered_current_delay();
        let min_base_delay = self.min_base_delay();
        let queuing_delay = filtered_current_delay - min_base_delay;

        debug!("filtered_current_delay: {}", filtered_current_delay);
        debug!("min_base_delay: {}", min_base_delay);
        debug!("queuing_delay: {}", queuing_delay);

        queuing_delay
    }

    /// Calculates the filtered current delay in the current window.
    ///
    /// The current delay is calculated through application of the exponential
    /// weighted moving average filter with smoothing factor 0.333 over the
    /// current delays in the current window.
    fn filtered_current_delay(&self) -> Delay {
        let input = self.current_delays.iter().map(|delay| &delay.difference);
        (ewma(input, 0.333) as i64).into()
    }

    /// Calculates the lowest base delay in the current window.
    fn min_base_delay(&self) -> Delay {
        self.base_delays.iter().min().cloned().unwrap_or_default()
    }

    /// Calculates the new congestion window size, increasing it or decreasing it.
    ///
    /// This is the core of uTP, the [LEDBAT][ledbat_rfc] congestion algorithm. It depends on
    /// estimating the queuing delay between the two peers, and adjusting the congestion window
    /// accordingly.
    ///
    /// `off_target` is a normalized value representing the difference between the current queuing
    /// delay and a fixed target delay (`CCONTROL_TARGET`). `off_target` ranges between -1.0 and 1.0. A
    /// positive value makes the congestion window increase, while a negative value makes the
    /// congestion window decrease.
    ///
    /// `bytes_newly_acked` is the number of bytes acknowledged by an inbound `State` packet. It may
    /// be the size of the packet explicitly acknowledged by the inbound packet (i.e., with sequence
    /// number equal to the inbound packet's acknowledgement number), or every packet implicitly
    /// acknowledged (every packet with sequence number between the previous inbound `State`
    /// packet's acknowledgement number and the current inbound `State` packet's acknowledgement
    /// number).
    ///
    ///[ledbat_rfc]: https://tools.ietf.org/html/rfc6817
    fn update_congestion_window(&mut self, off_target: f64, bytes_newly_acked: u32) {
        let flightsize = self.cur_window;

        let cwnd_increase =
            GAIN * off_target * bytes_newly_acked as f64 * MAX_DISCV5_PACKET_SIZE as f64;
        let cwnd_increase = cwnd_increase / self.cwnd as f64;
        debug!("cwnd_increase: {}", cwnd_increase);

        self.cwnd = (self.cwnd as f64 + cwnd_increase) as u32;
        let max_allowed_cwnd = flightsize + ALLOWED_INCREASE * MAX_DISCV5_PACKET_SIZE;
        self.cwnd = min(self.cwnd, max_allowed_cwnd);
        self.cwnd = max(self.cwnd, MIN_CWND * MAX_DISCV5_PACKET_SIZE);

        debug!("cwnd: {}", self.cwnd);
        debug!("max_allowed_cwnd: {}", max_allowed_cwnd);
    }

    fn update_congestion_timeout(&mut self, current_delay: i32) {
        let delta = self.rtt - current_delay;
        self.rtt_variance += (delta.abs() - self.rtt_variance) / 4;
        self.rtt += (current_delay - self.rtt) / 8;
        self.congestion_timeout = max(
            (self.rtt + self.rtt_variance * 4) as u64,
            MIN_CONGESTION_TIMEOUT,
        );
        self.congestion_timeout = min(self.congestion_timeout, MAX_CONGESTION_TIMEOUT);

        debug!("current_delay: {}", current_delay);
        debug!("delta: {}", delta);
        debug!("self.rtt_variance: {}", self.rtt_variance);
        debug!("self.rtt: {}", self.rtt);
        debug!("self.congestion_timeout: {}", self.congestion_timeout);
    }

    #[async_recursion]
    pub async fn recv(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let packet;

        // TODO: Refactor this and abort loop if the current try exceeds the maximum number of retransmission retries.
        // Try to receive a packet
        loop {
            let result = self.raw_receive().await;
            match result {
                Ok(pkt) => {
                    match pkt {
                        Some(pkt) => packet = pkt,
                        None => {
                            debug!("Received packet is NONE");
                            continue;
                        }
                    }
                    break;
                }
                Err(msg) => debug!("{msg}"),
            }
        }

        debug!("received {:?}", packet);

        // Process packet, including sending a reply if necessary
        if let Some(mut pkt) = self
            .handle_packet(&packet, self.connected_to.clone())
            .await?
        {
            pkt.set_wnd_size(WINDOW_SIZE);
            self.socket
                .send_talk_req(
                    self.connected_to.clone(),
                    ProtocolId::Utp,
                    Vec::from(pkt.as_ref()),
                )
                .await
                .unwrap();
            debug!("sent {:?}", pkt);
        }

        // Insert data packet into the incoming buffer if it isn't a duplicate of a previously
        // discarded packet
        if packet.get_type() == PacketType::Data
            && packet.seq_nr().wrapping_sub(self.last_dropped) > 0
        {
            self.insert_into_buffer(packet);
        }
        // Flush incoming buffer if possible
        let read = self.flush_incoming_buffer(buf);

        Ok(read)
    }

    /// Discards sequential, ordered packets in incoming buffer, starting from
    /// the most recently acknowledged to the most recent, as long as there are
    /// no missing packets. The discarded packets' payload is written to the
    /// slice `buf`, starting in position `start`.
    /// Returns the last written index.
    fn flush_incoming_buffer(&mut self, buf: &mut [u8]) -> usize {
        fn unsafe_copy(src: &[u8], dst: &mut [u8]) -> usize {
            let max_len = min(src.len(), dst.len());
            // Unsafe is needed because `copy` is unsafe function
            unsafe {
                use std::ptr::copy;
                copy(src.as_ptr(), dst.as_mut_ptr(), max_len);
            }
            max_len
        }

        // Return pending data from a partially read packet
        if !self.pending_data.is_empty() {
            let flushed = unsafe_copy(&self.pending_data[..], buf);

            if flushed == self.pending_data.len() {
                self.pending_data.clear();
                self.advance_incoming_buffer();
            } else {
                self.pending_data = self.pending_data[flushed..].to_vec();
            }

            return flushed;
        }

        if !self.incoming_buffer.is_empty()
            && (self.ack_nr == self.incoming_buffer[0].seq_nr()
                || self.ack_nr + 1 == self.incoming_buffer[0].seq_nr())
        {
            let flushed = unsafe_copy(self.incoming_buffer[0].payload(), buf);

            if flushed == self.incoming_buffer[0].payload().len() {
                self.advance_incoming_buffer();
            } else {
                self.pending_data = self.incoming_buffer[0].payload()[flushed..].to_vec();
            }

            return flushed;
        }

        0
    }

    /// Removes a packet in the incoming buffer and updates the current acknowledgement number.
    fn advance_incoming_buffer(&mut self) -> Option<Packet> {
        if !self.incoming_buffer.is_empty() {
            let packet = self.incoming_buffer.remove(0);
            debug!("Removed packet from incoming buffer: {:?}", packet);
            self.ack_nr = packet.seq_nr();
            self.last_dropped = self.ack_nr;
            Some(packet)
        } else {
            None
        }
    }

    /// Inserts a packet into the socket's buffer.
    ///
    /// The packet is inserted in such a way that the packets in the buffer are sorted according to
    /// their sequence number in ascending order. This allows storing packets that were received out
    /// of order.
    ///
    /// Trying to insert a duplicate of a packet will silently fail.
    /// it's more recent (larger timestamp).
    fn insert_into_buffer(&mut self, packet: Packet) {
        // Immediately push to the end if the packet's sequence number comes after the last
        // packet's.
        if self
            .incoming_buffer
            .last()
            .map_or(false, |p| packet.seq_nr() > p.seq_nr())
        {
            self.incoming_buffer.push(packet);
        } else {
            // Find index following the most recent packet before the one we wish to insert
            let i = self
                .incoming_buffer
                .iter()
                .filter(|p| p.seq_nr() < packet.seq_nr())
                .count();

            if self
                .incoming_buffer
                .get(i)
                .map_or(true, |p| p.seq_nr() != packet.seq_nr())
            {
                self.incoming_buffer.insert(i, packet);
            }
        }
    }

    /// Gracefully closes connection to peer.
    ///
    /// This method allows both peers to receive all packets still in
    /// flight.
    pub async fn close(&mut self) -> anyhow::Result<()> {
        // Nothing to do if the socket's already closed or not connected
        if self.state == SocketState::Closed
            || self.state == SocketState::Uninitialized
            || self.state == SocketState::SynSent
        {
            return Ok(());
        }

        // Flush unsent and unacknowledged packets
        self.flush().await?;

        let mut packet = Packet::new();
        packet.set_connection_id(self.sender_connection_id);
        packet.set_seq_nr(self.seq_nr);
        packet.set_ack_nr(self.ack_nr);
        packet.set_timestamp(now_microseconds());
        packet.set_type(PacketType::Fin);

        // Send FIN
        if let Err(msg) = self
            .socket
            .send_talk_req(
                self.connected_to.clone(),
                ProtocolId::Utp,
                Vec::from(packet.as_ref()),
            )
            .await
        {
            let msg = format!("Unavle to send FIN packet: {msg}");
            debug!("{msg}");
            return Err(anyhow!(msg));
        }

        debug!("CLosing connection, sent {:?}", packet);
        self.state = SocketState::FinSent;

        // Receive JAKE
        let mut buf = [0; BUF_SIZE];
        while self.state != SocketState::Closed {
            self.recv(&mut buf).await?;
        }

        Ok(())
    }

    /// Consumes acknowledgements for every pending packet.
    pub async fn flush(&mut self) -> anyhow::Result<()> {
        let mut buf = [0u8; BUF_SIZE];
        while !self.send_window.is_empty() {
            debug!("packets in send window: {}", self.send_window.len());
            self.recv(&mut buf).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::portalnet::discovery::Discovery;
    use crate::portalnet::types::messages::{PortalnetConfig, ProtocolId};
    use crate::portalnet::Enr;
    use crate::utils::node_id::generate_random_remote_enr;
    use crate::utp::packets::{Packet, PacketType};
    use crate::utp::stream::{SocketState, UtpSocket, BUF_SIZE};
    use discv5::Discv5Event;
    use std::convert::TryFrom;
    use std::str::FromStr;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio::sync::RwLock;

    fn next_test_port() -> u16 {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT_OFFSET: AtomicUsize = AtomicUsize::new(0);
        const BASE_PORT: u16 = 9600;
        BASE_PORT + NEXT_OFFSET.fetch_add(1, Ordering::Relaxed) as u16
    }

    fn create_portal_config() -> PortalnetConfig {
        PortalnetConfig {
            listen_port: next_test_port(),
            internal_ip: true,
            ..Default::default()
        }
    }

    async fn server_setup() -> UtpSocket {
        let config = create_portal_config();
        let mut discv5 = Discovery::new(config).unwrap();
        let enr = discv5.discv5.local_enr();
        discv5.start().await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel::<Packet>();

        let discv5_arc = Arc::new(discv5);
        let discv5_arc_clone = Arc::clone(&discv5_arc);

        // TODO: Create `Discv5Socket` struct to encapsulate all socket logic
        spawn_socket_recv(tx, discv5_arc_clone);

        UtpSocket::new(discv5_arc, enr, Arc::new(RwLock::new(rx)))
    }

    async fn client_setup(connected_to: Enr) -> (Enr, UtpSocket) {
        let config = create_portal_config();
        let mut discv5 = Discovery::new(config).unwrap();
        discv5.start().await.unwrap();

        let (tx, rx) = mpsc::unbounded_channel::<Packet>();

        let discv5_arc = Arc::new(discv5);
        let discv5_arc_clone = Arc::clone(&discv5_arc);

        spawn_socket_recv(tx, discv5_arc_clone);

        (
            discv5_arc.local_enr(),
            UtpSocket::new(discv5_arc, connected_to, Arc::new(RwLock::new(rx))),
        )
    }

    fn spawn_socket_recv(tx: UnboundedSender<Packet>, discv5_arc_clone: Arc<Discovery>) {
        tokio::spawn(async move {
            let mut receiver = discv5_arc_clone.discv5.event_stream().await.unwrap();
            while let Some(event) = receiver.recv().await {
                match event {
                    Discv5Event::TalkRequest(request) => {
                        let protocol_id =
                            ProtocolId::from_str(&hex::encode_upper(request.protocol())).unwrap();

                        match protocol_id {
                            ProtocolId::Utp => {
                                let payload = request.body();
                                let packet = Packet::try_from(payload).unwrap();
                                tx.send(packet).unwrap();
                            }
                            _ => {
                                panic!(
                                    "Received TalkRequest on unknown protocol from={} protocol={} body={}",
                                    request.node_id(),
                                    hex::encode_upper(request.protocol()),
                                    hex::encode(request.body()),
                                );
                            }
                        }
                    }
                    _ => continue,
                }
            }
        });
    }

    #[tokio::test]
    async fn test_handle_packet() {
        // Boilerplate test setup
        let initial_connection_id: u16 = rand::random();
        let sender_connection_id = initial_connection_id + 1;
        let (_, client_enr) = generate_random_remote_enr();
        let mut socket = server_setup().await;

        // ---------------------------------
        // Test connection setup - SYN packet

        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        // Do we have a response?
        let response = socket.handle_packet(&packet, client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        // Is it of the correct type?
        let response = response.unwrap();
        assert_eq!(response.get_type(), PacketType::State);

        // Same connection id on both ends during connection establishment
        assert_eq!(response.connection_id(), packet.connection_id());

        // Response acknowledges SYN
        assert_eq!(response.ack_nr(), packet.seq_nr());

        // Expect no payloadd
        assert!(response.payload().is_empty());

        // ---------------------------------
        // Test connection usage - transmitting DATA packet

        let old_packet = packet;
        let old_response = response;

        let mut packet = Packet::new();
        packet.set_type(PacketType::Data);
        packet.set_connection_id(sender_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());

        let response = socket.handle_packet(&packet, client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        let response = response.unwrap();
        assert_eq!(response.get_type(), PacketType::State);

        // Sender (i.e., who the initiated connection and sent a SYN) has connection id equal to
        // initial connection id + 1
        // Receiver (i.e., who accepted connection) has connection id equal to initial connection id
        assert_eq!(response.connection_id(), initial_connection_id);
        assert_eq!(response.connection_id(), packet.connection_id() - 1);

        // Previous packets should be ack'ed
        assert_eq!(response.ack_nr(), packet.seq_nr());

        // Responses with no payload should not increase the sequence number
        assert!(response.payload().is_empty());
        assert_eq!(response.seq_nr(), old_response.seq_nr());

        // ---------------------------------
        // Test connection teardown - FIN packet

        let old_packet = packet;
        let old_response = response;

        let mut packet = Packet::new();
        packet.set_type(PacketType::Fin);
        packet.set_connection_id(sender_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());

        let response = socket.handle_packet(&packet, client_enr).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        let response = response.unwrap();

        assert_eq!(response.get_type(), PacketType::State);

        // FIN packets have no payload but the sequence number shouldn't increase
        assert_eq!(packet.seq_nr(), old_packet.seq_nr() + 1);

        // Nor should the ACK packet's sequence number
        assert_eq!(response.seq_nr(), old_response.seq_nr());

        // FIN should be acknowledged
        assert_eq!(response.ack_nr(), packet.seq_nr());
    }

    #[tokio::test]
    async fn test_response_to_keepalive_ack() {
        // Boilerplate test setup
        let initial_connection_id: u16 = rand::random();
        let (_, client_enr) = generate_random_remote_enr();
        let mut socket = server_setup().await;

        // Establish connection
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        let response = socket.handle_packet(&packet, client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.get_type(), PacketType::State);

        let old_packet = packet;
        let old_response = response;

        // Now, send a keepalive packet
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::State);
        packet.set_connection_id(initial_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());

        let response = socket.handle_packet(&packet, client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_none());

        // Send a second keepalive packet, identical to the previous one
        let response = socket.handle_packet(&packet, client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_none());

        // Mark socket as closed
        socket.state = SocketState::Closed;
    }

    #[tokio::test]
    async fn test_response_to_wrong_connection_id() {
        // Boilerplate test setup
        let initial_connection_id: u16 = rand::random();
        let (_, client_enr) = generate_random_remote_enr();
        let mut socket = server_setup().await;

        // Establish connection
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        let response = socket.handle_packet(&packet, client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        assert_eq!(response.unwrap().get_type(), PacketType::State);

        // Now, disrupt connection with a packet with an incorrect connection id
        let new_connection_id = initial_connection_id.wrapping_mul(2);

        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::State);
        packet.set_connection_id(new_connection_id);

        let response = socket.handle_packet(&packet, client_enr).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        let response = response.unwrap();
        assert_eq!(response.get_type(), PacketType::Reset);
        assert_eq!(response.ack_nr(), packet.seq_nr());

        // Mark socket as closed
        socket.state = SocketState::Closed;
    }

    #[tokio::test]
    async fn test_unordered_packets() {
        // Boilerplate test setup
        let initial_connection_id: u16 = rand::random();
        let (_, client_enr) = generate_random_remote_enr();
        let mut socket = server_setup().await;

        // Establish connection
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        let response = socket.handle_packet(&packet, client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        let response = response.unwrap();
        assert_eq!(response.get_type(), PacketType::State);

        let old_packet = packet;
        let old_response = response;

        let mut window: Vec<Packet> = Vec::new();

        // Now, send a keepalive packet
        let mut packet = Packet::with_payload(&[1, 2, 3]);
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_connection_id(initial_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());
        window.push(packet);

        let mut packet = Packet::with_payload(&[4, 5, 6]);
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_connection_id(initial_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 2);
        packet.set_ack_nr(old_response.seq_nr());
        window.push(packet);

        // Send packets in reverse order
        let response = socket.handle_packet(&window[1], client_enr.clone()).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        let response = response.unwrap();
        assert!(response.ack_nr() != window[1].seq_nr());

        let response = socket.handle_packet(&window[0], client_enr).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        // Mark socket as closed
        socket.state = SocketState::Closed;
    }

    #[tokio::test]
    async fn test_base_delay_calculation() {
        let minute_in_microseconds = 60 * 10i64.pow(6);
        let samples = vec![
            (0, 10),
            (1, 8),
            (2, 12),
            (3, 7),
            (minute_in_microseconds + 1, 11),
            (minute_in_microseconds + 2, 19),
            (minute_in_microseconds + 3, 9),
        ];
        let mut socket = server_setup().await;

        for (timestamp, delay) in samples {
            socket.update_base_delay(delay.into(), ((timestamp + delay) as u32).into());
        }

        let expected = vec![7i64, 9i64]
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let actual = socket.base_delays.iter().cloned().collect::<Vec<_>>();
        assert_eq!(expected, actual);
        assert_eq!(
            socket.min_base_delay(),
            expected.iter().min().cloned().unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn test_response_to_triple_ack() {
        let mut buf = [0; BUF_SIZE];
        let mut server = server_setup().await;

        // Fits in a packet
        const LEN: usize = 50;
        let data = (0..LEN).map(|idx| idx as u8).collect::<Vec<u8>>();
        let data_clone = data.clone();
        assert_eq!(LEN, data.len());

        let (enr, mut client) = client_setup(server.connected_to.clone()).await;

        client.make_connection(12).await;

        // Expect SYN packet
        server.connected_to = enr;
        server.recv(&mut buf).await.unwrap();

        // Expect STATE packet
        client.recv(&mut buf).await.unwrap();

        // Send DATA packet
        client.send_to(&data_clone[..]).await.unwrap();

        // Receive data
        let data_packet = server.raw_receive().await.unwrap().unwrap();

        assert_eq!(data_packet.get_type(), PacketType::Data);
        assert_eq!(&data_packet.payload(), &data.as_slice());
        assert_eq!(data_packet.payload().len(), data.len());

        // Send triple ACK
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::State);
        packet.set_seq_nr(server.seq_nr);
        packet.set_ack_nr(data_packet.seq_nr() - 1);
        packet.set_connection_id(server.sender_connection_id);

        for _ in 0..3 {
            server
                .socket
                .discv5
                .talk_req(
                    server.connected_to.clone(),
                    Vec::try_from(ProtocolId::Utp).unwrap(),
                    packet.as_ref().to_vec(),
                )
                .await
                .unwrap();
        }

        // TODO: implement `recv_from` to handle all packets
        for _ in 0..3 {
            let mut buf = [0; BUF_SIZE];
            client.recv(&mut buf).await.unwrap();
        }

        // Receive data again and check that it's the same we reported as missing
        let client_addr = server.connected_to.clone();
        match server.raw_receive().await {
            Ok(packet) => {
                let packet = packet.unwrap();
                assert_eq!(packet.get_type(), PacketType::Data);
                assert_eq!(packet.seq_nr(), data_packet.seq_nr());
                assert_eq!(packet.payload(), data_packet.payload());
                let response = server.handle_packet(&packet, client_addr.clone()).await;
                assert!(response.is_ok());
                let response = response.unwrap();
                assert!(response.is_some());
                let response = response.unwrap();
                server
                    .socket
                    .discv5
                    .talk_req(
                        client_addr,
                        Vec::try_from(ProtocolId::Utp).unwrap(),
                        response.as_ref().to_vec(),
                    )
                    .await
                    .unwrap();
            }
            Err(e) => panic!("{}", e),
        }

        client.recv(&mut buf).await.unwrap();

        // Gracefully closes connection
        let handle = tokio::spawn(async move { client.close().await });

        // Received FIN Packet
        server.recv(&mut buf).await.unwrap();
        handle.await.unwrap().unwrap();
    }
}