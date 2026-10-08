//! Who is in the world, where they are, and how to reach them.
//!
//! Everyone online is in everyone's player list. Player *entities* are tracked
//! per viewer: each tick, [`Players::tick`] shows a viewer the players standing
//! in chunks within their view radius (AddPlayer), hides those who left it
//! (RemoveActor), and sends the movement of the players they can see.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bedrockrs_protocol::block::BlockState;
use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    ActorEvent, AddPlayer, Animate, EntityMetadata, INVENTORY_WINDOW, ItemInstance, MetadataValue,
    MobArmorEquipment, MobEquipment, MoveMode, MovePlayer, PlayerList, PlayerListEntry, PlayerSkin,
    RemoveActor, SetActorData, Skin, TakeItemActor, Text, TextType, UpdatePlayerGameType,
    entity_flag, metadata_key,
};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};
use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TrySendError};
use uuid::Uuid;

use crate::entities::EntityView;
use crate::game_mode::GameMode;
use crate::shape;
use crate::storage::{SavedInventory, SavedPlayer};

/// Packets that may wait for one player before more are dropped.
pub const OUTBOUND_QUEUE: usize = 256;

/// Height of a player's eyes above their feet.
pub const EYE_HEIGHT: f32 = 1.62;

/// Encoded packets for a session to batch and send to its client.
pub type Outbound = mpsc::Sender<Bytes>;

/// Who a player is, and how they look.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    /// The persistent identity, stable across sessions and name changes.
    pub uuid: Uuid,
    /// The skin their client sent, or a placeholder.
    pub skin: Arc<Skin>,
}

/// A plain skin in a colour taken from the UUID, for a player whose own
/// skin could not be used.
pub fn placeholder_skin(uuid: Uuid) -> Arc<Skin> {
    let [red, green, blue, ..] = uuid.into_bytes();
    Arc::new(Skin::solid(
        format!("bedrockrs.{uuid}"),
        [red, green, blue, 255],
    ))
}

/// Where a player is and where they look. Angles are in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Movement {
    /// Where the player's eyes are: their feet plus [`EYE_HEIGHT`].
    pub position: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub on_ground: bool,
}

impl Movement {
    /// Where the player's feet are.
    pub fn feet(&self) -> Vec3 {
        Vec3 {
            y: self.position.y - EYE_HEIGHT,
            ..self.position
        }
    }

    /// The chunk column the player stands in.
    pub fn chunk(&self) -> ChunkPos {
        ChunkPos::of_block(BlockPos::containing(self.feet()))
    }

    /// What is saved for the player: feet position, rotation and flying. The
    /// inventory is left for the caller to add.
    pub fn saved(&self, flying: bool) -> SavedPlayer {
        let feet = self.feet();
        SavedPlayer {
            x: feet.x,
            y: feet.y,
            z: feet.z,
            pitch: self.pitch,
            yaw: self.yaw,
            head_yaw: self.head_yaw,
            flying,
            inventory: None,
            game_mode: None,
            health: None,
        }
    }

    /// Where a returning player starts: where they left.
    pub fn from_saved(saved: &SavedPlayer) -> Self {
        Self {
            position: Vec3 {
                x: saved.x,
                y: saved.y + EYE_HEIGHT,
                z: saved.z,
            },
            pitch: saved.pitch,
            yaw: saved.yaw,
            head_yaw: saved.head_yaw,
            on_ground: true,
        }
    }
}

/// The chunks a player's client shows: a circle of `radius` chunks around `centre`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    pub centre: ChunkPos,
    pub radius: i32,
}

impl View {
    pub fn contains(&self, chunk: ChunkPos) -> bool {
        chunk.distance_squared(self.centre) <= i64::from(self.radius).pow(2)
    }
}

/// A player entering the world.
pub struct Joining {
    /// The runtime and unique ID of the player's entity, from
    /// [`Players::allocate_entity_id`].
    pub entity_id: u64,
    pub profile: Profile,
    pub movement: Movement,
    pub view: View,
    /// What the player carries, as saved.
    pub inventory: SavedInventory,
    /// What they hold and in which hotbar slot, which AddPlayer shows others.
    pub held: (ItemInstance, u8),
    /// What they wear.
    pub armor: [ItemInstance; 4],
    pub game_mode: GameMode,
    pub health: f32,
    pub outbound: Outbound,
}

struct Online {
    profile: Profile,
    movement: Movement,
    /// Whether `movement` changed since the last tick.
    moved: bool,
    view: View,
    sneaking: bool,
    /// Whether the player is flying, as their client last said.
    flying: bool,
    /// Players whose entity this player's client has, by entity ID.
    seen: HashSet<u64>,
    /// Item entities this player's client has.
    seen_items: HashSet<u64>,
    /// What the player holds, and in which hotbar slot, for others to see.
    held: (ItemInstance, u8),
    /// What the player wears, for others to see.
    armor: [ItemInstance; 4],
    /// The player's inventory as last changed, for saving.
    inventory: SavedInventory,
    game_mode: GameMode,
    health: f32,
    /// The tick the player died, while they are dead. Others see them fall
    /// over, then they disappear.
    died_at: Option<u64>,
    outbound: Outbound,
}

impl Online {
    /// Shows the items and falling blocks in view that the client lacks,
    /// hides those gone or out of view, and moves the rest that moved.
    fn sync_items(&mut self, items: &[EntityView]) {
        let current: HashSet<u64> = items.iter().map(|item| item.id).collect();
        let gone: Vec<u64> = self
            .seen_items
            .iter()
            .filter(|id| !current.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.seen_items.remove(&id);
            self.send(encode(&RemoveActor {
                entity_unique_id: unique_id(id),
            }));
        }
        for item in items {
            let visible = self.view.contains(item.chunk);
            let seen = self.seen_items.contains(&item.id);
            match (visible, seen) {
                (true, false) => {
                    self.seen_items.insert(item.id);
                    self.send(item.add.clone());
                }
                (true, true) if item.changed => {
                    // A different count: shown afresh.
                    self.send(encode(&RemoveActor {
                        entity_unique_id: unique_id(item.id),
                    }));
                    self.send(item.add.clone());
                }
                (true, true) => {
                    for packet in item.movement.iter().flatten() {
                        self.send(packet.clone());
                    }
                }
                (false, true) => {
                    self.seen_items.remove(&item.id);
                    self.send(encode(&RemoveActor {
                        entity_unique_id: unique_id(item.id),
                    }));
                }
                (false, false) => {}
            }
        }
    }

    fn send(&self, packet: Bytes) {
        if let Err(TrySendError::Full(_)) = self.outbound.try_send(packet) {
            tracing::warn!(
                "{} isn't keeping up, so a packet to them was dropped",
                self.profile.name
            );
        }
    }
}

/// The players in the world, by entity ID. Sessions join once their player has
/// spawned and leave when their [`Membership`] is dropped.
#[derive(Default)]
pub struct Players {
    last_entity_id: AtomicU64,
    /// The last tick [`Players::tick`] ran.
    tick: AtomicU64,
    online: Mutex<HashMap<u64, Online>>,
}

impl Players {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new entity ID for a session's player, unique for the server's lifetime.
    pub fn allocate_entity_id(&self) -> u64 {
        self.last_entity_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Adds a player and puts everyone in each other's player list. Entities
    /// follow on the next tick, for those in view.
    pub fn join(&self, joining: Joining) -> Membership<'_> {
        let Joining {
            entity_id,
            profile,
            movement,
            view,
            inventory,
            held,
            armor,
            game_mode,
            health,
            outbound,
        } = joining;
        let newcomer = Online {
            profile,
            movement,
            moved: false,
            view,
            sneaking: false,
            flying: false,
            seen: HashSet::new(),
            seen_items: HashSet::new(),
            held,
            armor,
            inventory,
            game_mode,
            health,
            died_at: None,
            outbound,
        };
        let mut online = self.online();

        if !online.is_empty() {
            let entries = online
                .iter()
                .map(|(id, other)| list_entry(*id, &other.profile))
                .collect();
            newcomer.send(encode(&PlayerList::Add(entries)));
            let entry = encode(&PlayerList::Add(vec![list_entry(
                entity_id,
                &newcomer.profile,
            )]));
            for other in online.values() {
                other.send(entry.clone());
            }
        }
        online.insert(entity_id, newcomer);
        Membership {
            players: self,
            entity_id,
        }
    }

    pub fn count(&self) -> usize {
        self.online().len()
    }

    /// Everyone online, as they would be saved.
    pub fn saved(&self) -> Vec<(Uuid, SavedPlayer)> {
        self.online()
            .values()
            .map(|player| {
                let mut saved = player.movement.saved(player.flying);
                saved.inventory = Some(player.inventory.clone());
                saved.game_mode = Some(player.game_mode.name().to_owned());
                saved.health = Some(player.health);
                (player.profile.uuid, saved)
            })
            .collect()
    }

    /// Updates who sees whom and which items and falling blocks, then sends everyone and
    /// everything that moved to the players who can see them.
    pub fn tick(&self, tick: u64, items: &[EntityView]) {
        self.tick.store(tick, Ordering::Relaxed);
        let mut online = self.online();
        for viewer in online.values_mut() {
            viewer.sync_items(items);
        }
        // Spectators are seen by nobody, and the dead only while they fall over.
        let snapshot: Vec<(u64, ChunkPos, bool)> = online
            .iter()
            .map(|(id, player)| {
                let fallen = player
                    .died_at
                    .is_some_and(|died| tick >= died + DEATH_ANIMATION_TICKS);
                let present = player.game_mode.is_present() && !fallen;
                (*id, player.movement.chunk(), present)
            })
            .collect();

        // Entities entering and leaving each viewer's view. A newly shown
        // entity already carries its current position.
        let mut shown: HashSet<(u64, u64)> = HashSet::new();
        let mut spawns = Vec::new();
        for (viewer_id, viewer) in online.iter_mut() {
            for (target_id, chunk, present) in &snapshot {
                if target_id == viewer_id {
                    continue;
                }
                let visible = *present && viewer.view.contains(*chunk);
                if visible && viewer.seen.insert(*target_id) {
                    shown.insert((*viewer_id, *target_id));
                    spawns.push((*viewer_id, *target_id));
                } else if !visible && viewer.seen.remove(target_id) {
                    viewer.send(encode(&RemoveActor {
                        entity_unique_id: unique_id(*target_id),
                    }));
                }
            }
        }
        for (viewer_id, target_id) in spawns {
            let target = &online[&target_id];
            let viewer = &online[&viewer_id];
            viewer.send(encode(&add_player(target_id, target)));
            // As Dragonfly does: the skin again, so the client applies it to
            // the entity, then what they hold, which AddPlayer alone does not
            // show.
            viewer.send(encode(&PlayerSkin {
                uuid: target.profile.uuid,
                skin: Skin::clone(&target.profile.skin),
            }));
            if !target.held.0.is_empty() {
                viewer.send(encode(&held_item(target_id, target)));
            }
            if target.armor.iter().any(|piece| !piece.is_empty()) {
                viewer.send(encode(&worn_armor(target_id, target)));
            }
        }

        let moved: Vec<(u64, Movement)> = online
            .iter_mut()
            .filter(|(_, player)| player.moved)
            .map(|(id, player)| {
                player.moved = false;
                (*id, player.movement)
            })
            .collect();
        for (mover, movement) in moved {
            let packet = encode(&MovePlayer {
                entity_runtime_id: mover,
                position: movement.position,
                pitch: movement.pitch,
                yaw: movement.yaw,
                head_yaw: movement.head_yaw,
                mode: MoveMode::Normal,
                on_ground: movement.on_ground,
                ridden_entity_runtime_id: 0,
                tick,
            });
            for (viewer_id, viewer) in online.iter() {
                if viewer.seen.contains(&mover) && !shown.contains(&(*viewer_id, mover)) {
                    viewer.send(packet.clone());
                }
            }
        }
    }

    /// Shows the players who see an item entity it flying to the player
    /// with entity ID `taker`, who picked it up.
    pub fn show_pickup(&self, item: u64, taker: u64) {
        let packet = encode(&TakeItemActor {
            item_entity_runtime_id: item,
            taker_entity_runtime_id: taker,
        });
        for (id, player) in self.online().iter() {
            // The taker sees it even before the next tick has shown them the item.
            if player.seen_items.contains(&item) || *id == taker {
                player.send(packet.clone());
            }
        }
    }

    /// Queues an encoded packet for every player. A player whose queue is full
    /// misses it rather than holding everyone else up.
    pub fn broadcast(&self, packet: &Bytes) {
        for online in self.online().values() {
            online.send(packet.clone());
        }
    }

    /// Whether any player's body is inside the collision of `state` placed
    /// at `pos`, so placing it there would trap them.
    pub fn in_the_way(&self, pos: BlockPos, state: &BlockState) -> bool {
        self.online().values().any(|player| {
            let height = if player.sneaking {
                SNEAKING_HEIGHT
            } else {
                STANDING_HEIGHT
            };
            shape::body_inside(player.movement.feet(), HALF_WIDTH, height, pos, state)
        })
    }

    /// Queues an encoded packet for every player whose client has `chunk`.
    pub fn send_to_viewers(&self, chunk: ChunkPos, packet: &Bytes) {
        for online in self.online().values() {
            if online.view.contains(chunk) {
                online.send(packet.clone());
            }
        }
    }

    /// Removes entity `id` now from everyone who has it, rather than at the
    /// end of the tick.
    pub fn hide_entity(&self, id: u64) {
        for online in self.online().values_mut() {
            if online.seen_items.remove(&id) {
                online.send(encode(&RemoveActor {
                    entity_unique_id: unique_id(id),
                }));
            }
        }
    }

    /// Shows entity `id` to everyone with `chunk` now, with `packets` (its
    /// AddActor first), rather than at the end of the tick, and remembers
    /// they have it, so the tick only moves it.
    pub fn show_entity(&self, chunk: ChunkPos, id: u64, packets: &[Bytes]) {
        for online in self.online().values_mut() {
            if online.view.contains(chunk) && online.seen_items.insert(id) {
                for packet in packets {
                    online.send(packet.clone());
                }
            }
        }
    }

    /// Shows a message from the server (a plugin announcement, say) in every
    /// player's chat, as a system message.
    pub fn broadcast_message(&self, message: &str) {
        self.broadcast_text(Text::system(message));
    }

    /// Shows a message from the server in one player's chat, as a system
    /// message. Returns whether that player is in the world.
    pub fn send_message(&self, uuid: Uuid, message: &str) -> bool {
        let text = Text::system(message);
        if text.message.is_empty() || text.message.len() > Text::MAX_MESSAGE_LEN {
            tracing::warn!(
                "Didn't send a message that is empty or too long ({} bytes)",
                text.message.len()
            );
            return false;
        }
        let online = self.online();
        let Some(player) = online.values().find(|player| player.profile.uuid == uuid) else {
            return false;
        };
        player.send(encode(&text));
        true
    }

    /// The player online with this name, ignoring case: their UUID and name.
    pub fn find(&self, name: &str) -> Option<(Uuid, String)> {
        self.online()
            .values()
            .find(|player| player.profile.name.eq_ignore_ascii_case(name))
            .map(|player| (player.profile.uuid, player.profile.name.clone()))
    }

    /// The names of everyone online.
    pub fn names(&self) -> Vec<String> {
        self.online()
            .values()
            .map(|player| player.profile.name.clone())
            .collect()
    }

    /// The name of the player in the world as `uuid`, if they are.
    pub fn name_of(&self, uuid: Uuid) -> Option<String> {
        self.online()
            .values()
            .find(|player| player.profile.uuid == uuid)
            .map(|player| player.profile.name.clone())
    }

    /// Relays a player's chat message to everyone, including its author, as
    /// plain text.
    pub fn chat(&self, from: &str, message: &str) {
        self.broadcast_text(Text::raw(format!("<{from}> {message}")));
    }

    /// Shows everyone a vanilla message that each client translates, such
    /// as `§e%multiplayer.player.joined`: `%` and a translation key, after
    /// any formatting, filled in with `parameters`.
    pub fn broadcast_translation(&self, message: &str, parameters: &[String]) {
        self.broadcast_text(Text {
            text_type: TextType::Translation,
            needs_translation: true,
            parameters: parameters.to_vec(),
            ..Text::raw(message)
        });
    }

    fn broadcast_text(&self, text: Text) {
        if text.message.is_empty() || text.message.len() > Text::MAX_MESSAGE_LEN {
            tracing::warn!(
                "Didn't broadcast a message that is empty or too long ({} bytes)",
                text.message.len()
            );
            return;
        }
        self.broadcast(&encode(&text));
    }

    fn online(&self) -> MutexGuard<'_, HashMap<u64, Online>> {
        // The map stays consistent even if a holder panicked.
        self.online.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A player's place in [`Players`]; dropping it removes the player, their
/// entity from everyone who sees it, and their player list entry.
#[must_use = "the player leaves when this is dropped"]
pub struct Membership<'a> {
    players: &'a Players,
    entity_id: u64,
}

impl Membership<'_> {
    /// Records where the player is now; others see it on the next tick.
    pub fn moved(&self, movement: Movement) {
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.movement = movement;
            player.moved = true;
        }
    }

    /// Records the chunks the player's client now shows; entities follow on
    /// the next tick.
    pub fn viewing(&self, view: View) {
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.view = view;
        }
    }

    /// Starts or stops the player sneaking: their entity data goes to everyone
    /// who sees them, and to themselves, since the client waits for it.
    pub fn sneaking(&self, sneaking: bool) {
        let mut online = self.players.online();
        let Some(player) = online.get_mut(&self.entity_id) else {
            return;
        };
        if player.sneaking == sneaking {
            return;
        }
        player.sneaking = sneaking;
        let packet = encode(&SetActorData {
            entity_runtime_id: self.entity_id,
            metadata: player_metadata(&player.profile.name, sneaking),
            tick: 0,
        });
        for (id, other) in online.iter() {
            if *id == self.entity_id || other.seen.contains(&self.entity_id) {
                other.send(packet.clone());
            }
        }
    }

    /// Records what the player carries now, for saving.
    pub fn inventory(&self, inventory: SavedInventory) {
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.inventory = inventory;
        }
    }

    /// Shows everyone who sees the player what they now hold.
    pub fn holding(&self, item: ItemInstance, slot: u8) {
        let mut online = self.players.online();
        let Some(player) = online.get_mut(&self.entity_id) else {
            return;
        };
        player.held = (item, slot);
        let packet = encode(&held_item(self.entity_id, player));
        for other in online.values() {
            if other.seen.contains(&self.entity_id) {
                other.send(packet.clone());
            }
        }
    }

    /// Shows everyone who sees the player what they now wear.
    pub fn armor(&self, armor: [ItemInstance; 4]) {
        let mut online = self.players.online();
        let Some(player) = online.get_mut(&self.entity_id) else {
            return;
        };
        player.armor = armor;
        let packet = encode(&worn_armor(self.entity_id, player));
        for other in online.values() {
            if other.seen.contains(&self.entity_id) {
                other.send(packet.clone());
            }
        }
    }

    /// Records the player's game mode, for saving, and tells everyone else.
    /// Spectators disappear from view on the next tick.
    pub fn game_mode(&self, mode: GameMode) {
        let mut online = self.players.online();
        let Some(player) = online.get_mut(&self.entity_id) else {
            return;
        };
        player.game_mode = mode;
        let packet = encode(&UpdatePlayerGameType {
            game_type: mode.id(),
            player_unique_id: unique_id(self.entity_id),
            tick: 0,
        });
        for (id, other) in online.iter() {
            if *id != self.entity_id {
                other.send(packet.clone());
            }
        }
    }

    /// Records the player's health, for saving.
    pub fn health(&self, health: f32) {
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.health = health;
        }
    }

    /// Shows everyone who sees the player flinching from damage.
    pub fn hurt(&self) {
        self.show_others(ActorEvent::new(self.entity_id, ActorEvent::HURT));
    }

    /// Shows everyone who sees the player falling over dead; they disappear
    /// shortly after.
    pub fn died(&self) {
        let tick = self.players.tick.load(Ordering::Relaxed);
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.died_at = Some(tick);
            player.health = 0.0;
        }
        self.show_others(ActorEvent::new(self.entity_id, ActorEvent::DEATH));
    }

    /// The player is alive again: others see them from the next tick, where
    /// they now stand.
    pub fn respawned(&self, health: f32) {
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.died_at = None;
            player.health = health;
        }
    }

    fn show_others(&self, packet: impl Encode) {
        let packet = encode(&packet);
        for other in self.players.online().values() {
            if other.seen.contains(&self.entity_id) {
                other.send(packet.clone());
            }
        }
    }

    /// Records whether the player is flying, for saving.
    pub fn flying(&self, flying: bool) {
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.flying = flying;
        }
    }

    /// Shows the player swinging their arm to everyone who sees them; their
    /// own client animates itself.
    pub fn swing(&self) {
        let packet = encode(&Animate {
            action: Animate::SWING_ARM,
            entity_runtime_id: self.entity_id,
            swing_source: None,
        });
        for other in self.players.online().values() {
            if other.seen.contains(&self.entity_id) {
                other.send(packet.clone());
            }
        }
    }
}

impl Drop for Membership<'_> {
    fn drop(&mut self) {
        let mut online = self.players.online();
        let Some(left) = online.remove(&self.entity_id) else {
            return;
        };
        let entity = encode(&RemoveActor {
            entity_unique_id: unique_id(self.entity_id),
        });
        let list = encode(&PlayerList::Remove(vec![left.profile.uuid]));
        for other in online.values_mut() {
            if other.seen.remove(&self.entity_id) {
                other.send(entity.clone());
            }
            other.send(list.clone());
        }
    }
}

/// Metadata for a player's entity, for their own client and for others: their
/// name, always shown, a player-sized box (lower while sneaking), and the
/// flags that make the client apply gravity and collisions.
pub fn player_metadata(name: &str, sneaking: bool) -> EntityMetadata {
    let mut flags = vec![
        entity_flag::HAS_GRAVITY,
        entity_flag::HAS_COLLISION,
        entity_flag::BREATHING,
        entity_flag::CAN_CLIMB,
        entity_flag::SHOW_NAME,
        entity_flag::ALWAYS_SHOW_NAME,
    ];
    if sneaking {
        flags.push(entity_flag::SNEAKING);
    }
    let height = if sneaking {
        SNEAKING_HEIGHT
    } else {
        STANDING_HEIGHT
    };
    EntityMetadata(vec![
        (
            metadata_key::FLAGS,
            MetadataValue::Long(entity_flag::bits(&flags)),
        ),
        (metadata_key::NAME, MetadataValue::String(name.to_owned())),
        (metadata_key::SCALE, MetadataValue::Float(1.0)),
        (metadata_key::WIDTH, MetadataValue::Float(0.6)),
        (metadata_key::HEIGHT, MetadataValue::Float(height)),
        (metadata_key::ALWAYS_SHOW_NAME_TAG, MetadataValue::Byte(1)),
    ])
}

/// Half a player's width: their box is 0.6 blocks wide around their feet.
pub const HALF_WIDTH: f32 = 0.3;

/// Whether a player box standing on `feet`, `height` tall, overlaps the block
/// at `pos`. Touching a face does not count, so standing on a block is fine.
pub fn body_overlaps(feet: Vec3, height: f32, pos: BlockPos) -> bool {
    let overlaps =
        |low: f32, high: f32, block: i32| low < (block + 1) as f32 && high > block as f32;
    overlaps(feet.x - HALF_WIDTH, feet.x + HALF_WIDTH, pos.x)
        && overlaps(feet.y, feet.y + height, pos.y)
        && overlaps(feet.z - HALF_WIDTH, feet.z + HALF_WIDTH, pos.z)
}

/// A player's height standing and sneaking, in blocks.
pub const STANDING_HEIGHT: f32 = 1.8;

/// How long others see a dead player lying there before they disappear: about
/// as long as the death animation.
const DEATH_ANIMATION_TICKS: u64 = 20;
pub const SNEAKING_HEIGHT: f32 = 1.5;

fn encode(packet: &impl Encode) -> Bytes {
    Bytes::from(packet.encode())
}

fn unique_id(entity_id: u64) -> i64 {
    i64::try_from(entity_id).expect("entity IDs stay far below i64::MAX")
}

fn list_entry(entity_id: u64, profile: &Profile) -> PlayerListEntry {
    PlayerListEntry {
        uuid: profile.uuid,
        entity_unique_id: unique_id(entity_id),
        username: profile.name.clone(),
        skin: Skin::clone(&profile.skin),
    }
}

fn add_player(entity_id: u64, player: &Online) -> AddPlayer {
    let movement = &player.movement;
    AddPlayer {
        uuid: player.profile.uuid,
        username: player.profile.name.clone(),
        entity_runtime_id: entity_id,
        position: movement.feet(),
        pitch: movement.pitch,
        yaw: movement.yaw,
        head_yaw: movement.head_yaw,
        game_mode: player.game_mode.id(),
        metadata: player_metadata(&player.profile.name, player.sneaking),
        held_item: player.held.0,
        entity_unique_id: unique_id(entity_id),
    }
}

fn worn_armor(entity_id: u64, player: &Online) -> MobArmorEquipment {
    MobArmorEquipment {
        entity_runtime_id: entity_id,
        armor: player.armor,
        body: ItemInstance::EMPTY,
    }
}

fn held_item(entity_id: u64, player: &Online) -> MobEquipment {
    let (item, slot) = player.held;
    MobEquipment {
        entity_runtime_id: entity_id,
        item,
        inventory_slot: slot,
        hotbar_slot: slot,
        window_id: INVENTORY_WINDOW as u8,
    }
}

#[cfg(test)]
mod tests {
    use bedrockrs_protocol::packet::{self, id};
    use bedrockrs_protocol::packets::TextType;

    use super::*;

    const SPAWN_EYES: Vec3 = Vec3 {
        x: 8.5,
        y: -60.0 + EYE_HEIGHT,
        z: 8.5,
    };

    fn standing_at(position: Vec3) -> Movement {
        Movement {
            position,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            on_ground: true,
        }
    }

    fn view_at(x: i32, z: i32) -> View {
        View {
            centre: ChunkPos::new(x, z),
            radius: 8,
        }
    }

    fn joining(players: &Players, name: &str) -> (Joining, mpsc::Receiver<Bytes>) {
        let (outbound, queue) = mpsc::channel(16);
        let joining = Joining {
            entity_id: players.allocate_entity_id(),
            profile: Profile {
                name: name.into(),
                uuid: Uuid::new_v4(),
                skin: placeholder_skin(Uuid::nil()),
            },
            movement: standing_at(SPAWN_EYES),
            view: view_at(0, 0),
            inventory: SavedInventory::default(),
            held: (ItemInstance::EMPTY, 0),
            armor: [ItemInstance::EMPTY; 4],
            game_mode: GameMode::Creative,
            health: 20.0,
            outbound,
        };
        (joining, queue)
    }

    fn ids(queue: &mut mpsc::Receiver<Bytes>) -> Vec<u32> {
        std::iter::from_fn(|| queue.try_recv().ok())
            .map(|packet| packet::read_header(&packet).unwrap().0.id)
            .collect()
    }

    fn text(packet: &[u8]) -> Text {
        let (header, payload) = packet::read_header(packet).unwrap();
        assert_eq!(header.id, id::TEXT);
        packet::decode(payload).unwrap()
    }

    #[test]
    fn players_in_view_see_each_other_join_move_and_leave() {
        let players = Players::new();
        let (steve, mut steve_queue) = joining(&players, "Steve");
        let (alex, mut alex_queue) = joining(&players, "Alex");

        let steve = players.join(steve);
        assert!(ids(&mut steve_queue).is_empty(), "nobody else was online");
        let alex = players.join(alex);
        assert_eq!(ids(&mut alex_queue), [id::PLAYER_LIST]);
        assert_eq!(ids(&mut steve_queue), [id::PLAYER_LIST]);

        // Entities appear on the next tick, carrying their position.
        players.tick(1, &[]);
        assert_eq!(ids(&mut alex_queue), [id::ADD_PLAYER, id::PLAYER_SKIN]);
        assert_eq!(ids(&mut steve_queue), [id::ADD_PLAYER, id::PLAYER_SKIN]);

        // Moves go to those who see the mover, never to the mover.
        steve.moved(standing_at(Vec3 {
            x: 9.0,
            ..SPAWN_EYES
        }));
        players.tick(2, &[]);
        assert_eq!(ids(&mut alex_queue), [id::MOVE_PLAYER]);
        assert!(ids(&mut steve_queue).is_empty());
        players.tick(3, &[]);
        assert!(ids(&mut alex_queue).is_empty(), "each move is sent once");

        drop(steve);
        assert_eq!(ids(&mut alex_queue), [id::REMOVE_ACTOR, id::PLAYER_LIST]);
        assert_eq!(players.count(), 1);
        drop(alex);
        assert_eq!(players.count(), 0);
    }

    #[test]
    fn entities_follow_view_distance_both_ways() {
        let players = Players::new();
        let (steve, mut steve_queue) = joining(&players, "Steve");
        let (alex, mut alex_queue) = joining(&players, "Alex");
        let steve = players.join(steve);
        let _alex = players.join(alex);
        players.tick(1, &[]);
        ids(&mut steve_queue);
        ids(&mut alex_queue);

        // Steve flies 20 chunks east, beyond Alex's radius of 8. His own view
        // moves with him, so Alex leaves his view too.
        let far = Vec3 {
            x: 20.0 * 16.0 + 8.5,
            ..SPAWN_EYES
        };
        steve.moved(standing_at(far));
        steve.viewing(view_at(20, 0));
        players.tick(2, &[]);
        assert_eq!(ids(&mut alex_queue), [id::REMOVE_ACTOR]);
        assert_eq!(ids(&mut steve_queue), [id::REMOVE_ACTOR]);

        // Moving out there is not sent to Alex, who cannot see him.
        steve.moved(standing_at(Vec3 { z: 20.0, ..far }));
        players.tick(3, &[]);
        assert!(ids(&mut alex_queue).is_empty());

        // Flying back into Alex's view shows him again, although Alex never moved.
        steve.moved(standing_at(SPAWN_EYES));
        steve.viewing(view_at(0, 0));
        players.tick(4, &[]);
        assert_eq!(ids(&mut alex_queue), [id::ADD_PLAYER, id::PLAYER_SKIN]);
        assert_eq!(ids(&mut steve_queue), [id::ADD_PLAYER, id::PLAYER_SKIN]);
    }

    #[test]
    fn sneaking_and_swings_reach_those_who_see_the_player() {
        let players = Players::new();
        let (steve, mut steve_queue) = joining(&players, "Steve");
        let (alex, mut alex_queue) = joining(&players, "Alex");
        let steve = players.join(steve);
        let _alex = players.join(alex);
        players.tick(1, &[]);
        ids(&mut steve_queue);
        ids(&mut alex_queue);

        // Sneaking: everyone who sees Steve, and Steve himself, get his data.
        steve.sneaking(true);
        assert_eq!(ids(&mut alex_queue), [id::SET_ACTOR_DATA]);
        assert_eq!(ids(&mut steve_queue), [id::SET_ACTOR_DATA]);
        steve.sneaking(true);
        assert!(ids(&mut alex_queue).is_empty(), "no change, nothing sent");

        // Swinging: only the others; Steve's client animates itself.
        steve.swing();
        assert_eq!(ids(&mut alex_queue), [id::ANIMATE]);
        assert!(ids(&mut steve_queue).is_empty());
    }

    #[test]
    fn held_items_reach_those_who_see_the_player_and_newcomers() {
        let players = Players::new();
        let (steve, mut steve_queue) = joining(&players, "Steve");
        let (alex, mut alex_queue) = joining(&players, "Alex");
        let steve_id = steve.entity_id;
        let steve = players.join(steve);
        let _alex = players.join(alex);
        players.tick(1, &[]);
        ids(&mut steve_queue);
        ids(&mut alex_queue);

        // Only the others; Steve's client knows what he holds.
        let torch = ItemInstance {
            network_id: 50,
            count: 1,
            ..ItemInstance::EMPTY
        };
        steve.holding(torch, 3);
        assert_eq!(ids(&mut alex_queue), [id::MOB_EQUIPMENT]);
        assert!(ids(&mut steve_queue).is_empty());

        // Someone who starts seeing Steve later gets his entity, his skin
        // and what he holds.
        let (sam, mut sam_queue) = joining(&players, "Sam");
        let _sam = players.join(sam);
        ids(&mut sam_queue);
        players.tick(2, &[]);
        // Sam sees Alex and Steve, in either order; only Steve holds something.
        let sent = ids(&mut sam_queue);
        let spawns: Vec<_> = sent
            .split(|id| *id == id::ADD_PLAYER)
            .skip(1)
            .map(<[u32]>::to_vec)
            .collect();
        assert_eq!(spawns.len(), 2, "{sent:?}");
        assert!(spawns.contains(&vec![id::PLAYER_SKIN, id::MOB_EQUIPMENT]));
        assert!(spawns.contains(&vec![id::PLAYER_SKIN]));
        let online = players.online();
        assert_eq!(add_player(steve_id, &online[&steve_id]).held_item, torch);
    }

    #[test]
    fn saved_players_keep_their_feet_and_rotation() {
        let movement = Movement {
            position: Vec3 {
                x: 12.25,
                y: -60.0 + EYE_HEIGHT,
                z: -3.5,
            },
            pitch: 15.0,
            yaw: 90.0,
            head_yaw: 80.0,
            on_ground: true,
        };
        let saved = movement.saved(false);
        // Files hold the feet, as a person would read coordinates.
        assert_eq!((saved.x, saved.y, saved.z), (12.25, -60.0, -3.5));
        assert_eq!(Movement::from_saved(&saved), movement);
    }

    #[test]
    fn bodies_overlap_blocks_they_are_in_but_not_ones_they_touch() {
        let feet = Vec3 {
            x: 8.5,
            y: -60.0,
            z: 8.5,
        };
        // Feet and head blocks, standing and sneaking.
        assert!(body_overlaps(
            feet,
            STANDING_HEIGHT,
            BlockPos { x: 8, y: -60, z: 8 }
        ));
        assert!(body_overlaps(
            feet,
            STANDING_HEIGHT,
            BlockPos { x: 8, y: -59, z: 8 }
        ));
        assert!(body_overlaps(
            feet,
            SNEAKING_HEIGHT,
            BlockPos { x: 8, y: -59, z: 8 }
        ));
        // The block stood on, the one above the head, and the neighbours.
        assert!(!body_overlaps(
            feet,
            STANDING_HEIGHT,
            BlockPos { x: 8, y: -61, z: 8 }
        ));
        assert!(!body_overlaps(
            feet,
            STANDING_HEIGHT,
            BlockPos { x: 8, y: -58, z: 8 }
        ));
        assert!(!body_overlaps(
            feet,
            STANDING_HEIGHT,
            BlockPos { x: 9, y: -60, z: 8 }
        ));
        // Standing near an edge puts the body in two columns.
        let at_edge = Vec3 { x: 8.9, ..feet };
        assert!(body_overlaps(
            at_edge,
            STANDING_HEIGHT,
            BlockPos { x: 9, y: -60, z: 8 }
        ));
    }

    #[test]
    fn player_metadata_makes_the_client_apply_gravity() {
        let metadata = player_metadata("Steve", false);
        let flags = metadata
            .0
            .iter()
            .find_map(|(key, value)| match (key, value) {
                (&metadata_key::FLAGS, MetadataValue::Long(flags)) => Some(*flags),
                _ => None,
            })
            .unwrap();
        assert_ne!(flags & (1 << entity_flag::HAS_GRAVITY), 0);
        assert_ne!(flags & (1 << entity_flag::HAS_COLLISION), 0);
    }

    #[test]
    fn chat_reaches_everyone_online() {
        let players = Players::new();
        let (steve, mut steve_queue) = joining(&players, "Steve");
        let (alex, mut alex_queue) = joining(&players, "Alex");
        let _steve = players.join(steve);
        let _alex = players.join(alex);
        ids(&mut steve_queue);
        ids(&mut alex_queue);

        players.chat("Steve", "hello");
        for queue in [&mut steve_queue, &mut alex_queue] {
            let text = text(&queue.try_recv().unwrap());
            assert_eq!(text.text_type, TextType::Raw);
            assert_eq!(text.message, "<Steve> hello");
        }
    }

    #[test]
    fn full_queues_and_empty_messages_are_skipped() {
        let players = Players::new();
        let (outbound, mut queue) = mpsc::channel(1);
        let (mut steve, _) = joining(&players, "Steve");
        steve.outbound = outbound;
        let _membership = players.join(steve);

        players.broadcast_message("");
        assert!(queue.try_recv().is_err());

        players.broadcast_message("one");
        players.broadcast_message("two");
        assert_eq!(text(&queue.try_recv().unwrap()).message, "one");
        assert!(queue.try_recv().is_err());
    }
}
