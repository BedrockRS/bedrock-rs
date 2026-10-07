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
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A player finished loading and is in the world. Plugins may cancel it,
    /// and then nobody sees vanilla's "joined the game" message.
    PlayerJoin(Player),
    /// A player who joined left the world, however they left. Plugins may
    /// cancel it, and then nobody sees vanilla's "left the game" message.
    PlayerQuit(Player),
    /// A player sent a chat message. Plugins may cancel it, and then nobody sees it.
    PlayerChat { player: Player, message: String },
    /// A player broke a block.
    BlockBreak(BlockChange),
    /// A player placed a block.
    BlockPlace(BlockChange),
    /// A player is about to take damage. Plugins may cancel it.
    PlayerDamage(Damage),
    /// A player died. `message` is the death message in English.
    PlayerDeath {
        player: Player,
        cause: String,
        message: String,
    },
    /// A dead player respawned.
    PlayerRespawn(Player),
}

/// Damage a player is about to take.
#[derive(Debug, Clone, PartialEq)]
pub struct Damage {
    pub player: Player,
    /// One of [`DAMAGE_CAUSES`].
    pub cause: String,
    pub amount: f32,
    /// The player's health before the damage.
    pub health: f32,
}

/// Vanilla's damage causes, as scripts name them (`@minecraft/server`'s
/// `EntityDamageCause`).
pub const DAMAGE_CAUSES: [&str; 35] = [
    "anvil",
    "blockExplosion",
    "campfire",
    "contact",
    "drowning",
    "entityAttack",
    "entityExplosion",
    "fall",
    "fallingBlock",
    "fire",
    "fireTick",
    "fireworks",
    "flyIntoWall",
    "freezing",
    "lava",
    "lightning",
    "maceSmash",
    "magic",
    "magma",
    "none",
    "override",
    "piston",
    "projectile",
    "ramAttack",
    "selfDestruct",
    "sonicBoom",
    "soulCampfire",
    "stalactite",
    "stalagmite",
    "starve",
    "suffocation",
    "temperature",
    "thorns",
    "void",
    "wither",
];

impl Event {
    /// Names plugins can listen for.
    pub const NAMES: [&str; 8] = [
        "player_join",
        "player_quit",
        "player_chat",
        "block_break",
        "block_place",
        "player_damage",
        "player_death",
        "player_respawn",
    ];

    /// The name plugins listen for this event by, e.g. `server.on("player_join", …)`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PlayerJoin(_) => "player_join",
            Self::PlayerQuit(_) => "player_quit",
            Self::PlayerChat { .. } => "player_chat",
            Self::BlockBreak(_) => "block_break",
            Self::BlockPlace(_) => "block_place",
            Self::PlayerDamage(_) => "player_damage",
            Self::PlayerDeath { .. } => "player_death",
            Self::PlayerRespawn(_) => "player_respawn",
        }
    }

    /// Whether plugins can stop what this event describes from happening.
    pub fn is_cancellable(&self) -> bool {
        matches!(
            self,
            Self::PlayerJoin(_)
                | Self::PlayerQuit(_)
                | Self::PlayerChat { .. }
                | Self::PlayerDamage(_)
        )
    }
}

/// Something a plugin asks the server to do. Players are named by UUID string.
#[derive(Debug, Clone, PartialEq)]
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
    /// Set a player's health; 0 kills them.
    SetHealth { player: String, health: f32 },
    /// Hurt a player, as `cause` (one of [`DAMAGE_CAUSES`]) would.
    Damage {
        player: String,
        amount: f32,
        cause: String,
    },
}
