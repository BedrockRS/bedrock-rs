//! What plugins and the server say to each other, whatever the engine.
//!
//! The server sends [`Event`]s to plugins, and plugins answer with
//! [`Action`]s for the server to carry out. Both cross threads as messages.

use crate::command::PluginCommand;

/// A player as plugins see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    /// The name shown in game. Players can change it, so key stored data by `uuid`.
    pub name: String,
    /// The player's persistent identity, a UUID string that stays the same
    /// across sessions and name changes.
    pub uuid: String,
}

/// A block position in the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// A block a player changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockChange {
    pub player: Player,
    pub position: Position,
    /// The block's name, e.g. `minecraft:stone`: the block broken, or the one placed.
    pub block: String,
}

/// Something that happened in the game, delivered to the plugins listening for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A player finished spawning and is in the world.
    PlayerJoin(Player),
    /// A player who joined left the world, however they left.
    PlayerQuit(Player),
    /// A player sent a chat message. Plugins may cancel it, and then nobody sees it.
    PlayerChat { player: Player, message: String },
    /// A player broke a block.
    BlockBreak(BlockChange),
    /// A player placed a block.
    BlockPlace(BlockChange),
}

impl Event {
    /// Names plugins can listen for.
    pub const NAMES: [&str; 5] = [
        "player_join",
        "player_quit",
        "player_chat",
        "block_break",
        "block_place",
    ];

    /// The name plugins listen for this event by, e.g. `server.on("player_join", …)`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PlayerJoin(_) => "player_join",
            Self::PlayerQuit(_) => "player_quit",
            Self::PlayerChat { .. } => "player_chat",
            Self::BlockBreak(_) => "block_break",
            Self::BlockPlace(_) => "block_place",
        }
    }

    /// Whether plugins can stop what this event describes from happening.
    pub fn is_cancellable(&self) -> bool {
        matches!(self, Self::PlayerChat { .. })
    }
}

/// Something a plugin asks the server to do. Players are named by UUID string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Show a message in every player's chat.
    Broadcast(String),
    /// Show a message in one player's chat.
    SendMessage { player: String, message: String },
    /// Disconnect a player, showing them `reason`.
    Kick { player: String, reason: String },
    /// Change a player's game mode; `mode` is one of
    /// [`GAME_MODE_VALUES`](crate::GAME_MODE_VALUES).
    SetGameMode { player: String, mode: String },
    /// The commands plugins have registered changed; these are all of them now.
    SetCommands(Vec<PluginCommand>),
}
