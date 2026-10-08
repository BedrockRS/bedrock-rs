//! State every session shares, and what plugins ask of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bedrockrs_plugins::{Action, Dispatcher};
use bedrockrs_protocol::packet::Encode as _;
use bedrockrs_protocol::packets::{LevelEvent, LevelSoundEvent, UpdateBlock};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};
use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::blocks::palette;
use crate::commands::Commands;
use crate::damage::DamageCause;
use crate::entities::{self, ItemEntities, ItemStack, PickedUp};
use crate::game_mode::GameMode;
use crate::game_rules::GameRules;
use crate::items::items;
use crate::logins::{Control, Logins};
use crate::ops::Operators;
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
    /// Every slash command, built-in and from plugins.
    pub commands: Commands,
    /// Players who may run operator commands.
    pub ops: Operators,
    /// The game mode of players who have not played here before.
    pub default_game_mode: GameMode,
    /// The world's game rules.
    pub game_rules: GameRules,
    /// The last tick the game loop ran; 0 before the first.
    tick: AtomicU64,
    /// Signalled when something asks the server to stop.
    stop: Notify,
    /// Set once the server disconnects everyone to stop.
    closing: AtomicBool,
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
            commands: Commands::new(),
            ops: Operators::in_memory(),
            default_game_mode: GameMode::Creative,
            game_rules: GameRules::in_memory(),
            tick: AtomicU64::new(0),
            stop: Notify::new(),
            closing: AtomicBool::new(false),
        }
    }

    /// Keeps operators in `ops` rather than in memory.
    pub fn with_operators(mut self, ops: Operators) -> Self {
        self.ops = ops;
        self
    }

    pub fn with_default_game_mode(mut self, mode: GameMode) -> Self {
        self.default_game_mode = mode;
        self
    }

    /// Keeps game rules in `rules` rather than in memory.
    pub fn with_game_rules(mut self, rules: GameRules) -> Self {
        self.game_rules = rules;
        self
    }

    /// Scatters what a dying player carried around where they died.
    pub fn drop_death_items(&self, feet: Vec3, stacks: &[ItemStack]) {
        for stack in stacks {
            let entity_id = self.players.allocate_entity_id();
            let (position, velocity) = entities::death_drop(feet, entity_id ^ self.current_tick());
            self.items.spawn(
                entity_id,
                *stack,
                position,
                velocity,
                entities::PICKUP_DELAY,
            );
        }
    }

    /// Tells everyone how a player died, in their own language, if the
    /// `showdeathmessages` rule allows. Logged in English either way.
    pub fn announce_death(&self, player: &str, cause: DamageCause) {
        tracing::info!(target: "chat", "{}", cause.death_message_english(player));
        if self.game_rules.values().showdeathmessages {
            self.players.broadcast_translation(
                &format!("%{}", cause.death_message()),
                &[player.to_owned()],
            );
        }
    }

    /// Asks the server to stop, as `/stop` does.
    pub fn request_stop(&self) {
        self.stop.notify_one();
    }

    /// Resolves once something asked the server to stop.
    pub async fn stop_requested(&self) {
        self.stop.notified().await;
    }

    /// Disconnects every player as the server stops. Their sessions still
    /// save them and tell plugins they quit, but nobody hears they left.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Relaxed);
        self.logins.close_all();
    }

    /// Whether the server is disconnecting everyone to stop.
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Relaxed)
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
            Err(err) => tracing::error!("Couldn't save the world: {err}"),
        }
        // Where everyone online is, in case the server stops without them leaving.
        for (uuid, player) in self.players.saved() {
            self.save_player(uuid, &player);
        }
    }

    /// Saves where a player is, logging a failure.
    pub fn save_player(&self, uuid: Uuid, player: &SavedPlayer) {
        if let Err(err) = self.world.save_player(uuid, player) {
            tracing::error!("Couldn't save player {uuid}: {err}");
        }
    }

    /// Replaces the block at `pos` with air. Every player whose client has
    /// that chunk sees the change and the breaking particles, and hears it,
    /// and, if `game_mode` collects broken blocks, the block's item pops out.
    /// Breaking air does nothing. Returns the block broken, if any.
    pub fn break_block(&self, pos: BlockPos, game_mode: GameMode) -> Option<u32> {
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
        if !game_mode.drops_broken_blocks() {
            return Some(broken);
        }
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
                nbt: None,
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

    /// Plays `sound` (a sound event, such as `armor.equip_iron`) at
    /// `position`, for everyone with its chunk.
    pub fn play_sound(&self, position: Vec3, sound: &str) {
        let packet = LevelSoundEvent {
            sound: sound.to_owned(),
            position,
            data: -1,
        };
        self.players.send_to_viewers(
            ChunkPos::of_block(BlockPos::containing(position)),
            &Bytes::from(packet.encode()),
        );
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
                let name = self.players.name_of(uuid);
                if self.logins.kick(uuid, reason.clone()) {
                    let name = name.unwrap_or_else(|| uuid.to_string());
                    tracing::info!("A plugin kicked {name}: {reason}");
                } else {
                    tracing::debug!(%uuid, "a plugin kicked a player who is not online");
                }
            }
            Action::SetGameMode { player, mode } => {
                let Some(uuid) = plugin_player(&player) else {
                    return;
                };
                let Some(mode) = GameMode::resolve(&mode, self.default_game_mode) else {
                    tracing::warn!(
                        "A plugin asked for the game mode {mode:?}, which doesn't exist"
                    );
                    return;
                };
                if !self.logins.send(uuid, Control::SetGameMode(mode)) {
                    tracing::debug!(%uuid, "a plugin set the game mode of a player who is not online");
                }
            }
            Action::SetHealth { player, health } => {
                let Some(uuid) = plugin_player(&player) else {
                    return;
                };
                if !self.logins.send(uuid, Control::SetHealth(health)) {
                    tracing::debug!(%uuid, "a plugin set the health of a player who is not online");
                }
            }
            Action::Damage {
                player,
                amount,
                cause,
            } => {
                let Some(uuid) = plugin_player(&player) else {
                    return;
                };
                let Some(cause) = DamageCause::from_name(&cause) else {
                    tracing::warn!("A plugin used the damage cause {cause:?}, which doesn't exist");
                    return;
                };
                if !self.logins.send(uuid, Control::Damage { cause, amount }) {
                    tracing::debug!(%uuid, "a plugin hurt a player who is not online");
                }
            }
            Action::SetCommands(commands) => {
                if self.commands.set_plugin_commands(commands) {
                    tracing::debug!("plugin commands changed; telling every player");
                    self.logins.send_all(&Control::RefreshCommands);
                }
            }
        }
    }
}

/// The UUID a plugin named a player by, logging a malformed one.
fn plugin_player(uuid: &str) -> Option<Uuid> {
    let parsed = Uuid::parse_str(uuid).ok();
    if parsed.is_none() {
        tracing::warn!("A plugin named a player by {uuid:?}, which isn't a UUID");
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
            armor: [bedrockrs_protocol::packets::ItemInstance::EMPTY; 4],
            game_mode: crate::game_mode::GameMode::Creative,
            health: 20.0,
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
        server.break_block(grass, GameMode::Creative);
        assert_eq!(server.world.block(grass), server.world.air());
        // The change, then the breaking particles and sound.
        assert_eq!(ids(&mut near), [id::UPDATE_BLOCK, id::LEVEL_EVENT]);
        assert!(
            ids(&mut far).is_empty(),
            "the chunk is not loaded out there"
        );

        assert_eq!(server.items.count(), 0, "creative breaks drop nothing");

        // Breaking air again changes nothing, so nothing is sent.
        server.break_block(grass, GameMode::Creative);
        assert!(ids(&mut near).is_empty());
    }

    #[test]
    fn survival_breaks_drop_the_block_item() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        let grass = BlockPos { x: 9, y: -61, z: 8 };
        assert!(server.break_block(grass, GameMode::Survival).is_some());
        assert_eq!(server.items.count(), 1);

        // Air has no item.
        server.break_block(grass, GameMode::Survival);
        assert_eq!(server.items.count(), 1);
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
        let Control::Kick(notice) = kicks.try_recv().unwrap() else {
            panic!("expected a kick");
        };
        assert_eq!(notice.reason, DisconnectReason::KICKED);
        assert_eq!(notice.message, "Bye");

        server.apply(Action::SetGameMode {
            player: steve_uuid.to_string(),
            mode: "survival".into(),
        });
        assert_eq!(
            kicks.try_recv().unwrap(),
            Control::SetGameMode(GameMode::Survival)
        );
        drop(steve);
    }

    #[test]
    fn plugin_commands_reach_every_player() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        let (controls, mut received) = mpsc::channel(4);
        let _claim = server.logins.claim(Uuid::new_v4(), controls);
        let warp = bedrockrs_plugins::PluginCommand {
            plugin: "warps".into(),
            spec: bedrockrs_plugins::CommandSpec {
                name: "warp".into(),
                description: String::new(),
                aliases: Vec::new(),
                root: bedrockrs_plugins::CommandNode::runs(),
            },
        };
        server.apply(Action::SetCommands(vec![warp.clone()]));
        assert!(server.commands.find("warp").is_some());
        assert_eq!(received.try_recv().unwrap(), Control::RefreshCommands);
        // The same commands again change nothing.
        server.apply(Action::SetCommands(vec![warp]));
        assert!(received.try_recv().is_err());
    }

    fn ids(queue: &mut mpsc::Receiver<Bytes>) -> Vec<u32> {
        std::iter::from_fn(|| queue.try_recv().ok())
            .map(|packet| packet::read_header(&packet).unwrap().0.id)
            .collect()
    }
}
