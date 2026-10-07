//! Opening and closing inventory windows.
//!
//! With server-authoritative inventories, the client asks before showing its
//! own inventory: pressing the inventory key sends Interact with action
//! [`interact_action::OPEN_INVENTORY`], and the screen opens only when the
//! server answers with ContainerOpen. Closing it sends ContainerClose, which
//! the server echoes. Layouts follow gophertunnel; replies follow Dragonfly.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::types::{BlockPos, Vec3};

/// Interact action IDs.
pub mod interact_action {
    pub const LEAVE_VEHICLE: u8 = 3;
    pub const MOUSE_OVER_ENTITY: u8 = 4;
    pub const NPC_OPEN: u8 = 5;
    pub const OPEN_INVENTORY: u8 = 6;
}

/// A player interacting with an entity, or with their own inventory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interact {
    /// One of the [`interact_action`] values.
    pub action: u8,
    pub target_entity_runtime_id: u64,
    pub position: Option<Vec3>,
}

impl Packet for Interact {
    const ID: u32 = id::INTERACT;
}

impl Decode for Interact {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let action = reader.u8()?;
        let target_entity_runtime_id = reader.var_u64()?;
        let position = if reader.bool()? {
            Some(Vec3::read(reader)?)
        } else {
            None
        };
        Ok(Self {
            action,
            target_entity_runtime_id,
            position,
        })
    }
}

impl Encode for Interact {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.u8(self.action);
        writer.var_u64(self.target_entity_runtime_id);
        writer.bool(self.position.is_some());
        if let Some(position) = self.position {
            position.write(writer);
        }
    }
}

/// Window types for ContainerOpen and ContainerClose.
pub mod container_type {
    /// The player's own inventory, as a window type (-1 as a byte).
    pub const INVENTORY: u8 = 0xFF;
}

/// The window ID the player's own inventory opens as.
pub const OWN_INVENTORY_WINDOW: u8 = 0;
/// The window ID a client closes with when the inventory and chat open together.
pub const NO_WINDOW: u8 = 0xFF;

/// Tells the client it may show a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerOpen {
    pub window_id: u8,
    pub container_type: u8,
    pub position: BlockPos,
    /// The entity the window belongs to; -1 for none.
    pub entity_unique_id: i64,
}

impl ContainerOpen {
    /// Opens the player's own inventory, which vanilla places at their feet.
    pub fn own_inventory(position: BlockPos) -> Self {
        Self {
            window_id: OWN_INVENTORY_WINDOW,
            container_type: container_type::INVENTORY,
            position,
            entity_unique_id: -1,
        }
    }
}

impl Packet for ContainerOpen {
    const ID: u32 = id::CONTAINER_OPEN;
}

impl Encode for ContainerOpen {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.u8(self.window_id);
        writer.u8(self.container_type);
        self.position.write(writer);
        writer.var_i64(self.entity_unique_id);
    }
}

/// A window closing: from the client when the player closes it, and from
/// the server to confirm or to close it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerClose {
    pub window_id: u8,
    pub container_type: u8,
    /// Whether the server closed the window rather than the player.
    pub server_side: bool,
}

impl Packet for ContainerClose {
    const ID: u32 = id::CONTAINER_CLOSE;
}

impl Decode for ContainerClose {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            window_id: reader.u8()?,
            container_type: reader.u8()?,
            server_side: reader.bool()?,
        })
    }
}

impl Encode for ContainerClose {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.u8(self.window_id);
        writer.u8(self.container_type);
        writer.bool(self.server_side);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    #[test]
    fn open_inventory_requests_decode() {
        let interact = Interact {
            action: interact_action::OPEN_INVENTORY,
            target_entity_runtime_id: 1,
            position: None,
        };
        let bytes = interact.encode();
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::INTERACT);
        assert_eq!(decode::<Interact>(payload).unwrap(), interact);

        let hovering = Interact {
            action: interact_action::MOUSE_OVER_ENTITY,
            position: Some(Vec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            }),
            ..interact
        };
        let bytes = hovering.encode();
        let (_, payload) = read_header(&bytes).unwrap();
        assert_eq!(decode::<Interact>(payload).unwrap(), hovering);
    }

    #[test]
    fn the_own_inventory_opens_as_window_0_of_type_minus_1() {
        let open = ContainerOpen::own_inventory(BlockPos {
            x: 1,
            y: -60,
            z: -2,
        });
        let bytes = open.encode();
        let (header, mut payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::CONTAINER_OPEN);
        assert_eq!(
            payload.take(payload.remaining()).unwrap(),
            [
                0, 0xFF, // window 0, inventory
                2, 119, 3, // (1, -60, -2) as zigzag varints
                1, // entity -1
            ]
        );
    }

    #[test]
    fn closing_round_trips() {
        let close = ContainerClose {
            window_id: OWN_INVENTORY_WINDOW,
            container_type: 0,
            server_side: false,
        };
        let bytes = close.encode();
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::CONTAINER_CLOSE);
        assert_eq!(decode::<ContainerClose>(payload).unwrap(), close);
    }
}
