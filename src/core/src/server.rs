//! State every session shares, and what plugins ask of it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bedrockrs_plugins::{Action, Dispatcher};
use bedrockrs_protocol::block::BlockState;
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
use crate::support;
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

    /// Replaces the block at `pos` with air, with the other half of a door,
    /// bed or tall flower. Every player whose client has that chunk sees the
    /// change and the breaking particles, and hears it, and, if `game_mode`
    /// collects broken blocks and `dotiledrops` is on, the block's item pops
    /// out, once. Blocks that stood on or hung from it pop off too. Breaking
    /// air does nothing. Returns the block broken, if any.
    pub fn break_block(&self, pos: BlockPos, game_mode: GameMode) -> Option<u32> {
        let broken = self.remove_block(pos)?;
        let state = self.world.state_of(broken).cloned();
        let mut changed = vec![pos];
        if let Some(state) = &state
            && let Some((other_pos, _)) = support::partner(state, pos)
            && self
                .world
                .block_state(other_pos)
                .is_some_and(|other| other.name == state.name)
            && self.remove_block(other_pos).is_some()
        {
            changed.push(other_pos);
        }
        if game_mode.drops_broken_blocks()
            && let Some(state) = &state
        {
            self.drop_block(pos, state);
        }
        self.settle(&changed);
        Some(broken)
    }

    /// Sets `pos` to air for everyone with its chunk, with the breaking
    /// particles and sound. Returns the block that was there, if any.
    fn remove_block(&self, pos: BlockPos) -> Option<u32> {
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
        Some(broken)
    }

    /// Pops the item of a broken block out at `pos`, if `dotiledrops` is on:
    /// the item that places it (a door for either half, seeds for a crop, a
    /// torch for a wall torch); a double slab is two of its slab.
    fn drop_block(&self, pos: BlockPos, state: &BlockState) {
        if !self.game_rules.values().dotiledrops {
            return;
        }
        let (item, count) = match palette().single_slab(&state.name) {
            Some(slab) => (items().by_name(slab), 2),
            None => (items().pick(&state.name), 1),
        };
        let Some(item) = item else { return };
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

    /// After the blocks at `changed` changed: their neighbours' connections
    /// and shapes follow, and neighbours left without support (a torch whose
    /// wall is gone, the top of a door whose bottom broke) pop off, dropping
    /// their item, and so on outwards.
    fn settle(&self, changed: &[BlockPos]) {
        let mut queue: VecDeque<BlockPos> = changed.iter().copied().collect();
        // A long ladder or tall cactus pops block by block; a bound keeps a
        // mistake in the rules from running away.
        let mut budget = 4096;
        while let Some(pos) = queue.pop_front() {
            // A neighbour whose shape changed changes what its own neighbours
            // should be in turn: a wall gaining a connection makes the wall
            // under it taller (found live on 2026-10-08).
            for changed in self.update_neighbours(pos) {
                if budget == 0 {
                    tracing::warn!(?changed, "stopped updating neighbouring blocks");
                    return;
                }
                budget -= 1;
                queue.push_back(changed);
            }
            for face in 0..6 {
                let neighbour = placement::side(pos, face);
                let Some(state) = self.world.block_state(neighbour).cloned() else {
                    continue;
                };
                if state.name == "minecraft:air"
                    || support::supported(&state, neighbour, &*self.world)
                {
                    continue;
                }
                if budget == 0 {
                    tracing::warn!(?neighbour, "stopped popping unsupported blocks");
                    return;
                }
                budget -= 1;
                if self.remove_block(neighbour).is_some() {
                    if support::drops_item(&state) {
                        self.drop_block(neighbour, &state);
                    }
                    queue.push_back(neighbour);
                }
            }
        }
    }

    /// Opens or closes the door, trapdoor or fence gate at `pos` for a player
    /// looking at `yaw`, with both halves of a door, and the sound of it.
    /// Returns whether there was one to open.
    pub fn toggle_block(&self, pos: BlockPos, yaw: f32) -> bool {
        let Some(state) = self.world.block_state(pos).cloned() else {
            return false;
        };
        if !placement::openable(&state) {
            return false;
        }
        let toggled = placement::toggled(&state, yaw);
        let mut changes = vec![(pos, toggled.clone())];
        if let Some((other_pos, _)) = support::partner(&state, pos)
            && let Some(other) = self.world.block_state(other_pos)
            && other.name == state.name
        {
            changes.push((other_pos, placement::toggled(other, yaw)));
        }
        for (at, new) in &changes {
            let block = new.network_id();
            if self.world.set_block(*at, block) {
                self.players
                    .send_to_viewers(ChunkPos::of_block(*at), &block_update(*at, block));
            }
        }
        let name = support::short(&state.name);
        let kind = if name.ends_with("trapdoor") {
            "trapdoor"
        } else if name.ends_with("fence_gate") {
            "fence_gate"
        } else {
            "door"
        };
        let open = support::int(&toggled, "open_bit") == Some(1);
        let sound = LevelSoundEvent {
            sound: format!("{kind}.{}", if open { "open" } else { "close" }),
            position: centre(pos),
            data: toggled.network_id() as i32,
        };
        self.players
            .send_to_viewers(ChunkPos::of_block(pos), &Bytes::from(sound.encode()));
        let positions: Vec<BlockPos> = changes.iter().map(|(at, _)| *at).collect();
        self.settle(&positions);
        true
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
        self.place_blocks(&[(pos, block, self.world.air())])
    }

    /// Like [`Server::place_block`], but over `replacing` rather than air, as
    /// when a slab becomes a double slab.
    pub fn place_block_over(&self, pos: BlockPos, block: u32, replacing: u32) -> bool {
        self.place_blocks(&[(pos, block, replacing)])
    }

    /// Places blocks that go in together, such as the two halves of a door:
    /// each `(pos, block, replacing)` goes where `replacing` (air, or a plant
    /// it covers) still is, with no player's body in the way, or none does.
    pub fn place_blocks(&self, parts: &[(BlockPos, u32, u32)]) -> bool {
        // A block inside a player traps them, and their client fights it; a
        // torch at their feet, or a fence post beside them, is fine.
        let in_the_way = |(pos, block, _): &(BlockPos, u32, u32)| {
            self.world
                .state_of(*block)
                .is_some_and(|state| self.players.in_the_way(*pos, state))
        };
        if parts.iter().any(in_the_way) {
            return false;
        }
        for (done, (pos, block, replacing)) in parts.iter().enumerate() {
            if !self.world.replace_exact(*pos, *replacing, *block) {
                // Someone got there first: undo the parts already in.
                for (pos, block, replacing) in &parts[..done] {
                    self.world.replace_exact(*pos, *block, *replacing);
                }
                return false;
            }
        }
        for (pos, block, _) in parts {
            self.players
                .send_to_viewers(ChunkPos::of_block(*pos), &block_update(*pos, *block));
        }
        let (pos, block, _) = parts[0];
        let sound = LevelSoundEvent {
            sound: LevelSoundEvent::PLACE.into(),
            position: centre(pos),
            data: block as i32,
        };
        self.players
            .send_to_viewers(ChunkPos::of_block(pos), &Bytes::from(sound.encode()));
        let positions: Vec<BlockPos> = parts.iter().map(|(pos, _, _)| *pos).collect();
        self.settle(&positions);
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
    /// on it, shows the changes to everyone who has those chunks, and
    /// returns where blocks changed.
    fn update_neighbours(&self, pos: BlockPos) -> Vec<BlockPos> {
        let mut changed = Vec::new();
        for (neighbour, state) in placement::neighbour_updates(pos, &self.world) {
            let block = state.network_id();
            if self.world.set_block(neighbour, block) {
                self.players.send_to_viewers(
                    ChunkPos::of_block(neighbour),
                    &block_update(neighbour, block),
                );
                changed.push(neighbour);
            }
        }
        changed
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

    use bedrockrs_protocol::block::StateValue;

    use super::*;
    use crate::game_rules;
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

    fn state(name: &str, states: &[(&str, StateValue)]) -> BlockState {
        let mut state = palette()
            .upgrade(&BlockState::new(format!("minecraft:{name}")))
            .unwrap();
        for (key, value) in states {
            let (_, slot) = state.states.iter_mut().find(|(k, _)| k == key).unwrap();
            *slot = value.clone();
        }
        state
    }

    fn test_server() -> Server {
        Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        )
    }

    #[test]
    fn blocks_left_without_support_pop_off() {
        let server = test_server();
        let wall = BlockPos { x: 3, y: -55, z: 3 };
        let ladder_at = BlockPos { x: 3, y: -55, z: 4 };
        let stone = state("stone", &[]).network_id();
        // Facing south, fixed to the stone on its north.
        let ladder = state("ladder", &[("facing_direction", StateValue::Int(3))]).network_id();
        assert!(server.place_block(wall, stone));
        assert!(server.place_block(ladder_at, ladder));

        // Even a creative break pops the ladder, as an item.
        server.break_block(wall, GameMode::Creative);
        assert_eq!(server.world.block(ladder_at), server.world.air());
        assert_eq!(server.items.count(), 1);

        // Without tile drops, it pops and drops nothing.
        server.game_rules.set(game_rules::Rule::DoTileDrops, false);
        assert!(server.place_block(wall, stone));
        assert!(server.place_block(ladder_at, ladder));
        server.break_block(wall, GameMode::Survival);
        assert_eq!(server.world.block(ladder_at), server.world.air());
        assert_eq!(server.items.count(), 1, "nothing new");
    }

    #[test]
    fn doors_break_open_and_close_as_one() {
        let server = test_server();
        let lower_at = BlockPos { x: 3, y: -60, z: 3 };
        let upper_at = BlockPos { x: 3, y: -59, z: 3 };
        let lower = state("wooden_door", &[]);
        let upper = state("wooden_door", &[("upper_block_bit", StateValue::Byte(1))]);
        let air = server.world.air();
        let place = || {
            server.place_blocks(&[
                (lower_at, lower.network_id(), air),
                (upper_at, upper.network_id(), air),
            ])
        };
        assert!(place());

        // Opening either half opens both.
        assert!(server.toggle_block(upper_at, 0.0));
        let opened = |pos| {
            let state = server.world.block_state(pos).unwrap();
            support::int(state, "open_bit") == Some(1)
        };
        assert!(opened(lower_at) && opened(upper_at));

        // Breaking the top breaks the bottom, for one door.
        server.break_block(upper_at, GameMode::Survival);
        assert_eq!(server.world.block(lower_at), air);
        assert_eq!(server.items.count(), 1);

        // Taking the floor away pops it, for one door again.
        assert!(place());
        server.break_block(BlockPos { x: 3, y: -61, z: 3 }, GameMode::Creative);
        assert_eq!(server.world.block(lower_at), air);
        assert_eq!(server.world.block(upper_at), air);
        assert_eq!(server.items.count(), 2);
    }

    #[test]
    fn stacked_walls_settle_whatever_the_order() {
        // Pillars three high, and between them walls two high and three
        // wide, placed bottom row first, then top row, left to right.
        let server = test_server();
        let place = |x: i32, y: i32, name: &str| {
            let item = items().by_name(name).unwrap().block.clone().unwrap();
            let placing = crate::placement::Placing {
                face: 1,
                click: Vec3 {
                    x: 0.5,
                    y: 1.0,
                    z: 0.5,
                },
                pitch: 10.0,
                yaw: 0.0,
            };
            let pos = BlockPos { x, y, z: 0 };
            let state =
                crate::placement::placed_state(&item, pos, &placing, &server.world).unwrap();
            assert!(
                server.place_block(pos, state.network_id()),
                "{name} at {pos:?}"
            );
        };
        for y in -60..=-58 {
            place(0, y, "minecraft:cobblestone");
            place(4, y, "minecraft:cobblestone");
        }
        for y in [-60, -59] {
            for x in 1..=3 {
                place(x, y, "minecraft:cobblestone_wall");
            }
        }
        // Every wall is as its neighbours make it: nothing left to update.
        for y in [-60, -59] {
            for x in 1..=3 {
                let pos = BlockPos { x, y, z: 0 };
                assert!(
                    crate::placement::neighbour_updates(
                        BlockPos { x, y: y + 1, z: 0 },
                        &server.world
                    )
                    .iter()
                    .chain(&crate::placement::neighbour_updates(
                        BlockPos { x: x + 1, y, z: 0 },
                        &server.world
                    ))
                    .all(|(at, _)| *at != pos),
                    "{pos:?} is stale: {:?}",
                    server.world.block_state(pos)
                );
            }
        }
        // The bottom row, under walls running the same way, is tall.
        let bottom = server
            .world
            .block_state(BlockPos { x: 1, y: -60, z: 0 })
            .unwrap();
        assert_eq!(
            support::text(bottom, "wall_connection_type_east"),
            Some("tall")
        );
        assert_eq!(
            support::text(bottom, "wall_connection_type_west"),
            Some("tall")
        );
        assert_eq!(support::int(bottom, "wall_post_bit"), Some(0));
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
