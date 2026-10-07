//! Player movement: the client's input each tick, and positions sent to others.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::packets::BlockAction;
use crate::packets::inventory::skip_embedded_use_item;
use crate::packets::item_stack::StackRequest;
use crate::types::{Vec2, Vec3};

/// PlayerAuthInput input flag IDs, as gophertunnel numbers them; Mojang's
/// PlayerActionType descriptions confirm the ones given there.
pub mod input_flag {
    pub const START_SNEAKING: i32 = 27;
    pub const STOP_SNEAKING: i32 = 28;
    /// Swinging at nothing (left-clicking air).
    pub const MISSED_SWING: i32 = 39;
    pub const START_FLYING: i32 = 42;
    pub const STOP_FLYING: i32 = 43;
}

/// Most input flags a PlayerAuthInput may list; the protocol defines about 65.
const MAX_INPUT_FLAGS: u32 = 128;

/// What the client did this tick, sent every client tick (20 per second) once
/// it is in the world, even while standing still.
///
/// Decoded up to the block actions. Before them come an optional item
/// interaction, which is stepped over, and an optional item stack request,
/// which cannot be yet: when one is present the block actions are unreachable
/// and `block_actions_unread` is set. Vehicle data after them is skipped.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerAuthInput {
    pub pitch: f32,
    pub yaw: f32,
    /// Where the player's eyes are: their feet plus 1.62 blocks.
    pub position: Vec3,
    pub move_vector: Vec2,
    pub head_yaw: f32,
    /// IDs of the input flags set this tick (sneaking, jumping, collisions…),
    /// numbered as in gophertunnel. Mojang's enum omits a few values, but the
    /// bit numbers in its descriptions match gophertunnel's.
    pub input_flags: Vec<i32>,
    pub input_mode: u32,
    pub play_mode: u32,
    pub interaction_model: i32,
    pub interact_rotation: Vec2,
    /// The client's tick counter.
    pub tick: u64,
    /// How far the player moved this tick.
    pub delta: Vec3,
    /// Block breaking progress this tick, with server-authoritative breaking.
    pub block_actions: Vec<BlockAction>,
    /// The tail after the movement could not be read, so any block actions
    /// and item stack request this tick are lost.
    pub block_actions_unread: bool,
    /// An item stack request sent along with the input, such as a tool's
    /// predicted durability while mining; it needs a response like any other.
    pub item_stack_request: Option<StackRequest>,
}

impl Packet for PlayerAuthInput {
    const ID: u32 = id::PLAYER_AUTH_INPUT;
}

impl Decode for PlayerAuthInput {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let pitch = reader.f32_le()?;
        let yaw = reader.f32_le()?;
        let position = Vec3::read(reader)?;
        let move_vector = Vec2::read(reader)?;
        let head_yaw = reader.f32_le()?;
        let count = reader.var_u32()?;
        if count > MAX_INPUT_FLAGS {
            return Err(DecodeError::InvalidValue {
                field: "input flag count",
                value: count.into(),
            });
        }
        let input_flags = (0..count)
            .map(|_| reader.var_i32())
            .collect::<Result<_, _>>()?;
        let mut input = Self {
            pitch,
            yaw,
            position,
            move_vector,
            head_yaw,
            input_flags,
            input_mode: reader.var_u32()?,
            play_mode: reader.var_u32()?,
            interaction_model: reader.var_i32()?,
            interact_rotation: Vec2::read(reader)?,
            tick: reader.var_u64()?,
            delta: Vec3::read(reader)?,
            block_actions: Vec::new(),
            block_actions_unread: false,
            item_stack_request: None,
        };
        // The rest is optional. Reading it must never cost the movement above,
        // so a tail that cannot be parsed only leaves the block actions unread.
        match read_tail(&mut reader.clone()) {
            Ok((actions, request)) => {
                input.block_actions = actions;
                input.item_stack_request = request;
            }
            Err(_) => input.block_actions_unread = true,
        }
        reader.take(reader.remaining())?;
        Ok(input)
    }
}

/// Reads past the item interaction to the item stack request and the block
/// actions.
fn read_tail(
    reader: &mut Reader<'_>,
) -> Result<(Vec<BlockAction>, Option<StackRequest>), DecodeError> {
    if reader.bool()? {
        skip_embedded_use_item(reader)?;
    }
    let request = if reader.bool()? {
        Some(StackRequest::read(reader)?)
    } else {
        None
    };
    if !reader.bool()? {
        return Ok((Vec::new(), request));
    }
    let count = reader.var_u32()?;
    if count > MAX_BLOCK_ACTIONS {
        return Err(DecodeError::InvalidValue {
            field: "block action count",
            value: count.into(),
        });
    }
    let actions = (0..count)
        .map(|_| BlockAction::read(reader))
        .collect::<Result<_, _>>()?;
    Ok((actions, request))
}

/// Most block actions a PlayerAuthInput may carry.
const MAX_BLOCK_ACTIONS: u32 = 64;

impl Encode for PlayerAuthInput {
    /// Writes the decoded fields followed by empty optional fields and zero
    /// vectors, as a client that did nothing else this tick would.
    fn encode_payload(&self, writer: &mut Writer) {
        writer.f32_le(self.pitch);
        writer.f32_le(self.yaw);
        self.position.write(writer);
        self.move_vector.write(writer);
        writer.f32_le(self.head_yaw);
        writer.var_u32(u32::try_from(self.input_flags.len()).expect("a few flags"));
        for flag in &self.input_flags {
            writer.var_i32(*flag);
        }
        writer.var_u32(self.input_mode);
        writer.var_u32(self.play_mode);
        writer.var_i32(self.interaction_model);
        self.interact_rotation.write(writer);
        writer.var_u64(self.tick);
        self.delta.write(writer);
        // No item interaction.
        writer.bool(false);
        writer.bool(self.item_stack_request.is_some());
        if let Some(request) = &self.item_stack_request {
            request.write(writer);
        }
        writer.bool(!self.block_actions.is_empty());
        if !self.block_actions.is_empty() {
            writer.var_u32(u32::try_from(self.block_actions.len()).expect("a few actions"));
            for action in &self.block_actions {
                action.write(writer);
            }
        }
        // No vehicle rotation or predicted vehicle.
        writer.bool(false);
        writer.bool(false);
        // Analogue move vector, camera orientation and raw move vector.
        Vec2::default().write(writer);
        Vec3::default().write(writer);
        Vec2::default().write(writer);
    }
}

/// How a [`MovePlayer`] moves the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveMode {
    /// Interpolated movement.
    Normal = 0,
    Reset = 1,
    Rotation = 3,
}

/// Moves a player, as seen by another client. Teleports are not supported yet.
#[derive(Debug, Clone, PartialEq)]
pub struct MovePlayer {
    pub entity_runtime_id: u64,
    /// Where the player's eyes are: their feet plus 1.62 blocks.
    pub position: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub mode: MoveMode,
    pub on_ground: bool,
    /// The entity the player rides, or 0.
    pub ridden_entity_runtime_id: u64,
    /// The server tick the movement belongs to.
    pub tick: u64,
}

impl Packet for MovePlayer {
    const ID: u32 = id::MOVE_PLAYER;
}

impl Encode for MovePlayer {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        self.position.write(writer);
        writer.f32_le(self.pitch);
        writer.f32_le(self.yaw);
        writer.f32_le(self.head_yaw);
        writer.u8(self.mode as u8);
        writer.bool(self.on_ground);
        writer.var_u64(self.ridden_entity_runtime_id);
        // No teleport data.
        writer.bool(false);
        writer.var_u64(self.tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn input() -> PlayerAuthInput {
        PlayerAuthInput {
            pitch: 10.0,
            yaw: -90.0,
            position: Vec3 {
                x: 8.5,
                y: -58.38,
                z: 8.5,
            },
            move_vector: Vec2 { x: 0.0, y: 1.0 },
            head_yaw: -90.0,
            input_flags: vec![9, 49],
            input_mode: 1,
            play_mode: 0,
            interaction_model: 0,
            interact_rotation: Vec2 { x: 10.0, y: -90.0 },
            tick: 1234,
            delta: Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.2,
            },
            block_actions: Vec::new(),
            block_actions_unread: false,
            item_stack_request: None,
        }
    }

    /// The fields up to `delta`, then `tail` as the optional part.
    fn with_tail(tail: &[u8]) -> Vec<u8> {
        let mut bytes = input().encode();
        let head = bytes.len() - (5 + 8 + 12 + 8);
        bytes.truncate(head);
        bytes.extend(tail);
        bytes
    }

    fn decode_input(bytes: &[u8]) -> PlayerAuthInput {
        let (_, payload) = read_header(bytes).unwrap();
        decode(payload).unwrap()
    }

    #[test]
    fn block_actions_round_trip() {
        let breaking = PlayerAuthInput {
            block_actions: vec![BlockAction {
                action: crate::packets::player_action::PREDICT_DESTROY_BLOCK,
                position: crate::types::BlockPos { x: 3, y: -61, z: 4 },
                face: 1,
            }],
            ..input()
        };
        assert_eq!(decode_input(&breaking.encode()), breaking);
    }

    #[test]
    fn block_actions_are_found_behind_an_item_interaction() {
        let mut tail = vec![0x01];
        // Legacy request ID 0, no legacy slots, one inventory action.
        tail.extend([0x00, 0x00, 0x01]);
        // Source 0, window ID 0, no flags, slot 1, two empty items.
        tail.extend([0x00, 0x01, 0x00, 0x00, 0x01]);
        let empty_item = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        tail.extend(empty_item);
        tail.extend(empty_item);
        // Action, trigger, block position, face, hotbar slot, hand, held item.
        tail.extend([0x00, 0x01, 0x10, 0x79, 0x10, 0x01, 0x00, 0x00]);
        tail.extend(empty_item);
        // Two positions, block runtime ID, prediction and cooldown.
        tail.extend([0; 24]);
        tail.extend([0x00, 0x01, 0x00]);
        // No item stack request; one block action: start break at (8, -61, 8).
        tail.extend([0x00, 0x01, 0x01, 0x00, 0x10, 0x79, 0x10, 0x02]);
        tail.extend([0x00, 0x00]);
        tail.extend([0; 8 + 12 + 8]);

        let input = decode_input(&with_tail(&tail));
        assert_eq!(
            input.block_actions,
            [BlockAction {
                action: crate::packets::player_action::START_BREAK,
                position: crate::types::BlockPos { x: 8, y: -61, z: 8 },
                face: 1,
            }]
        );
        assert!(!input.block_actions_unread);
    }

    #[test]
    fn item_stack_requests_are_read_along_with_block_actions() {
        use crate::packets::StackAction;
        let mining = PlayerAuthInput {
            item_stack_request: Some(StackRequest {
                id: -7,
                actions: vec![StackAction::MineBlock {
                    hotbar_slot: 0,
                    predicted_durability: 3,
                    stack_id: 12,
                }],
                filter_strings: Vec::new(),
                filter_cause: 0,
            }),
            block_actions: vec![BlockAction {
                action: crate::packets::player_action::START_BREAK,
                position: crate::types::BlockPos { x: 1, y: -61, z: 1 },
                face: 1,
            }],
            ..input()
        };
        assert_eq!(decode_input(&mining.encode()), mining);
    }

    #[test]
    fn an_unreadable_tail_keeps_the_movement() {
        // An item stack request that ends early.
        let input = decode_input(&with_tail(&[0x00, 0x01, 0x05, 0x06]));
        assert!(input.block_actions_unread);
        assert_eq!(input.position, super::tests::input().position);

        // So does garbage: an item interaction that ends early.
        let input = decode_input(&with_tail(&[0x01, 0x00]));
        assert!(input.block_actions_unread);
        assert_eq!(input.delta, super::tests::input().delta);
    }

    #[test]
    fn decodes_the_leading_fields_and_skips_the_rest() {
        let bytes = input().encode();
        // Header, then pitch and yaw as little-endian floats.
        assert_eq!(bytes[..2], [0x90, 0x01]);
        assert_eq!(bytes[2..6], 10.0f32.to_le_bytes());
        assert_eq!(bytes[6..10], (-90.0f32).to_le_bytes());

        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::PLAYER_AUTH_INPUT);
        assert_eq!(decode::<PlayerAuthInput>(payload).unwrap(), input());
    }

    #[test]
    fn rejects_absurd_flag_counts() {
        let mut bytes = vec![0x90, 0x01];
        bytes.extend([0; 4 * 8]);
        bytes.extend([0xFF, 0x0F]);
        let (_, payload) = read_header(&bytes).unwrap();
        assert!(matches!(
            decode::<PlayerAuthInput>(payload),
            Err(DecodeError::InvalidValue {
                field: "input flag count",
                ..
            })
        ));
    }

    #[test]
    fn move_player_layout() {
        let packet = MovePlayer {
            entity_runtime_id: 2,
            position: Vec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            pitch: 0.0,
            yaw: 90.0,
            head_yaw: 45.0,
            mode: MoveMode::Normal,
            on_ground: true,
            ridden_entity_runtime_id: 0,
            tick: 300,
        };
        let mut expected = vec![0x13, 0x02];
        for value in [1.0f32, 2.0, 3.0, 0.0, 90.0, 45.0] {
            expected.extend(value.to_le_bytes());
        }
        expected.extend([0x00, 0x01, 0x00, 0x00, 0xAC, 0x02]);
        assert_eq!(packet.encode(), expected);
    }
}
