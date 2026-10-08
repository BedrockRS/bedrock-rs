//! BedrockRS game server core.
//!
//! Owns the fixed-rate game loop, which runs on a dedicated OS thread while tokio
//! handles networking, plus world state (chunks, blocks) and entity management.
//! It wires together `bedrockrs_net` (transport), `bedrockrs_protocol` (packets)
//! and `bedrockrs_plugins` (scripting).

use std::time::Duration;

pub mod auth;
pub mod blocks;
pub mod commands;
pub mod config;
pub mod console;
pub mod damage;
pub mod entities;
pub mod falling;
pub mod game_mode;
pub mod game_rules;
pub mod inventory;
pub mod items;
pub mod logins;
pub mod permissions;
pub mod placement;
pub mod players;
pub mod server;
pub mod session;
pub mod shape;
pub mod storage;
pub mod support;
pub mod tick;
pub mod view;
pub mod world;

/// Simulation rate of the game loop.
pub const TICKS_PER_SECOND: u32 = 20;

/// Wall-clock budget of a single tick (50 ms at 20 TPS).
pub const TICK_DURATION: Duration = Duration::from_millis(1000 / TICKS_PER_SECOND as u64);
