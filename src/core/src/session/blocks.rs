//! Breaking and placing blocks, and arm swings.

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    Animate, InventoryTransaction, PlayerAction, UseItem, player_action, use_item_action,
};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};

use crate::items::items;
use crate::placement::{self, Placing};
use crate::players::{STANDING_HEIGHT, View, body_overlaps};
use crate::server;
use crate::world::{MAX_Y, MIN_Y};

use super::{Reply, Session, SessionEvent};

/// Farthest a player may break a block from, eyes to block centre, in blocks.
/// Creative reach is about 7.5; the slack covers lag between input and movement.
const MAX_REACH: f32 = 12.0;

impl Session {
    /// Right-clicking a block with a block item places it against the clicked
    /// face. The held stack is the server's, and must be the item the client
    /// says it holds. Outside creative mode, placing uses one up. The client
    /// predicts the placement, so a refused one is undone with the block that
    /// is really there.
    pub(super) fn inventory_transaction(&mut self, transaction: InventoryTransaction) -> Reply {
        // Other transactions change items the client already moved on its
        // side; none are carried out, so it is shown what it really has.
        let use_item = match transaction {
            InventoryTransaction::UseItem(use_item) => use_item,
            InventoryTransaction::Normal(actions) => return self.hud_drop(&actions),
            InventoryTransaction::Other(_) => return Reply::send(self.inventory_sync()),
        };
        if use_item.action != use_item_action::CLICK_BLOCK || use_item.held_item.is_empty() {
            return Reply::default();
        }
        let target = use_item.target();
        match self.placed_block(&use_item, target) {
            Ok((pos, state, replacing)) => {
                tracing::debug!(player = %self.player, ?pos, block = %describe(&state), id = state.network_id(), "placing a block");
                let mut reply = Reply {
                    events: vec![
                        SessionEvent::PlacedBlock {
                            pos,
                            block: state.network_id(),
                            replacing,
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
            // double): both get what is really there, or they stay out of step.
            Err(reason) => {
                tracing::debug!(player = %self.player, ?target, slot = use_item.hotbar_slot, %reason, "refusing a placement");
                let clicked = use_item.block_position;
                Reply::send(vec![
                    server::block_update(target, self.world.block(target)).to_vec(),
                    server::block_update(clicked, self.world.block(clicked)).to_vec(),
                ])
            }
        }
    }

    /// Where this click places a block (next to the clicked one, or over a
    /// half slab it doubles), the state the placement rules give it and the
    /// block it replaces, or why it cannot be placed.
    pub(super) fn placed_block(
        &self,
        use_item: &UseItem,
        target: BlockPos,
    ) -> Result<(BlockPos, BlockState, Option<u32>), String> {
        let stack = self
            .inventory
            .hotbar(use_item.hotbar_slot)
            .filter(|stack| stack.item == use_item.held_item.network_id)
            .ok_or("the client holds something else")?;
        let item_state = items()
            .get(stack.item)
            .and_then(|item| item.block.as_ref())
            .ok_or("the held item is not a block")?;
        if let Some((pos, state, replacing)) = placement::slab_merge(
            item_state,
            use_item.block_position,
            use_item.face,
            &self.world,
        ) {
            if self.break_block(pos).is_none()
                || body_overlaps(self.movement.feet(), STANDING_HEIGHT, pos)
            {
                return Err("the slab to double is out of reach or the player is in it".into());
            }
            return Ok((pos, state, Some(replacing)));
        }
        if !self.may_place(target) {
            return Err("the spot is taken, out of reach or inside the player".into());
        }
        let placing = Placing {
            face: use_item.face,
            click: use_item.clicked_position,
            pitch: self.movement.pitch,
            yaw: self.movement.yaw,
        };
        placement::placed_state(item_state, target, &placing, &self.world)
            .map(|state| (target, state, None))
            .map_err(|err| err.to_string())
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

    /// Whether a block may go at `pos`: reachable, in a loaded chunk, into
    /// air, and not inside the player placing it.
    pub(super) fn may_place(&self, pos: BlockPos) -> bool {
        if self.break_block(pos).is_none() || self.world.block(pos) != self.world.air() {
            return false;
        }
        // A quick check against the placer's own box, from their latest
        // input; `Server::place_block` checks every player's.
        !body_overlaps(self.movement.feet(), STANDING_HEIGHT, pos)
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
        if !(MIN_Y..=MAX_Y).contains(&pos.y) || reach_squared > MAX_REACH * MAX_REACH || !in_view {
            tracing::debug!(player = %self.player, ?pos, "refusing to break an unreachable block");
            return None;
        }
        Some(SessionEvent::BrokeBlock(pos))
    }
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
