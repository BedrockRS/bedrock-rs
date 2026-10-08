//! Movement input, and the chunks streamed as the player moves.

use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    ItemStackRequest, NetworkChunkPublisherUpdate, PlayerAuthInput, input_flag, player_action,
};
use bedrockrs_protocol::types::{BlockPos, ChunkPos};

use crate::game_mode::GameMode;
use crate::players::{Movement, View};

use super::{Reply, Session, SessionEvent, Stage};

impl Session {
    /// The chunks the client shows now, for the rest of the server once the
    /// player is in the world.
    pub(super) fn view_event(&self) -> Vec<SessionEvent> {
        match self.view.centre() {
            Some(centre) if self.stage == Stage::InGame => vec![SessionEvent::Viewing(View {
                centre,
                radius: self.view.radius(),
            })],
            _ => Vec::new(),
        }
    }

    /// Centres the view on the player's chunk: a NetworkChunkPublisherUpdate,
    /// so the client renders around its new position, then every chunk in
    /// range it does not have yet.
    pub(super) fn stream_chunks(&mut self, radius: i32) -> Vec<Vec<u8>> {
        let block = BlockPos::containing(self.movement.feet());
        let centre = ChunkPos::of_block(block);
        let chunks = self.view.update(centre, radius);

        let mut packets = Vec::with_capacity(chunks.len() + 1);
        packets.push(
            NetworkChunkPublisherUpdate {
                position: block,
                radius: radius.unsigned_abs() << 4,
            }
            .encode(),
        );
        packets.extend(
            chunks
                .iter()
                .map(|chunk| self.world.chunk(chunk.x, chunk.z).encode()),
        );
        tracing::debug!(
            player = %self.player,
            centre = ?(centre.x, centre.z),
            radius,
            sent = chunks.len(),
            "streamed chunks"
        );
        packets
    }

    /// Records where the client says its player is. The client is trusted
    /// for now: movement is not validated beyond rejecting non-finite values.
    pub(super) fn auth_input(&mut self, input: PlayerAuthInput) -> Reply {
        // The dead lie still until they respawn.
        if self.is_dead() {
            return Reply::default();
        }
        // Blocks broken this tick, even when the player stands still.
        if input.block_actions_unread {
            tracing::debug!(player = %self.player, "block actions hidden behind an item stack request");
        }
        let mut events: Vec<SessionEvent> = input
            .block_actions
            .iter()
            .filter(|action| match action.action {
                // In creative mode, starting to break a block breaks it;
                // otherwise the client says when it finished, which is
                // trusted for now: breaking time is not checked.
                player_action::START_BREAK => self.game_mode.breaks_instantly(),
                player_action::PREDICT_DESTROY_BLOCK => true,
                _ => false,
            })
            .filter_map(|action| self.break_block(action.position))
            .collect();

        // What others see: the arm swinging at air or at a block, and sneaking.
        let flags = &input.input_flags;
        if flags.contains(&input_flag::MISSED_SWING) || !events.is_empty() {
            events.push(SessionEvent::Swing);
        }
        if flags.contains(&input_flag::START_SNEAKING) {
            self.sneaking = true;
            events.push(SessionEvent::Sneaking(true));
        } else if flags.contains(&input_flag::STOP_SNEAKING) {
            self.sneaking = false;
            events.push(SessionEvent::Sneaking(false));
        }

        // Flying: remembered for saving, and answered with the abilities that
        // accept it, as the client expects.
        let flying = if flags.contains(&input_flag::START_FLYING) {
            Some(true)
        } else if flags.contains(&input_flag::STOP_FLYING) {
            Some(false)
        } else {
            None
        };
        let mut packets = Vec::new();
        if let Some(flying) = flying {
            // Only modes that may fly can start; spectators cannot stop.
            self.flying = match self.game_mode {
                GameMode::Spectator => true,
                mode => flying && mode.may_fly(),
            };
            events.push(SessionEvent::Flying(self.flying));
            packets.push(self.own_abilities().encode());
        }

        // An item stack request riding along, such as a tool's durability.
        let request = input.item_stack_request.clone();
        let mut reply = self.movement_input(input);
        reply.packets.extend(packets);
        reply.events.extend(events);
        if let Some(request) = request {
            let answer = self.item_stack_request(ItemStackRequest {
                requests: vec![request],
            });
            reply.packets.extend(answer.packets);
            reply.events.extend(answer.events);
        }
        reply
    }

    pub(super) fn movement_input(&mut self, input: PlayerAuthInput) -> Reply {
        let movement = Movement {
            position: input.position,
            pitch: input.pitch,
            yaw: input.yaw,
            head_yaw: input.head_yaw,
            // Standing or walking on the ground leaves no vertical movement.
            on_ground: input.delta.y == 0.0,
        };
        let finite = movement.position.is_finite()
            && [movement.pitch, movement.yaw, movement.head_yaw]
                .iter()
                .all(|angle| angle.is_finite());
        if !finite {
            tracing::debug!(player = %self.player, ?input, "ignoring non-finite movement");
            return Reply::default();
        }
        if movement == self.movement {
            return Reply::default();
        }
        let collided = input.input_flags.contains(&input_flag::VERTICAL_COLLISION);
        let landing = self.fall(self.movement, movement, collided);
        self.movement = movement;

        // Crossing into another chunk moves the view along with the player.
        let mut events = vec![SessionEvent::Moved(movement)];
        events.extend(landing);
        let packets = if self.view.crosses_into(movement.chunk()) {
            let packets = self.stream_chunks(self.view.radius());
            events.extend(self.view_event());
            packets
        } else {
            Vec::new()
        };
        Reply {
            packets,
            events,
            ..Reply::default()
        }
    }
}
