//! Particles, sounds and animations other players see and hear.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::types::Vec3;

/// World events: particles and sounds tied to a position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelEvent {
    /// One of the `LevelEvent::` constants.
    pub event: i32,
    pub position: Vec3,
    pub data: i32,
}

impl LevelEvent {
    /// A block's breaking particles and sound; `data` is the broken block's
    /// network ID.
    pub const DESTROY_BLOCK: i32 = 2001;
}

impl Packet for LevelEvent {
    const ID: u32 = id::LEVEL_EVENT;
}

impl Encode for LevelEvent {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i32(self.event);
        self.position.write(writer);
        writer.var_i32(self.data);
    }
}

/// A sound at a position, named by its sound event (e.g. `place`).
#[derive(Debug, Clone, PartialEq)]
pub struct LevelSoundEvent {
    pub sound: String,
    pub position: Vec3,
    /// Sound-specific data; for block sounds, the block's network ID.
    pub data: i32,
}

impl LevelSoundEvent {
    /// The sound of placing a block.
    pub const PLACE: &str = "place";
}

impl Packet for LevelSoundEvent {
    const ID: u32 = id::LEVEL_SOUND_EVENT;
}

impl Encode for LevelSoundEvent {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.string(&self.sound);
        self.position.write(writer);
        writer.var_i32(self.data);
        // No entity type (":" as Dragonfly sends), not a baby, relative volume,
        // no entity, and not fired at another position.
        writer.string(":");
        writer.bool(false);
        writer.bool(false);
        writer.i64_le(-1);
        writer.bool(false);
    }
}

/// An entity animation: arm swings, both ways. Clients say what caused a
/// swing, such as `attack` or `build`.
#[derive(Debug, Clone, PartialEq)]
pub struct Animate {
    /// One of the `Animate::` constants.
    pub action: u8,
    pub entity_runtime_id: u64,
    pub swing_source: Option<String>,
}

impl Animate {
    pub const SWING_ARM: u8 = 1;
}

impl Decode for Animate {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let action = reader.u8()?;
        let entity_runtime_id = reader.var_u64()?;
        // Data, used by some actions.
        reader.f32_le()?;
        let swing_source = if reader.bool()? {
            Some(reader.string()?.to_owned())
        } else {
            None
        };
        Ok(Self {
            action,
            entity_runtime_id,
            swing_source,
        })
    }
}

impl Packet for Animate {
    const ID: u32 = id::ANIMATE;
}

impl Encode for Animate {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.u8(self.action);
        writer.var_u64(self.entity_runtime_id);
        // Data, then the swing source.
        writer.f32_le(0.0);
        writer.bool(self.swing_source.is_some());
        if let Some(source) = &self.swing_source {
            writer.string(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_layouts() {
        let destroy = LevelEvent {
            event: LevelEvent::DESTROY_BLOCK,
            position: Vec3::default(),
            data: 5,
        };
        let mut expected = vec![0x19, 0xA2, 0x1F];
        expected.extend([0; 12]);
        expected.push(10);
        assert_eq!(destroy.encode(), expected);

        let swing = Animate {
            action: Animate::SWING_ARM,
            entity_runtime_id: 2,
            swing_source: None,
        };
        assert_eq!(swing.encode(), [0x2C, 0x01, 0x02, 0, 0, 0, 0, 0x00]);
        let attack = Animate {
            swing_source: Some("attack".into()),
            ..swing
        };
        let bytes = attack.encode();
        let (_, payload) = crate::packet::read_header(&bytes).unwrap();
        assert_eq!(crate::packet::decode::<Animate>(payload).unwrap(), attack);

        let place = LevelSoundEvent {
            sound: LevelSoundEvent::PLACE.into(),
            position: Vec3::default(),
            data: 7,
        }
        .encode();
        assert_eq!(place[..3], [0x7B, 0x05, b'p']);
        assert_eq!(place[place.len() - 13..place.len() - 9], [0x01, b':', 0, 0]);
    }
}
