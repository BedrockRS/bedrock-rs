//! NetherNet transport for BedrockRS.
//!
//! Serves the HTTP signaling endpoints (`GET /v1/join`, `POST /v1/join/{networkId}`)
//! on the server's TCP port, negotiates a WebRTC session per client (ICE, DTLS,
//! SCTP) over a shared UDP socket, and exposes each connection as a stream of
//! reassembled byte messages on the reliable and unreliable data channels.
//! It knows nothing about Minecraft packets; see `bedrockrs_protocol` for those.
//!
//! [`Listener::bind`] starts everything, and [`Listener::accept`] yields a
//! [`Connection`] once a client's reliable data channel is open.

pub mod identity;
pub mod sdp;
pub mod segment;
pub mod signaling;

mod listener;
mod mux;
mod peer;

pub use identity::{ClientIdentity, ServerIdentity};
pub use listener::{Listener, ListenerConfig, ListenerError};
pub use peer::{Connection, ConnectionClosed, Message, Reliability};
pub use signaling::{OfferError, OfferHandler, ServerStatus};

/// Label of the ordered, reliable data channel the client opens.
pub const RELIABLE_CHANNEL: &str = "ReliableDataChannel";

/// Label of the unordered, unreliable (`maxRetransmits` 0) data channel the client opens.
pub const UNRELIABLE_CHANNEL: &str = "UnreliableDataChannel";
