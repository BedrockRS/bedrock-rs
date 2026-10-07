//! A client's protocol session: login, then spawning into the world.
//!
//! [`Session`] is a sans-IO state machine over decoded packets; [`run`] drives
//! it over a NetherNet [`Connection`](bedrockrs_net::Connection). The flow follows gophertunnel's server
//! for NetherNet, where there is no ServerToClientHandshake because DTLS already
//! encrypts the connection:
//!
//! 1. RequestNetworkSettings → NetworkSettings, then compression starts.
//! 2. Login → PlayStatus(LoginSuccess) + ResourcePacksInfo.
//! 3. ResourcePackClientResponse(downloading finished) → ResourcePackStack.
//! 4. ResourcePackClientResponse(stack finished) → JigsawStructureData,
//!    VoxelShapes, StartGame and ItemRegistry.
//! 5. RequestChunkRadius → ChunkRadiusUpdated, NetworkChunkPublisherUpdate, the
//!    chunks in view, PlayStatus(PlayerSpawn) and CreativeContent.
//! 6. SetLocalPlayerAsInitialized: the player is past the loading screen and
//!    in the world. They join the [`Players`](crate::players::Players); a
//!    moment later plugins hear `player_join` and, unless one cancels it,
//!    everyone sees vanilla's "joined the game" message.
//!
//! From then on, chat messages (Text) are relayed to every player unless a
//! plugin cancels them, slash commands (CommandRequest) are run and answered
//! with their output, and plugins hear of blocks broken and placed. What the
//! player may do follows their [`GameMode`], which commands and plugins can
//! change through the session's [`Control`](crate::logins::Control) channel. Item
//! stack requests move items in the player's [`Inventory`], which is saved
//! with the player. Plugins
//! that heard `player_join` hear `player_quit` when the session ends, and
//! unless one cancels it, everyone sees vanilla's "left the game" message.

mod blocks;
mod commands;
mod connection;
mod health;
mod inventory;
mod login;
mod movement;
mod reply;
#[cfg(test)]
mod tests;

use std::sync::{Arc, LazyLock};

use bedrockrs_net::ClientIdentity;
use bedrockrs_protocol::batch::{BatchError, Compression, CompressionAlgorithm};
use bedrockrs_protocol::io::DecodeError;
use bedrockrs_protocol::login::LoginError;
use bedrockrs_protocol::packet::{self, Encode, id};
use bedrockrs_protocol::packets::{DisconnectReason, ItemInstance, Skin};
use bedrockrs_protocol::types::Vec3;
use uuid::Uuid;

use crate::damage::{
    DamageCause, Health, PEACEFUL_REGENERATION_INTERVAL, VOID_DAMAGE, VOID_DEPTH, VOID_INTERVAL,
};
use crate::entities::ItemEntities;
use crate::game_mode::GameMode;
use crate::game_rules;
use crate::inventory::Inventory;
use crate::items::items;
use crate::players::{EYE_HEIGHT, Movement};
use crate::storage::SavedPlayer;
use crate::view::ChunkView;
use crate::world::World;

pub use connection::run;
pub use reply::{Reply, SessionEvent};

/// Compression the server asks clients to use.
const COMPRESSION: Compression = Compression {
    algorithm: CompressionAlgorithm::Flate,
    threshold: 256,
};

/// Largest view distance granted, in chunks.
const MAX_VIEW_DISTANCE: i32 = 8;

/// Longest chat message relayed, in characters.
const MAX_CHAT_LENGTH: usize = 512;

/// The ItemRegistry and CreativeContent packets, the same for every player,
/// encoded once.
static ITEM_REGISTRY: LazyLock<Vec<u8>> = LazyLock::new(|| items().registry_packet().encode());
static CREATIVE_CONTENT: LazyLock<Vec<u8>> = LazyLock::new(|| items().creative_packet().encode());

/// Where the session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    RequestNetworkSettings,
    Login,
    /// Login is received; waiting for its token to be verified.
    Authenticating,
    ResourcePacks,
    /// StartGame is sent; waiting for the client's view distance.
    Spawning,
    /// Chunks and PlayerSpawn are sent; waiting for the client to finish loading.
    Initializing,
    InGame,
}

impl Stage {
    /// Whether the player has a world to be in. Packets without a handler are
    /// ignored from here on, since clients send many kinds while playing.
    fn in_world(self) -> bool {
        matches!(self, Self::Spawning | Self::Initializing | Self::InGame)
    }
}

/// Why the session failed; each error disconnects the client.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("malformed batch: {0}")]
    Batch(#[from] BatchError),
    #[error("malformed packet: {0}")]
    Decode(#[from] DecodeError),
    #[error("malformed login: {0}")]
    Login(#[from] LoginError),
    #[error("unexpected packet {id} while waiting for {stage:?}")]
    UnexpectedPacket { id: u32, stage: Stage },
}

impl SessionError {
    fn into_reply(self) -> Reply {
        tracing::debug!(err = %self, "session failed");
        let reason = match self {
            Self::UnexpectedPacket { .. } => DisconnectReason::UNEXPECTED_PACKET,
            _ => DisconnectReason::BAD_PACKET,
        };
        Reply::disconnect(reason, format!("BedrockRS: {self}"))
    }
}

/// One client's session, one decoded packet at a time.
#[derive(Debug)]
pub struct Session {
    stage: Stage,
    /// The identity the client proved during NetherNet signaling, if any.
    identity: Option<ClientIdentity>,
    /// The name the player logged in with.
    player: String,
    /// The player's persistent identity, known after login.
    uuid: Uuid,
    /// The runtime and unique ID of the player's entity.
    entity_id: u64,
    /// The chunks around the player and those the client already has.
    view: ChunkView,
    /// Whether the player is flying, as their client last said.
    flying: bool,
    /// Where the player is, as last reported.
    movement: Movement,
    /// What the player carries; empty until a saved one is loaded.
    inventory: Inventory,
    /// The skin the client sent at login, if it could be used.
    skin: Option<Arc<Skin>>,
    /// The hotbar slot the player holds, as their client last said.
    held_slot: u8,
    /// What others were last shown the player holding.
    shown_held: ItemInstance,
    /// Whether the player's inventory screen is open. Opening it twice makes
    /// the client crash, and latency can make it ask twice.
    inventory_open: bool,
    /// What the player may do.
    game_mode: GameMode,
    /// The game mode of new players, which the world reports as its own.
    default_game_mode: GameMode,
    /// Whether the player may run operator commands.
    operator: bool,
    /// The player's `minecraft:health`; 0 while they are dead.
    health: Health,
    /// How far the player has fallen since they last stood on something.
    fall_distance: f32,
    /// The player's own ticks since they joined, for what happens every so
    /// many: the void hurting, regeneration.
    ticks: u64,
    /// The world's game rules, as last told.
    rules: game_rules::Values,
    world: Arc<World>,
}

impl Session {
    pub fn new(
        identity: Option<ClientIdentity>,
        world: Arc<World>,
        entity_id: u64,
        default_game_mode: GameMode,
    ) -> Self {
        let spawn = world.spawn();
        Self {
            stage: Stage::RequestNetworkSettings,
            identity,
            player: String::from("<unknown>"),
            uuid: Uuid::nil(),
            entity_id,
            movement: Movement {
                position: Vec3 {
                    x: spawn.x as f32 + 0.5,
                    y: spawn.y as f32 + EYE_HEIGHT,
                    z: spawn.z as f32 + 0.5,
                },
                pitch: 0.0,
                yaw: 0.0,
                head_yaw: 0.0,
                on_ground: true,
            },
            view: ChunkView::new(),
            flying: false,
            inventory: Inventory::default(),
            skin: None,
            held_slot: 0,
            shown_held: ItemInstance::EMPTY,
            inventory_open: false,
            game_mode: default_game_mode,
            default_game_mode,
            operator: false,
            health: Health::PLAYER,
            fall_distance: 0.0,
            ticks: 0,
            rules: game_rules::Values::default(),
            world,
        }
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The player's name, once logged in.
    pub fn player(&self) -> &str {
        &self.player
    }

    /// The player's UUID and what to save for them, once they have been in
    /// the world; players who never finished spawning are not saved.
    pub fn saved_player(&self) -> Option<(Uuid, SavedPlayer)> {
        (self.stage == Stage::InGame).then(|| {
            let mut saved = self.movement.saved(self.flying);
            saved.inventory = Some(self.inventory.saved());
            saved.game_mode = Some(self.game_mode.name().to_owned());
            saved.health = Some(self.health.value);
            (self.uuid, saved)
        })
    }

    /// What the player carries.
    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn game_mode(&self) -> GameMode {
        self.game_mode
    }

    pub fn health(&self) -> Health {
        self.health
    }

    /// The player's own tick, while they are in the world: picking items up,
    /// the void hurting, and regeneration (the world is always peaceful, where
    /// players regain a point a second).
    pub fn tick(&mut self, items: &ItemEntities) -> Reply {
        if self.stage != Stage::InGame || self.is_dead() {
            return Reply::default();
        }
        self.ticks += 1;
        let mut reply = self.pick_up(items);
        if self.movement.feet().y < VOID_DEPTH && self.ticks.is_multiple_of(VOID_INTERVAL) {
            reply
                .events
                .extend(self.proposed_damage(DamageCause::Void, VOID_DAMAGE));
        }
        if self.rules.naturalregeneration
            && !self.health.is_full()
            && self.ticks.is_multiple_of(PEACEFUL_REGENERATION_INTERVAL)
        {
            self.health.heal(1.0);
            reply.packets.push(self.health_attribute().encode());
            reply
                .events
                .push(SessionEvent::HealthChanged(self.health.value));
        }
        reply
    }

    /// Handles one encoded packet (header and payload).
    pub fn handle(&mut self, packet: &[u8]) -> Result<Reply, SessionError> {
        let (header, payload) = packet::read_header(packet)?;
        match (self.stage, header.id) {
            // The blob cache is not supported; the client copes without it.
            (_, id::CLIENT_CACHE_STATUS) => Ok(Reply::default()),
            (Stage::RequestNetworkSettings, id::REQUEST_NETWORK_SETTINGS) => {
                self.request_network_settings(packet::decode(payload)?)
            }
            (Stage::Login, id::LOGIN) => self.login(packet::decode(payload)?),
            (Stage::ResourcePacks, id::RESOURCE_PACK_CLIENT_RESPONSE) => {
                self.pack_response(packet::decode(payload)?)
            }
            (stage, id::REQUEST_CHUNK_RADIUS) if stage.in_world() => {
                Ok(self.chunk_radius(packet::decode(payload)?))
            }
            (Stage::Initializing | Stage::InGame, id::SET_LOCAL_PLAYER_AS_INITIALIZED) => {
                Ok(self.initialized(packet::decode(payload)?))
            }
            (Stage::InGame, id::TEXT) => Ok(self.text(packet::decode(payload)?)),
            (Stage::InGame, id::COMMAND_REQUEST) => {
                Ok(self.command_request(packet::decode(payload)?))
            }
            (Stage::InGame, id::PLAYER_AUTH_INPUT) => Ok(self.auth_input(packet::decode(payload)?)),
            (Stage::InGame, id::PLAYER_ACTION) => Ok(self.player_action(packet::decode(payload)?)),
            (Stage::InGame, id::RESPAWN) => Ok(self.respawn(packet::decode(payload)?)),
            (Stage::InGame, id::ANIMATE) => Ok(self.animate(packet::decode(payload)?)),
            (stage, id::MOB_EQUIPMENT) if stage.in_world() => {
                Ok(self.equipment(packet::decode(payload)?))
            }
            // The inventory screen opens only once the server says so.
            (stage, id::INTERACT) if stage.in_world() => match packet::decode(payload) {
                Ok(interact) => Ok(self.interact(interact)),
                Err(err) => {
                    tracing::debug!(player = %self.player, %err, "ignoring an unreadable interaction");
                    Ok(Reply::default())
                }
            },
            (stage, id::CONTAINER_CLOSE) if stage.in_world() => {
                Ok(self.container_close(packet::decode(payload)?))
            }
            // A request that cannot be read (such as auto-crafting, which is
            // not supported) cannot be answered either; the client's view of
            // its inventory is corrected at the next accepted request.
            // It cannot be answered, so the client is shown its inventory.
            (stage, id::ITEM_STACK_REQUEST) if stage.in_world() => match packet::decode(payload) {
                Ok(request) => Ok(self.item_stack_request(request)),
                Err(err) => {
                    tracing::debug!(player = %self.player, %err, "ignoring an unreadable item stack request");
                    Ok(Reply::send(self.inventory_sync()))
                }
            },
            // Only item use is decoded; a transaction that cannot be read is
            // ignored rather than ending the session.
            (Stage::InGame, id::INVENTORY_TRANSACTION) => match packet::decode(payload) {
                Ok(transaction) => Ok(self.inventory_transaction(transaction)),
                Err(err) => {
                    tracing::debug!(player = %self.player, %err, "ignoring an unreadable inventory transaction");
                    Ok(Reply::send(self.inventory_sync()))
                }
            },
            (stage, id) if stage.in_world() => {
                // Movement input starts before the player is initialized.
                if id != id::PLAYER_AUTH_INPUT {
                    tracing::trace!(id, "ignoring packet without a handler");
                }
                Ok(Reply::default())
            }
            (stage, id) => Err(SessionError::UnexpectedPacket { id, stage }),
        }
    }
}
