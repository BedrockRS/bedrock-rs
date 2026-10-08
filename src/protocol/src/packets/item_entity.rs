//! Item entities and other plain entities (falling blocks): showing them,
//! how they move, and items being picked up. Layouts follow gophertunnel
//! for protocol 2193.

use crate::io::Writer;
use crate::packet::{Encode, Packet, id};
use crate::packets::{EntityMetadata, ItemInstance};
use crate::types::Vec3;

/// Shows an item entity.
#[derive(Debug, Clone, PartialEq)]
pub struct AddItemActor {
    pub entity_unique_id: i64,
    pub entity_runtime_id: u64,
    pub item: ItemInstance,
    pub position: Vec3,
    pub velocity: Vec3,
    pub metadata: EntityMetadata,
    pub from_fishing: bool,
}

impl Packet for AddItemActor {
    const ID: u32 = id::ADD_ITEM_ACTOR;
}

impl Encode for AddItemActor {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i64(self.entity_unique_id);
        writer.var_u64(self.entity_runtime_id);
        self.item.write(writer);
        self.position.write(writer);
        self.velocity.write(writer);
        self.metadata.write(writer);
        writer.bool(self.from_fishing);
    }
}

/// Shows an entity that is not a player or an item, by its type, such as
/// `minecraft:falling_block`.
#[derive(Debug, Clone, PartialEq)]
pub struct AddActor {
    pub entity_unique_id: i64,
    pub entity_runtime_id: u64,
    pub entity_type: String,
    pub position: Vec3,
    pub velocity: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub body_yaw: f32,
    pub metadata: EntityMetadata,
}

impl Packet for AddActor {
    const ID: u32 = id::ADD_ACTOR;
}

impl Encode for AddActor {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i64(self.entity_unique_id);
        writer.var_u64(self.entity_runtime_id);
        writer.string(&self.entity_type);
        self.position.write(writer);
        self.velocity.write(writer);
        for angle in [self.pitch, self.yaw, self.head_yaw, self.body_yaw] {
            writer.f32_le(angle);
        }
        // No attributes.
        writer.var_u32(0);
        self.metadata.write(writer);
        // No integer or float entity properties, no entity links.
        writer.var_u32(0);
        writer.var_u32(0);
        writer.var_u32(0);
    }
}

/// An entity picking up an item entity: the item flies to the taker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeItemActor {
    pub item_entity_runtime_id: u64,
    pub taker_entity_runtime_id: u64,
}

impl Packet for TakeItemActor {
    const ID: u32 = id::TAKE_ITEM_ACTOR;
}

impl Encode for TakeItemActor {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.item_entity_runtime_id);
        writer.var_u64(self.taker_entity_runtime_id);
    }
}

/// Moves a (non-player) entity to a position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveActorAbsolute {
    pub entity_runtime_id: u64,
    /// [`MoveActorAbsolute::ON_GROUND`] and friends.
    pub flags: u8,
    pub position: Vec3,
    /// Pitch, yaw and head yaw, in degrees.
    pub rotation: Vec3,
}

impl MoveActorAbsolute {
    pub const ON_GROUND: u8 = 1;
    pub const TELEPORT: u8 = 1 << 1;
}

impl Packet for MoveActorAbsolute {
    const ID: u32 = id::MOVE_ACTOR_ABSOLUTE;
}

impl Encode for MoveActorAbsolute {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        writer.u8(self.flags);
        self.position.write(writer);
        // Angles as a byte each: 256 steps per turn.
        for angle in [self.rotation.x, self.rotation.y, self.rotation.z] {
            writer.u8((angle / (360.0 / 256.0)).rem_euclid(256.0) as u8);
        }
    }
}

/// Sets an entity's velocity, which the client uses between position updates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetActorMotion {
    pub entity_runtime_id: u64,
    pub velocity: Vec3,
    pub tick: u64,
}

impl Packet for SetActorMotion {
    const ID: u32 = id::SET_ACTOR_MOTION;
}

impl Encode for SetActorMotion {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        self.velocity.write(writer);
        writer.var_u64(self.tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::read_header;

    fn payload(packet: &impl Encode) -> Vec<u8> {
        let bytes = packet.encode();
        let (_, mut reader) = read_header(&bytes).unwrap();
        reader.take(reader.remaining()).unwrap().to_vec()
    }

    #[test]
    fn items_are_added_with_an_item_and_a_velocity() {
        let add = AddItemActor {
            entity_unique_id: 7,
            entity_runtime_id: 7,
            item: ItemInstance::EMPTY,
            position: Vec3::default(),
            velocity: Vec3 {
                x: 0.0,
                y: 0.1,
                z: 0.0,
            },
            metadata: EntityMetadata(Vec::new()),
            from_fishing: false,
        };
        let bytes = payload(&add);
        // IDs 7 (zigzag 14 for the unique ID), then the item.
        assert_eq!(bytes[..2], [14, 7]);
        assert_eq!(bytes.len(), 2 + 8 + 12 + 12 + 1 + 1);
        assert_eq!(read_header(&add.encode()).unwrap().0.id, id::ADD_ITEM_ACTOR);
    }

    #[test]
    fn actors_are_added_by_type() {
        let add = AddActor {
            entity_unique_id: 7,
            entity_runtime_id: 7,
            entity_type: "minecraft:falling_block".into(),
            position: Vec3::default(),
            velocity: Vec3::default(),
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            body_yaw: 0.0,
            metadata: EntityMetadata(vec![(
                crate::packets::metadata_key::VARIANT,
                crate::packets::MetadataValue::Int(-1),
            )]),
        };
        let bytes = payload(&add);
        assert_eq!(bytes[..3], [14, 7, 23]);
        assert_eq!(&bytes[3..26], b"minecraft:falling_block");
        // Position, velocity, four angles, no attributes, then the one
        // metadata entry (key 2, type 2 twice, zigzag -1), then three empty lists.
        assert_eq!(&bytes[26 + 40..], [0, 1, 2, 2, 2, 1, 0, 0, 0]);
        assert_eq!(read_header(&add.encode()).unwrap().0.id, id::ADD_ACTOR);
    }

    #[test]
    fn movement_and_pickups_encode() {
        let moved = MoveActorAbsolute {
            entity_runtime_id: 3,
            flags: MoveActorAbsolute::ON_GROUND,
            position: Vec3::default(),
            rotation: Vec3 {
                x: 0.0,
                y: 90.0,
                z: -90.0,
            },
        };
        let bytes = payload(&moved);
        assert_eq!(bytes[..2], [3, 1]);
        assert_eq!(bytes[14..], [0, 64, 192]);
        assert_eq!(
            payload(&TakeItemActor {
                item_entity_runtime_id: 5,
                taker_entity_runtime_id: 1,
            }),
            [5, 1]
        );
        assert_eq!(
            payload(&SetActorMotion {
                entity_runtime_id: 5,
                velocity: Vec3::default(),
                tick: 0,
            })
            .len(),
            1 + 12 + 1
        );
    }
}
