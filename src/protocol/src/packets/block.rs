//! Breaking blocks, and telling clients about changed blocks.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::types::BlockPos;

/// Action types of [`PlayerAction`] and of PlayerAuthInput block actions, as
/// gophertunnel and PocketMine number them (Mojang's schema omits some values).
pub mod player_action {
    pub const START_BREAK: i32 = 0;
    pub const ABORT_BREAK: i32 = 1;
    pub const STOP_BREAK: i32 = 2;
    /// The death screen's Respawn button; the Respawn packet follows.
    pub const RESPAWN: i32 = 7;
    /// Left-clicking a block in creative; sent in a [`super::PlayerAction`].
    pub const CREATIVE_DESTROY_BLOCK: i32 = 13;
    /// The client predicts it finished breaking a block.
    pub const PREDICT_DESTROY_BLOCK: i32 = 26;
    pub const CONTINUE_DESTROY_BLOCK: i32 = 27;
}

/// A block action from PlayerAuthInput, such as starting or finishing a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockAction {
    /// One of the [`player_action`] values.
    pub action: i32,
    pub position: BlockPos,
    pub face: i32,
}

impl BlockAction {
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            action: reader.var_i32()?,
            position: BlockPos::read(reader)?,
            face: reader.var_i32()?,
        })
    }

    pub fn write(&self, writer: &mut Writer) {
        writer.var_i32(self.action);
        self.position.write(writer);
        writer.var_i32(self.face);
    }
}

/// An action the player took, such as destroying a block in creative mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerAction {
    pub entity_runtime_id: u64,
    /// One of the [`player_action`] values.
    pub action: i32,
    pub block_position: BlockPos,
    pub result_position: BlockPos,
    pub face: i32,
}

impl Packet for PlayerAction {
    const ID: u32 = id::PLAYER_ACTION;
}

impl Decode for PlayerAction {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            entity_runtime_id: reader.var_u64()?,
            action: reader.var_i32()?,
            block_position: BlockPos::read(reader)?,
            result_position: BlockPos::read(reader)?,
            face: reader.var_i32()?,
        })
    }
}

impl Encode for PlayerAction {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        writer.var_i32(self.action);
        self.block_position.write(writer);
        self.result_position.write(writer);
        writer.var_i32(self.face);
    }
}

/// Changes one block on the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateBlock {
    pub position: BlockPos,
    /// The new block's network ID (a block state hash).
    pub block: u32,
    /// [`UpdateBlock::NETWORK`] and friends.
    pub flags: u32,
    /// 0 for blocks, 1 for liquids inside them.
    pub layer: u32,
}

impl UpdateBlock {
    pub const NEIGHBOURS: u32 = 1;
    /// The usual flag for a change sent to clients.
    pub const NETWORK: u32 = 2;
}

impl Packet for UpdateBlock {
    const ID: u32 = id::UPDATE_BLOCK;
}

impl Encode for UpdateBlock {
    fn encode_payload(&self, writer: &mut Writer) {
        self.position.write(writer);
        writer.var_u32(self.block);
        writer.var_u32(self.flags);
        writer.var_u32(self.layer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    #[test]
    fn player_action_round_trips() {
        let action = PlayerAction {
            entity_runtime_id: 1,
            action: player_action::CREATIVE_DESTROY_BLOCK,
            block_position: BlockPos { x: 8, y: -61, z: 8 },
            result_position: BlockPos::default(),
            face: 1,
        };
        let bytes = action.encode();
        assert_eq!(bytes[..4], [0x24, 0x01, 26, 16]);
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::PLAYER_ACTION);
        assert_eq!(decode::<PlayerAction>(payload).unwrap(), action);
    }

    #[test]
    fn update_block_layout() {
        let update = UpdateBlock {
            position: BlockPos { x: 8, y: -61, z: 8 },
            block: 300,
            flags: UpdateBlock::NETWORK,
            layer: 0,
        };
        assert_eq!(update.encode(), [0x15, 16, 121, 16, 0xAC, 0x02, 0x02, 0x00]);
    }
}
