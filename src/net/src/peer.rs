//! One client's WebRTC session, driven by str0m on its own task.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use str0m::channel::{ChannelData, ChannelId};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Event, IceConnectionState, Input, Output, Rtc, RtcError};
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::identity::ClientIdentity;
use crate::mux::{Datagram, PeerRoute, UdpMux};
use crate::segment::{self, Reassembler, SegmentError};
use crate::{RELIABLE_CHANNEL, UNRELIABLE_CHANNEL};

/// Largest SCTP message str0m sends when set up through its direct API, which
/// cannot pass it the client's advertised `max-message-size`.
const MAX_SCTP_SEND: u32 = 64 * 1024;
const INBOUND_QUEUE: usize = 1024;
const OUTBOUND_QUEUE: usize = 1024;
/// Stop taking outbound messages while this many bytes wait for SCTP buffer space.
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
const BUFFERED_AMOUNT_LOW: usize = 64 * 1024;
/// How long ICE may stay disconnected before the session is dropped.
const DISCONNECT_GRACE: Duration = Duration::from_secs(10);
/// How long a closing session may spend sending its close messages.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Longest sleep between polls, bounding str0m's "nothing scheduled" timeouts.
const MAX_IDLE: Duration = Duration::from_secs(5);

/// Which data channel a message travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reliability {
    Reliable,
    Unreliable,
}

/// A complete message received from or sent to a client.
#[derive(Debug, Clone)]
pub struct Message {
    pub payload: Bytes,
    pub reliability: Reliability,
}

/// The connection has closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("connection closed")]
pub struct ConnectionClosed;

/// An established NetherNet connection to one client. Dropping it closes the session.
#[derive(Debug)]
pub struct Connection {
    network_id: u64,
    identity: Option<ClientIdentity>,
    inbound: mpsc::Receiver<Message>,
    outbound: mpsc::Sender<Message>,
}

impl Connection {
    /// The client's NetherNet network ID from the signaling request.
    pub fn network_id(&self) -> u64 {
        self.network_id
    }

    /// The identity asserted in the client's offer. See [`ClientIdentity`] for
    /// what has been verified so far.
    pub fn client_identity(&self) -> Option<&ClientIdentity> {
        self.identity.as_ref()
    }

    /// Receives the next complete message, or `None` once the session has ended.
    pub async fn recv(&mut self) -> Option<Message> {
        self.inbound.recv().await
    }

    /// Queues a message, waiting while the session's send buffer is full.
    ///
    /// Empty payloads are ignored. Unreliable messages too large for one segment,
    /// or sent before the client opens its unreliable channel, go reliably instead.
    pub async fn send(
        &self,
        payload: Bytes,
        reliability: Reliability,
    ) -> Result<(), ConnectionClosed> {
        if payload.is_empty() {
            return Ok(());
        }
        self.outbound
            .send(Message {
                payload,
                reliability,
            })
            .await
            .map_err(|_| ConnectionClosed)
    }
}

/// Why a session ended.
#[derive(Debug, thiserror::Error)]
enum SessionEnd {
    #[error(transparent)]
    Rtc(#[from] RtcError),
    #[error("protocol violation: {0}")]
    Protocol(#[from] SegmentError),
    #[error("{0}")]
    Closed(&'static str),
}

/// Everything a session needs, prepared while answering the offer.
pub(crate) struct PeerSetup {
    pub id: u64,
    pub network_id: u64,
    pub rtc: Rtc,
    pub identity: Option<ClientIdentity>,
    pub mux: Arc<UdpMux>,
    pub route: PeerRoute,
    pub datagrams: mpsc::Receiver<Datagram>,
    pub accept: mpsc::Sender<Connection>,
    /// The client's `a=max-message-size`.
    pub remote_max_message_size: Option<u32>,
    /// Largest reassembled message accepted from the client.
    pub max_message_size: usize,
    pub connect_timeout: Duration,
}

pub(crate) struct Peer {
    id: u64,
    network_id: u64,
    rtc: Rtc,
    mux: Arc<UdpMux>,
    route: PeerRoute,
    learned_paths: HashSet<(SocketAddr, SocketAddr)>,
    datagrams: mpsc::Receiver<Datagram>,
    /// Taken when the connection is handed to the listener.
    accept: Option<mpsc::Sender<Connection>>,
    identity: Option<ClientIdentity>,
    to_app: Option<mpsc::Sender<Message>>,
    from_app: Option<mpsc::Receiver<Message>>,
    reliable: Option<ChannelId>,
    unreliable: Option<ChannelId>,
    reassembler: Reassembler,
    /// Reliable segments waiting for SCTP buffer space, in send order.
    pending: VecDeque<Vec<u8>>,
    pending_bytes: usize,
    segment_payload: NonZeroUsize,
    connect_deadline: Option<Instant>,
    disconnected_since: Option<Instant>,
    close_deadline: Option<Instant>,
}

impl Peer {
    pub fn new(setup: PeerSetup) -> Self {
        let max_send = setup
            .remote_max_message_size
            .unwrap_or(MAX_SCTP_SEND)
            .min(MAX_SCTP_SEND);
        // The SDP parser guarantees max-message-size > 1, leaving room for the header.
        let segment_payload = NonZeroUsize::new(max_send as usize - 1).unwrap_or(NonZeroUsize::MIN);
        Self {
            id: setup.id,
            network_id: setup.network_id,
            rtc: setup.rtc,
            mux: setup.mux,
            route: setup.route,
            learned_paths: HashSet::new(),
            datagrams: setup.datagrams,
            accept: Some(setup.accept),
            identity: setup.identity,
            to_app: None,
            from_app: None,
            reliable: None,
            unreliable: None,
            reassembler: Reassembler::new(setup.max_message_size),
            pending: VecDeque::new(),
            pending_bytes: 0,
            segment_payload,
            connect_deadline: Some(Instant::now() + setup.connect_timeout),
            disconnected_since: None,
            close_deadline: None,
        }
    }

    pub async fn run(mut self) {
        let end = self.drive().await;
        tracing::debug!(peer = self.id, network_id = self.network_id, %end, "NetherNet session ended");
        self.mux.unregister(self.id);
    }

    async fn drive(&mut self) -> SessionEnd {
        loop {
            let timeout = match self.poll() {
                Ok(timeout) => timeout,
                Err(end) => return end,
            };
            if !self.rtc.is_alive() {
                return SessionEnd::Closed("session closed");
            }
            let now = Instant::now();
            if let Some(end) = self.expired(now) {
                return end;
            }
            let wake = self.wake_time(timeout).min(now + MAX_IDLE);

            let result = tokio::select! {
                datagram = self.datagrams.recv() => match datagram {
                    Some(datagram) => self.receive(datagram),
                    None => Err(SessionEnd::Closed("UDP sockets closed")),
                },
                message = next_message(&mut self.from_app), if self.pending_bytes < MAX_PENDING_BYTES => {
                    match message {
                        Some(message) => self.send(message),
                        None => {
                            self.begin_close();
                            Ok(())
                        }
                    }
                }
                () = tokio::time::sleep_until(wake.into()) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now())).map_err(SessionEnd::from)
                }
            };
            if let Err(end) = result {
                return end;
            }
        }
    }

    /// Drains str0m's output and returns when it next needs a timeout.
    fn poll(&mut self) -> Result<Instant, SessionEnd> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(at) => return Ok(at),
                Output::Transmit(transmit) => self.transmit(&transmit),
                Output::Event(event) => self.handle_event(event)?,
            }
        }
    }

    fn transmit(&mut self, transmit: &Transmit) {
        if self
            .learned_paths
            .insert((transmit.source, transmit.destination))
        {
            self.mux
                .learn(transmit.source, transmit.destination, &self.route);
        }
        self.mux
            .send(transmit.source, transmit.destination, &transmit.contents);
    }

    fn receive(&mut self, datagram: Datagram) -> Result<(), SessionEnd> {
        // Anything that is not STUN, DTLS, RTP or RTCP is not ours to handle.
        let Ok(receive) = Receive::new(
            Protocol::Udp,
            datagram.source,
            datagram.destination,
            &datagram.data,
        ) else {
            return Ok(());
        };
        self.rtc
            .handle_input(Input::Receive(datagram.received, receive))?;
        // Acknowledgements may have freed SCTP buffer space.
        self.flush()
    }

    fn handle_event(&mut self, event: Event) -> Result<(), SessionEnd> {
        match event {
            Event::IceConnectionStateChange(state) => {
                tracing::trace!(peer = self.id, ?state, "ICE state changed");
                match state {
                    IceConnectionState::Disconnected => {
                        self.disconnected_since.get_or_insert_with(Instant::now);
                    }
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        self.disconnected_since = None;
                    }
                    _ => {}
                }
            }
            Event::Connected => tracing::debug!(peer = self.id, "DTLS established"),
            Event::ChannelOpen(id, label) => self.channel_opened(id, &label)?,
            Event::ChannelData(data) => self.channel_data(data)?,
            Event::ChannelClose(id) => {
                if self.reliable == Some(id) {
                    return Err(SessionEnd::Closed("client closed the reliable channel"));
                }
                if self.unreliable == Some(id) {
                    self.unreliable = None;
                }
            }
            Event::ChannelBufferedAmountLow(_) => self.flush()?,
            Event::Closed => return Err(SessionEnd::Closed("client closed the session")),
            _ => {}
        }
        Ok(())
    }

    fn channel_opened(&mut self, id: ChannelId, label: &str) -> Result<(), SessionEnd> {
        match label {
            RELIABLE_CHANNEL => {
                self.reliable = Some(id);
                if let Some(mut channel) = self.rtc.channel(id) {
                    channel.set_buffered_amount_low_threshold(BUFFERED_AMOUNT_LOW);
                }
                self.hand_out_connection()
            }
            UNRELIABLE_CHANNEL => {
                self.unreliable = Some(id);
                Ok(())
            }
            _ => {
                tracing::debug!(peer = self.id, label, "ignoring unexpected data channel");
                Ok(())
            }
        }
    }

    /// Gives the listener a [`Connection`] once the reliable channel is open.
    fn hand_out_connection(&mut self) -> Result<(), SessionEnd> {
        let Some(accept) = self.accept.take() else {
            return Ok(());
        };
        let (to_app, inbound) = mpsc::channel(INBOUND_QUEUE);
        let (outbound, from_app) = mpsc::channel(OUTBOUND_QUEUE);
        let connection = Connection {
            network_id: self.network_id,
            identity: self.identity.take(),
            inbound,
            outbound,
        };
        accept.try_send(connection).map_err(|err| match err {
            TrySendError::Full(_) => SessionEnd::Closed("listener accept queue is full"),
            TrySendError::Closed(_) => SessionEnd::Closed("listener closed"),
        })?;
        self.to_app = Some(to_app);
        self.from_app = Some(from_app);
        self.connect_deadline = None;
        Ok(())
    }

    fn channel_data(&mut self, data: ChannelData) -> Result<(), SessionEnd> {
        let message = if self.reliable == Some(data.id) {
            let Some(payload) = self.reassembler.push(&data.data)? else {
                return Ok(());
            };
            Message {
                payload: payload.into(),
                reliability: Reliability::Reliable,
            }
        } else if self.unreliable == Some(data.id) {
            // The unreliable channel never carries fragments.
            let Some((0, payload)) = data.data.split_first() else {
                tracing::trace!(peer = self.id, "dropping malformed unreliable message");
                return Ok(());
            };
            Message {
                payload: Bytes::copy_from_slice(payload),
                reliability: Reliability::Unreliable,
            }
        } else {
            return Ok(());
        };

        let Some(to_app) = &self.to_app else {
            return Ok(());
        };
        match to_app.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(SessionEnd::Closed(
                "client sends faster than the server reads",
            )),
            Err(TrySendError::Closed(_)) => {
                self.begin_close();
                Ok(())
            }
        }
    }

    fn send(&mut self, message: Message) -> Result<(), SessionEnd> {
        if message.reliability == Reliability::Unreliable
            && message.payload.len() <= self.segment_payload.get()
            && let Some(id) = self.unreliable
        {
            let mut frame = Vec::with_capacity(message.payload.len() + 1);
            frame.push(0);
            frame.extend_from_slice(&message.payload);
            if let Some(mut channel) = self.rtc.channel(id)
                && !channel.write(true, &frame)?
            {
                tracing::trace!(
                    peer = self.id,
                    "send buffer full, dropping unreliable message"
                );
            }
            return Ok(());
        }

        match segment::split(&message.payload, self.segment_payload) {
            Ok(segments) => {
                for segment in segments {
                    self.pending_bytes += segment.len();
                    self.pending.push_back(segment);
                }
            }
            Err(err) => tracing::warn!(peer = self.id, %err, "dropping outgoing message"),
        }
        self.flush()
    }

    /// Writes queued reliable segments while SCTP has buffer space for them.
    fn flush(&mut self) -> Result<(), SessionEnd> {
        let Some(id) = self.reliable else {
            return Ok(());
        };
        while let Some(segment) = self.pending.front() {
            let Some(mut channel) = self.rtc.channel(id) else {
                break;
            };
            if !channel.write(true, segment)? {
                break;
            }
            self.pending_bytes -= segment.len();
            self.pending.pop_front();
        }
        Ok(())
    }

    /// Starts a graceful close once the application drops its [`Connection`].
    fn begin_close(&mut self) {
        if self.close_deadline.is_some() {
            return;
        }
        self.from_app = None;
        self.close_deadline = Some(Instant::now() + CLOSE_GRACE);
        if let Err(err) = self.rtc.close() {
            tracing::debug!(peer = self.id, %err, "failed to close session cleanly");
            self.rtc.disconnect();
        }
    }

    fn expired(&self, now: Instant) -> Option<SessionEnd> {
        if self
            .connect_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            return Some(SessionEnd::Closed("data channels did not open in time"));
        }
        if self
            .disconnected_since
            .is_some_and(|since| now >= since + DISCONNECT_GRACE)
        {
            return Some(SessionEnd::Closed("ICE connection lost"));
        }
        if self.close_deadline.is_some_and(|deadline| now >= deadline) {
            return Some(SessionEnd::Closed("closed by the server"));
        }
        None
    }

    fn wake_time(&self, rtc_timeout: Instant) -> Instant {
        [
            Some(rtc_timeout),
            self.connect_deadline,
            self.disconnected_since
                .map(|since| since + DISCONNECT_GRACE),
            self.close_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rtc_timeout)
    }
}

async fn next_message(from_app: &mut Option<mpsc::Receiver<Message>>) -> Option<Message> {
    match from_app {
        Some(from_app) => from_app.recv().await,
        None => std::future::pending().await,
    }
}
