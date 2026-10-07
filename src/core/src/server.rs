//! State every session shares, and what plugins ask of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bedrockrs_plugins::{Action, Dispatcher};
use bedrockrs_protocol::packet::Encode as _;
use bedrockrs_protocol::packets::{LevelEvent, LevelSoundEvent, UpdateBlock};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};
use bytes::Bytes;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::blocks::palette;
use crate::entities::{self, ItemEntities, ItemStack, PickedUp};
use crate::items::items;
use crate::logins::Logins;
use crate::placement;
use crate::players::Players;
use crate::storage::SavedPlayer;
use crate::world::World;

/// Ticks between saves of changed chunks: every 5 seconds.
const SAVE_INTERVAL: u64 = 100;

/// Plugin actions that may wait before plugins are told the server is busy.
pub const PLUGIN_ACTION_QUEUE: usize = 1024;

/// The world, the players in it and the plugins watching it.
pub struct Server {
    pub world: Arc<World>,
    pub players: Players,
    pub plugins: Dispatcher,
    /// Checks that players are who their login says.
    pub authenticator: Authenticator,
    /// One session per verified player.
    pub logins: Logins,
    /// Items lying in the world.
    pub items: ItemEntities,
    /// The last tick the game loop ran; 0 before the first.
    tick: AtomicU64,
}

impl Server {
    pub fn new(world: World, plugins: Dispatcher, authenticator: Authenticator) -> Self {
        Self {
            world: Arc::new(world),
            players: Players::new(),
            plugins,
            authenticator,
            logins: Logins::new(),
            items: ItemEntities::new(),
            tick: AtomicU64::new(0),
        }
    }

    pub fn current_tick(&self) -> u64 {
        self.tick.load(Ordering::Relaxed)
    }

    /// Advances the world by one tick; called by the game loop.
    pub fn tick(&self, tick: u64) {
        self.tick.store(tick, Ordering::Relaxed);
        let items = self.items.tick(&self.world, tick);
        self.players.tick(tick, &items);
        if tick.is_multiple_of(SAVE_INTERVAL) {
            self.save();
        }
    }

    /// Writes the chunks changed since the last save to disk, logging how it
    /// went. Chunks that fail to save are tried again next time.
    pub fn save(&self) {
        match self.world.save() {
            Ok(0) => {}
            Ok(saved) => tracing::debug!(saved, "saved changed chunks"),
            Err(err) => tracing::error!(%err, "failed to save the world"),
        }
        // Where everyone online is, in case the server stops without them leaving.
        for (uuid, player) in self.players.saved() {
            self.save_player(uuid, &player);
        }
    }

    /// Saves where a player is, logging a failure.
    pub fn save_player(&self, uuid: Uuid, player: &SavedPlayer) {
        if let Err(err) = self.world.save_player(uuid, player) {
            tracing::error!(%uuid, %err, "failed to save a player");
        }
    }

    /// Replaces the block at `pos` with air. Every player whose client has
    /// that chunk sees the change and the breaking particles, and hears it,
    /// and the block's item pops out. Breaking air does nothing. Returns the
    /// block broken, if any.
    pub fn break_block(&self, pos: BlockPos) -> Option<u32> {
        let broken = self.world.replace_block(pos, self.world.air())?;
        let chunk = ChunkPos::of_block(pos);
        self.players
            .send_to_viewers(chunk, &block_update(pos, self.world.air()));
        let effect = LevelEvent {
            event: LevelEvent::DESTROY_BLOCK,
            position: centre(pos),
            data: broken as i32,
        };
        self.players
            .send_to_viewers(chunk, &Bytes::from(effect.encode()));
        self.update_neighbours(pos);
        // The block's own item, whatever state it was in; a double slab is
        // two of its slab.
        let name = self.world.state_of(broken).map(|state| state.name.as_str());
        let (name, count) = match name.and_then(|name| palette().single_slab(name)) {
            Some(slab) => (Some(slab), 2),
            None => (name, 1),
        };
        let item = name
            .and_then(|name| items().by_name(name))
            .filter(|item| item.block.is_some());
        if let Some(item) = item {
            let entity_id = self.players.allocate_entity_id();
            let (position, velocity) = entities::block_drop(pos, entity_id ^ self.current_tick());
            let stack = ItemStack {
                item: item.network_id,
                count,
                metadata: 0,
            };
            self.items
                .spawn(entity_id, stack, position, velocity, entities::PICKUP_DELAY);
        }
        Some(broken)
    }

    /// Throws items a player dropped the way they look.
    pub fn throw_items(&self, feet: Vec3, pitch: f32, yaw: f32, stacks: &[ItemStack]) {
        let (position, velocity) = entities::throw(feet, pitch, yaw);
        for stack in stacks {
            let entity_id = self.players.allocate_entity_id();
            self.items.spawn(
                entity_id,
                *stack,
                position,
                velocity,
                entities::THROWN_PICKUP_DELAY,
            );
        }
    }

    /// Shows everyone who sees them the items a player picked up flying to
    /// that player. What is left of the items goes on the next tick.
    pub fn show_pickups(&self, picked: &[PickedUp], taker: u64) {
        for pickup in picked {
            self.players.show_pickup(pickup.entity_id, taker);
        }
    }

    /// Puts `block` at `pos` if it is air there and no player's body is in
    /// the way. Every player whose client has that chunk sees it and hears it
    /// placed. Returns whether it was placed; the caller undoes a refused
    /// placement the client already predicted.
    pub fn place_block(&self, pos: BlockPos, block: u32) -> bool {
        self.put_block(pos, block, None)
    }

    /// Like [`Server::place_block`], but over `replacing` rather than air, as
    /// when a slab becomes a double slab.
    pub fn place_block_over(&self, pos: BlockPos, block: u32, replacing: u32) -> bool {
        self.put_block(pos, block, Some(replacing))
    }

    fn put_block(&self, pos: BlockPos, block: u32, replacing: Option<u32>) -> bool {
        // A block inside a player traps them, and their client fights it.
        if self.players.occupies(pos) {
            return false;
        }
        let placed = match replacing {
            Some(expected) => self.world.replace_exact(pos, expected, block),
            None => self.world.place_block(pos, block),
        };
        if !placed {
            return false;
        }
        let chunk = ChunkPos::of_block(pos);
        self.players
            .send_to_viewers(chunk, &block_update(pos, block));
        let sound = LevelSoundEvent {
            sound: LevelSoundEvent::PLACE.into(),
            position: centre(pos),
            data: block as i32,
        };
        self.players
            .send_to_viewers(chunk, &Bytes::from(sound.encode()));
        self.update_neighbours(pos);
        true
    }

    /// Recomputes the blocks around `pos` whose connections or shape depend
    /// on it (fences, panes, bars, walls, stairs corners), and shows the
    /// changes to everyone who has those chunks.
    fn update_neighbours(&self, pos: BlockPos) {
        for (neighbour, state) in placement::neighbour_updates(pos, &self.world) {
            let block = state.network_id();
            if self.world.set_block(neighbour, block) {
                self.players.send_to_viewers(
                    ChunkPos::of_block(neighbour),
                    &block_update(neighbour, block),
                );
            }
        }
    }

    /// Carries out one plugin action.
    pub fn apply(&self, action: Action) {
        match action {
            Action::Broadcast(message) => {
                // Logged so the server log shows each chat line exactly as sent.
                tracing::info!(target: "chat", "[broadcast] {message}");
                self.players.broadcast_message(&message);
            }
            Action::SendMessage { player, message } => {
                let Some(uuid) = plugin_player(&player) else {
                    return;
                };
                match self.players.name_of(uuid) {
                    Some(name) if self.players.send_message(uuid, &message) => {
                        tracing::info!(target: "chat", "[to {name}] {message}");
                    }
                    _ => tracing::debug!(%uuid, "a plugin messaged a player who is not online"),
                }
            }
            Action::Kick { player, reason } => {
                let Some(uuid) = plugin_player(&player) else {
                    return;
                };
                if self.logins.kick(uuid, reason.clone()) {
                    tracing::info!(%uuid, %reason, "a plugin kicked a player");
                } else {
                    tracing::debug!(%uuid, "a plugin kicked a player who is not online");
                }
            }
        }
    }
}

/// The UUID a plugin named a player by, logging a malformed one.
fn plugin_player(uuid: &str) -> Option<Uuid> {
    let parsed = Uuid::parse_str(uuid).ok();
    if parsed.is_none() {
        tracing::warn!(
            uuid,
            "a plugin named a player by something that is not a UUID"
        );
    }
    parsed
}

/// An encoded UpdateBlock setting `pos` to `block`.
pub fn block_update(pos: BlockPos, block: u32) -> Bytes {
    let update = UpdateBlock {
        position: pos,
        block,
        flags: UpdateBlock::NETWORK,
        layer: 0,
    };
    Bytes::from(update.encode())
}

/// The middle of a block, where its particles and sounds come from.
fn centre(pos: BlockPos) -> Vec3 {
    Vec3 {
        x: pos.x as f32 + 0.5,
        y: pos.y as f32 + 0.5,
        z: pos.z as f32 + 0.5,
    }
}

/// Carries out plugin actions as they arrive, until the plugins stop.
pub async fn apply_plugin_actions(server: Arc<Server>, mut actions: mpsc::Receiver<Action>) {
    while let Some(action) = actions.recv().await {
        server.apply(action);
    }
}

#[cfg(test)]
mod tests {
    use bedrockrs_protocol::packet::{self, id};
    use bedrockrs_protocol::packets::DisconnectReason;
    use uuid::Uuid;

    use super::*;
    use crate::players::{EYE_HEIGHT, Joining, Movement, Profile, View};
    use crate::world::World;

    fn join_at<'a>(
        server: &'a Server,
        name: &str,
        chunk: ChunkPos,
    ) -> (crate::players::Membership<'a>, mpsc::Receiver<Bytes>) {
        let (outbound, queue) = mpsc::channel(16);
        let membership = server.players.join(Joining {
            entity_id: server.players.allocate_entity_id(),
            profile: Profile {
                name: name.into(),
                uuid: Uuid::new_v4(),
                skin: crate::players::placeholder_skin(Uuid::nil()),
            },
            movement: Movement {
                position: bedrockrs_protocol::types::Vec3 {
                    x: chunk.x as f32 * 16.0 + 8.0,
                    y: -60.0 + EYE_HEIGHT,
                    z: chunk.z as f32 * 16.0 + 8.0,
                },
                pitch: 0.0,
                yaw: 0.0,
                head_yaw: 0.0,
                on_ground: true,
            },
            view: View {
                centre: chunk,
                radius: 4,
            },
            inventory: Default::default(),
            held: (bedrockrs_protocol::packets::ItemInstance::EMPTY, 0),
            outbound,
        });
        (membership, queue)
    }

    #[test]
    fn broken_blocks_reach_players_who_have_the_chunk() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        let (_near, mut near) = join_at(&server, "Near", ChunkPos::new(0, 0));
        let (_far, mut far) = join_at(&server, "Far", ChunkPos::new(100, 0));
        while near.try_recv().is_ok() {}
        while far.try_recv().is_ok() {}

        let grass = BlockPos { x: 9, y: -61, z: 8 };
        server.break_block(grass);
        assert_eq!(server.world.block(grass), server.world.air());
        // The change, then the breaking particles and sound.
        assert_eq!(ids(&mut near), [id::UPDATE_BLOCK, id::LEVEL_EVENT]);
        assert!(
            ids(&mut far).is_empty(),
            "the chunk is not loaded out there"
        );

        // Breaking air again changes nothing, so nothing is sent.
        server.break_block(grass);
        assert!(ids(&mut near).is_empty());
    }

    #[test]
    fn placed_blocks_need_air_and_are_heard() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        let (_near, mut near) = join_at(&server, "Near", ChunkPos::new(0, 0));
        ids(&mut near);
        let stone = bedrockrs_protocol::block::BlockState::new("minecraft:stone").network_id();

        let above_grass = BlockPos { x: 9, y: -60, z: 8 };
        assert!(server.place_block(above_grass, stone));
        assert_eq!(server.world.block(above_grass), stone);
        assert_eq!(ids(&mut near), [id::UPDATE_BLOCK, id::LEVEL_SOUND_EVENT]);

        assert!(!server.place_block(above_grass, stone), "occupied");
        assert!(ids(&mut near).is_empty());
    }

    #[test]
    fn blocks_are_never_placed_inside_any_player() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        // Another player stands in the middle of chunk (0, 0), feet at (8, -60, 8).
        let (_other, mut other) = join_at(&server, "Other", ChunkPos::new(0, 0));
        ids(&mut other);
        let stone = bedrockrs_protocol::block::BlockState::new("minecraft:stone").network_id();

        // Their feet and their head are both off limits.
        for occupied in [
            BlockPos { x: 8, y: -60, z: 8 },
            BlockPos { x: 8, y: -59, z: 8 },
        ] {
            assert!(!server.place_block(occupied, stone), "{occupied:?}");
            assert_eq!(server.world.block(occupied), server.world.air());
        }
        assert!(ids(&mut other).is_empty(), "nothing changed");

        // Beside them and above their head is fine.
        assert!(server.place_block(BlockPos { x: 9, y: -60, z: 8 }, stone));
        assert!(server.place_block(BlockPos { x: 8, y: -58, z: 8 }, stone));
    }

    #[test]
    fn plugins_message_and_kick_single_players() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        let (steve, mut steve_queue) = join_at(&server, "Steve", ChunkPos::new(0, 0));
        let (_alex, mut alex_queue) = join_at(&server, "Alex", ChunkPos::new(0, 0));
        ids(&mut steve_queue);
        ids(&mut alex_queue);
        let steve_uuid = server
            .players
            .saved()
            .into_iter()
            .map(|(uuid, _)| uuid)
            .find(|uuid| server.players.name_of(*uuid).as_deref() == Some("Steve"))
            .unwrap();

        server.apply(Action::SendMessage {
            player: steve_uuid.to_string(),
            message: "psst".into(),
        });
        assert_eq!(ids(&mut steve_queue), [id::TEXT]);
        assert!(ids(&mut alex_queue).is_empty(), "only Steve hears it");
        // Unknown players and malformed UUIDs are ignored.
        server.apply(Action::SendMessage {
            player: Uuid::new_v4().to_string(),
            message: "psst".into(),
        });
        server.apply(Action::SendMessage {
            player: "Steve".into(),
            message: "psst".into(),
        });
        assert!(ids(&mut steve_queue).is_empty());

        let (kick, mut kicks) = mpsc::channel(1);
        let _claim = server.logins.claim(steve_uuid, kick);
        server.apply(Action::Kick {
            player: steve_uuid.to_string(),
            reason: "Bye".into(),
        });
        let notice = kicks.try_recv().unwrap();
        assert_eq!(notice.reason, DisconnectReason::KICKED);
        assert_eq!(notice.message, "Bye");
        drop(steve);
    }

    fn ids(queue: &mut mpsc::Receiver<Bytes>) -> Vec<u32> {
        std::iter::from_fn(|| queue.try_recv().ok())
            .map(|packet| packet::read_header(&packet).unwrap().0.id)
            .collect()
    }
}
