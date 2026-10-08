//! Breaking, placing and picking blocks, and arm swings.

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    Animate, BlockPickRequest, INVENTORY_WINDOW, InventoryTransaction, ItemStackResponse,
    PlayerAction, PlayerHotBar, TransactionData, UseItem, player_action, use_item_action,
};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};

use crate::entities::ItemStack;
use crate::game_mode::GameMode;
use crate::items::items;
use crate::placement::{self, Placing, Refusal};
use crate::players::{HALF_WIDTH, SNEAKING_HEIGHT, STANDING_HEIGHT, View};
use crate::server;
use crate::shape;
use crate::support;
use crate::world::{MAX_Y, MIN_Y};

use super::{Reply, Session, SessionEvent};

/// Farthest a player may break a block from, eyes to block centre, in blocks.
/// Creative reach is about 7.5; the slack covers lag between input and movement.
const MAX_REACH: f32 = 12.0;

impl Session {
    /// An inventory transaction: using the held item, or throwing it from the
    /// HUD. Other transactions change items the client already moved on its
    /// side; none are carried out, so it is shown what it really has.
    pub(super) fn inventory_transaction(&mut self, transaction: InventoryTransaction) -> Reply {
        let InventoryTransaction {
            legacy_request,
            data,
        } = transaction;
        let mut reply = match data {
            TransactionData::UseItem(use_item) => self.use_item(use_item),
            TransactionData::Normal(actions) => self.hud_drop(&actions),
            TransactionData::Other(_) => Reply::send(self.inventory_sync()),
        };
        // The client changed those slots itself and keeps them locked until
        // it hears what they hold (found live: a helmet put on by using it
        // could not be taken off).
        if let Some(request) = legacy_request {
            tracing::debug!(player = %self.player, id = request.id, slots = ?request.slots, "answering a legacy request");
            let response = ItemStackResponse {
                responses: vec![self.inventory.legacy_response(&request)],
            };
            reply.packets.insert(0, response.encode());
        }
        reply
    }

    /// Using the held item: in the air, armour is put on. Right-clicking a
    /// block with a block item places it against the clicked face. The held
    /// stack is the server's, and must be the item the client says it holds.
    /// Outside creative mode, placing uses one up. The client predicts the
    /// placement, so a refused one is undone with the block that is really
    /// there.
    fn use_item(&mut self, use_item: UseItem) -> Reply {
        if use_item.action == use_item_action::CLICK_AIR {
            return self.use_in_air();
        }
        if use_item.action != use_item_action::CLICK_BLOCK {
            return Reply::default();
        }
        // Using a door, trapdoor or gate opens or closes it; sneaking places
        // against it instead.
        let clicked = use_item.block_position;
        let opens = self
            .world
            .block_state(clicked)
            .is_some_and(placement::openable);
        if opens
            && !self.sneaking
            && !self.is_dead()
            && self.game_mode != GameMode::Spectator
            && self.within_reach(clicked)
        {
            return Reply {
                events: vec![
                    SessionEvent::Toggled {
                        pos: clicked,
                        yaw: self.movement.yaw,
                    },
                    SessionEvent::Swing,
                ],
                ..Reply::default()
            };
        }
        if use_item.held_item.is_empty() {
            return Reply::default();
        }
        let target = use_item.target();
        match self.placed_block(&use_item, target) {
            Ok(parts) => {
                let (pos, state, replacing) = parts[0].clone();
                tracing::debug!(player = %self.player, ?pos, block = %describe(&state), id = state.network_id(), parts = parts.len(), "placing a block");
                let air = self.world.air();
                let mut reply = Reply {
                    events: vec![
                        SessionEvent::PlacedBlock {
                            pos,
                            block: state.network_id(),
                            replacing: (replacing != air).then_some(replacing),
                            other_half: parts.get(1).map(|(pos, state, replacing)| {
                                (*pos, state.network_id(), *replacing)
                            }),
                        },
                        SessionEvent::Swing,
                    ],
                    ..Reply::default()
                };
                // The item is used up even if someone else fills the spot
                // before the block goes in, which is rare.
                if !self.game_mode.is_creative()
                    && let Some(slot) = self.inventory.use_one(use_item.hotbar_slot)
                {
                    reply.packets.push(slot.encode());
                    reply
                        .events
                        .push(SessionEvent::InventoryChanged(self.inventory.saved()));
                    reply.events.extend(self.held_event());
                }
                reply
            }
            // Whatever the reason, the client already shows its prediction,
            // next to the clicked block or in it (a slab it expected to
            // double, grass it covered), with any other half around it: all
            // get what is really there, or they stay out of step.
            Err(reason) => {
                tracing::debug!(player = %self.player, ?target, slot = use_item.hotbar_slot, %reason, "refusing a placement");
                // Second halves go above or beside, never below.
                let mut shown = vec![target, clicked];
                for face in 1..6 {
                    let pos = placement::side(target, face);
                    if !shown.contains(&pos) {
                        shown.push(pos);
                    }
                }
                Reply::send(
                    shown
                        .into_iter()
                        .map(|pos| server::block_update(pos, self.world.block(pos)).to_vec())
                        .collect(),
                )
            }
        }
    }

    /// The blocks this click places: where each goes (next to the clicked
    /// block, into a plant or snow it clicked, or over a half slab it
    /// doubles), its state, and the block it replaces there. Two-block blocks
    /// place both halves. Or why nothing can be placed.
    pub(super) fn placed_block(
        &self,
        use_item: &UseItem,
        target: BlockPos,
    ) -> Result<Vec<(BlockPos, BlockState, u32)>, String> {
        let stack = self
            .inventory
            .hotbar(use_item.hotbar_slot)
            .filter(|stack| stack.item == use_item.held_item.network_id)
            .ok_or("the client holds something else")?;
        let item = items().get(stack.item).ok_or("the held item is unknown")?;
        // Clicking grass, snow and the like places into it, as if on top of
        // the block below.
        let clicked = use_item.block_position;
        let into_clicked = self
            .world
            .block_state(clicked)
            .is_some_and(support::replaceable);
        let (target, face) = if into_clicked {
            (clicked, support::UP)
        } else {
            (target, use_item.face)
        };
        let item_state = items()
            .placed_block(item, (2..6).contains(&face))
            .ok_or("the held item places no block")?;
        if !into_clicked
            && let Some((pos, state, replacing)) =
                placement::slab_merge(&item_state, clicked, face, &self.world)
        {
            if self.break_block(pos).is_none() || self.body_inside(pos, &state) {
                return Err("the slab to double is out of reach or the player is in it".into());
            }
            return Ok(vec![(pos, state, replacing)]);
        }
        // What it is fixed to is the clicked face if that holds it, or else,
        // as vanilla tries each way a block can go, the floor, a wall, or the
        // ceiling around the same spot: a torch clicked onto the side of
        // another torch stands on the floor beside it.
        let fallbacks = [support::UP, 2, 3, 4, 5, support::DOWN];
        let mut first_error = None;
        for face in std::iter::once(face).chain(fallbacks.into_iter().filter(|f| *f != face)) {
            match self.placed_parts(item, target, face, use_item) {
                Ok(parts) => return Ok(parts),
                Err(Unplaced::Here(reason)) => return Err(reason),
                Err(Unplaced::Unheld(reason)) => {
                    first_error.get_or_insert(reason);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| "nothing holds it there".into()))
    }

    /// The blocks `item` places at `target` fixed to face `face` of the block
    /// beside it, or why not: either nothing can go there at all, or nothing
    /// would hold it up that way.
    fn placed_parts(
        &self,
        item: &crate::items::ItemType,
        target: BlockPos,
        face: u8,
        use_item: &UseItem,
    ) -> Result<Vec<(BlockPos, BlockState, u32)>, Unplaced> {
        let item_state = items()
            .placed_block(item, (2..6).contains(&face))
            .ok_or_else(|| Unplaced::Here("the held item places no block".into()))?;
        let placing = Placing {
            face,
            click: use_item.clicked_position,
            pitch: self.movement.pitch,
            yaw: self.movement.yaw,
        };
        let parts =
            placement::placed_parts(&item_state, target, &placing, &self.world).map_err(|err| {
                match err {
                    Refusal::OnCeiling | Refusal::WrongFace | Refusal::NoValidState => {
                        Unplaced::Unheld(err.to_string())
                    }
                    err => Unplaced::Here(err.to_string()),
                }
            })?;
        for (pos, state) in &parts {
            if !self.may_place(*pos, state) {
                return Err(Unplaced::Here(
                    "a spot is taken, out of reach or inside the player".into(),
                ));
            }
        }
        // Each part stays up where it goes: torches on walls, doors on the
        // floor, ladders on full blocks.
        let placed = support::WithParts {
            world: &self.world,
            parts: &parts,
        };
        if let Some((pos, state)) = parts
            .iter()
            .find(|(pos, state)| !support::supported(state, *pos, &placed))
        {
            return Err(Unplaced::Unheld(format!(
                "nothing would hold {} up at {pos:?}",
                state.name
            )));
        }
        // Beds also need the floor, though they stay once placed.
        let bed = support::int(&parts[0].1, "head_piece_bit").is_some();
        let floored = |pos: BlockPos| {
            self.world
                .block_state(placement::side(pos, support::DOWN))
                .is_some_and(|below| support::sturdy(below, support::UP))
        };
        if bed && !parts.iter().all(|(pos, _)| floored(*pos)) {
            return Err(Unplaced::Here(
                "a bed needs the floor under both halves".into(),
            ));
        }
        Ok(parts
            .into_iter()
            .map(|(pos, state)| {
                let replacing = self.world.block(pos);
                (pos, state, replacing)
            })
            .collect())
    }

    /// A swing the client animated: others see it too, unless it came from
    /// building, mining or dropping, which the server already shows.
    pub(super) fn animate(&mut self, animate: Animate) -> Reply {
        let shown_already = matches!(
            animate.swing_source.as_deref(),
            Some("build" | "mine" | "dropitem")
        );
        if animate.action != Animate::SWING_ARM || shown_already {
            return Reply::default();
        }
        Reply {
            events: vec![SessionEvent::Swing],
            ..Reply::default()
        }
    }

    /// Whether the player's own body is inside the collision of `state` at
    /// `pos`, from their latest input.
    fn body_inside(&self, pos: BlockPos, state: &BlockState) -> bool {
        let height = if self.sneaking {
            SNEAKING_HEIGHT
        } else {
            STANDING_HEIGHT
        };
        shape::body_inside(self.movement.feet(), HALF_WIDTH, height, pos, state)
    }

    /// Whether `state` may go at `pos`: reachable, in a loaded chunk, into
    /// air or something it replaces, and, if it blocks bodies, not inside
    /// the player placing it.
    pub(super) fn may_place(&self, pos: BlockPos, state: &BlockState) -> bool {
        let free = self
            .world
            .block_state(pos)
            .is_some_and(support::replaceable);
        if self.break_block(pos).is_none() || !free {
            return false;
        }
        // A quick check against the placer's own box, from their latest
        // input; `Server::place_blocks` checks every player's.
        !self.body_inside(pos, state)
    }

    /// A PlayerAction: creative clients report instant breaks this way too.
    pub(super) fn player_action(&mut self, action: PlayerAction) -> Reply {
        let events = match action.action {
            player_action::CREATIVE_DESTROY_BLOCK if self.game_mode.breaks_instantly() => self
                .break_block(action.block_position)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
        Reply {
            events,
            ..Reply::default()
        }
    }

    /// Checks that the player may break the block at `pos`: their game mode
    /// lets them build, and it is inside the world's height, within reach,
    /// and in a chunk their client has.
    pub(super) fn break_block(&self, pos: BlockPos) -> Option<SessionEvent> {
        if self.is_dead() {
            return None;
        }
        if !self.game_mode.may_build() {
            tracing::debug!(player = %self.player, ?pos, mode = self.game_mode.name(), "refusing to change a block in this game mode");
            return None;
        }
        if !self.within_reach(pos) {
            tracing::debug!(player = %self.player, ?pos, "refusing to break an unreachable block");
            return None;
        }
        Some(SessionEvent::BrokeBlock(pos))
    }

    /// Whether the block at `pos` is inside the world's height, within reach,
    /// and in a chunk the player's client has.
    fn within_reach(&self, pos: BlockPos) -> bool {
        let centre = Vec3 {
            x: pos.x as f32 + 0.5,
            y: pos.y as f32 + 0.5,
            z: pos.z as f32 + 0.5,
        };
        let eyes = self.movement.position;
        let reach_squared =
            (centre.x - eyes.x).powi(2) + (centre.y - eyes.y).powi(2) + (centre.z - eyes.z).powi(2);
        let in_view = self.view.centre().is_some_and(|centre| {
            View {
                centre,
                radius: self.view.radius(),
            }
            .contains(ChunkPos::of_block(pos))
        });
        (MIN_Y..=MAX_Y).contains(&pos.y) && reach_squared <= MAX_REACH * MAX_REACH && in_view
    }

    /// Picking a block (middle click) brings its item into the hotbar, as
    /// [`Inventory::pick`](crate::inventory::Inventory::pick) describes. With
    /// Ctrl held the item would also carry the block's data, but no block
    /// keeps any yet (there are no block entities).
    pub(super) fn pick_block(&mut self, request: BlockPickRequest) -> Reply {
        let pos = request.position;
        if self.is_dead() || self.game_mode == GameMode::Spectator || !self.within_reach(pos) {
            return Reply::default();
        }
        let block = self.world.block_name(self.world.block(pos));
        let Some(item) = items().pick(block) else {
            return Reply::default();
        };
        if request.add_block_nbt {
            tracing::debug!(player = %self.player, ?pos, block, "no block data to copy into a picked item");
        }
        let picked = ItemStack {
            item: item.network_id,
            count: 1,
            metadata: 0,
            nbt: None,
        };
        let creative = self.game_mode.is_creative();
        let Some((slot, changed)) = self.inventory.pick(picked, self.held_slot.into(), creative)
        else {
            return Reply::default();
        };
        self.held_slot = slot as u8;
        let mut reply = Reply::default();
        if changed {
            reply.packets = self.inventory_sync();
            reply
                .events
                .push(SessionEvent::InventoryChanged(self.inventory.saved()));
        }
        reply.packets.push(
            PlayerHotBar {
                selected_slot: u32::from(self.held_slot),
                window_id: INVENTORY_WINDOW as u8,
                select_slot: true,
            }
            .encode(),
        );
        reply.events.extend(self.held_event());
        reply
    }
}

/// Why a block cannot be placed one way.
enum Unplaced {
    /// Not there at all: the spot is taken or out of reach.
    Here(String),
    /// Not fixed to that face; another may hold it.
    Unheld(String),
}

/// A block state for logs: `minecraft:oak_stairs[upside_down_bit=0,…]`.
fn describe(state: &BlockState) -> String {
    let states: Vec<String> = state
        .states
        .iter()
        .map(|(name, value)| match value {
            StateValue::Byte(v) => format!("{name}={v}"),
            StateValue::Int(v) => format!("{name}={v}"),
            StateValue::String(v) => format!("{name}={v}"),
        })
        .collect();
    format!("{}[{}]", state.name, states.join(","))
}
