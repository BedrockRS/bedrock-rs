//! Bedrock Edition network protocol for BedrockRS.
//!
//! Primitive codecs (varints, little-endian numerics, strings), NBT in Bedrock's
//! little-endian and network (varint) flavors, the compressed batch format, and
//! packet definitions for the protocol version below. This crate is
//! transport-agnostic: it consumes and produces the byte messages that
//! `bedrockrs_net` carries over NetherNet.
//!
//! So far it covers the batch framing ([`batch`]), the login handshake
//! ([`packets`], [`login`]) and the spawn sequence: StartGame, hashed block
//! network IDs ([`block`]) and chunk encoding ([`chunk`]). NBT is encode-only.

pub mod batch;
pub mod block;
pub mod chunk;
pub mod io;
pub mod login;
pub mod nbt;
pub mod packet;
pub mod packets;
pub mod skin;
pub mod types;

/// Network protocol version spoken by this build (Bedrock 26.51).
pub const PROTOCOL_VERSION: i32 = 2193;

/// Game version reported to clients, e.g. in the `GET /v1/join` status response.
pub const GAME_VERSION: &str = "1.26.51";
