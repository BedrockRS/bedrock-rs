//! Items, the player's inventory, and using items on blocks.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::types::{BlockPos, Vec3};

/// Most entries in any list read from an inventory transaction.
const MAX_LIST: u32 = 256;

/// An item stack as sent in inventories and transactions. Of its user data,
/// only NBT is supported: can-place-on and can-break lists are sent empty,
/// and shields' blocking tick as 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemInstance {
    /// The item's network ID from the ItemRegistry; 0 is an empty slot.
    pub network_id: i16,
    pub count: u16,
    pub metadata: u32,
    /// The server's ID for this stack, needed with server-authoritative inventories.
    pub stack_network_id: Option<i32>,
    /// For block items, the network ID of the block they place.
    pub block_runtime_id: u32,
    /// Whether the item is a shield, whose user data has an extra field.
    pub shield: bool,
    /// The item's NBT (enchantments and the like), as a little-endian NBT
    /// compound with its root tag.
    pub nbt: Option<&'static [u8]>,
}

impl ItemInstance {
    pub const EMPTY: Self = Self {
        network_id: 0,
        count: 0,
        metadata: 0,
        stack_network_id: None,
        block_runtime_id: 0,
        shield: false,
        nbt: None,
    };

    pub fn is_empty(&self) -> bool {
        self.network_id == 0
    }

    pub fn write(&self, writer: &mut Writer) {
        writer.i16_le(self.network_id);
        writer.u16_le(self.count);
        writer.var_u32(self.metadata);
        writer.bool(self.stack_network_id.is_some());
        if let Some(id) = self.stack_network_id {
            writer.var_i32(id);
        }
        writer.var_u32(self.block_runtime_id);
        write_user_data(writer, !self.is_empty(), self.shield, self.nbt);
    }

    /// Reads an item, skipping its user data: what the client says an item
    /// carries is never taken from it.
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let network_id = i16::from_le_bytes([reader.u8()?, reader.u8()?]);
        let count = reader.u16_le()?;
        let metadata = reader.var_u32()?;
        let stack_network_id = if reader.bool()? {
            Some(reader.var_i32()?)
        } else {
            None
        };
        let block_runtime_id = reader.var_u32()?;
        reader.byte_array()?;
        Ok(Self {
            network_id,
            count,
            metadata,
            stack_network_id,
            block_runtime_id,
            // Only the registry knows; nothing read needs it.
            shield: false,
            nbt: None,
        })
    }
}

/// Writes an item's user data, as a length-prefixed blob: nothing for an
/// empty slot; otherwise its NBT, if any (a length of -1, version 1 and the
/// little-endian compound; else a length of 0), no can-place-on or can-break
/// entries, and a blocking tick of 0 for shields.
pub(crate) fn write_user_data(
    writer: &mut Writer,
    present: bool,
    shield: bool,
    nbt: Option<&[u8]>,
) {
    if !present {
        writer.var_u32(0);
        return;
    }
    let nbt_len = nbt.map_or(0, |nbt| 1 + nbt.len());
    let len = 2 + nbt_len + 4 + 4 + if shield { 8 } else { 0 };
    writer.var_u32(u32::try_from(len).expect("item NBT is small"));
    match nbt {
        Some(nbt) => {
            writer.i16_le(-1);
            writer.u8(1);
            writer.raw(nbt);
        }
        None => writer.i16_le(0),
    }
    writer.u32_le(0);
    writer.u32_le(0);
    if shield {
        writer.i64_le(0);
    }
}

/// Window ID of the player's own inventory (hotbar in slots 0..9).
pub const INVENTORY_WINDOW: u32 = 0;

/// Replaces the whole contents of one of the player's windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryContent {
    pub window_id: u32,
    pub content: Vec<ItemInstance>,
}

impl Packet for InventoryContent {
    const ID: u32 = id::INVENTORY_CONTENT;
}

impl Encode for InventoryContent {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u32(self.window_id);
        writer.var_u32(u32::try_from(self.content.len()).expect("a few dozen slots"));
        for item in &self.content {
            item.write(writer);
        }
        // Container name: ID 0 without a dynamic ID, as Dragonfly sends; no
        // storage item.
        writer.u8(0);
        writer.bool(false);
        ItemInstance::EMPTY.write(writer);
    }
}

/// Window ID of the UI inventory, whose slot 0 is the cursor.
pub const UI_WINDOW: u32 = 124;

/// Replaces one slot of one of the player's windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventorySlot {
    pub window_id: u32,
    pub slot: u32,
    pub item: ItemInstance,
}

impl Packet for InventorySlot {
    const ID: u32 = id::INVENTORY_SLOT;
}

impl Encode for InventorySlot {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u32(self.window_id);
        writer.var_u32(self.slot);
        // No container name and no storage item, both optional.
        writer.bool(false);
        writer.bool(false);
        self.item.write(writer);
    }
}

/// What an item-use transaction did.
pub mod use_item_action {
    /// Right-clicked a block: place a block or use the item on it.
    pub const CLICK_BLOCK: i32 = 0;
    pub const CLICK_AIR: i32 = 1;
    pub const BREAK_BLOCK: i32 = 2;
}

/// Using the held item, such as right-clicking a block to place one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UseItem {
    /// One of the [`use_item_action`] values.
    pub action: i32,
    pub trigger: u8,
    /// The block clicked.
    pub block_position: BlockPos,
    /// The face of that block clicked, 0 to 5: down, up, north, south, west, east.
    pub face: u8,
    pub hotbar_slot: i32,
    pub held_item: ItemInstance,
    pub player_position: Vec3,
    /// Where on the block the click landed.
    pub clicked_position: Vec3,
    /// The clicked block's network ID, as the client sees it.
    pub block_runtime_id: u32,
    /// 1 when the client predicted success (and already placed the block).
    pub client_prediction: u8,
}

impl UseItem {
    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let action = reader.var_i32()?;
        let trigger = reader.u8()?;
        let block_position = BlockPos::read(reader)?;
        let face = reader.u8()?;
        let hotbar_slot = reader.var_i32()?;
        // The hand.
        reader.u8()?;
        let held_item = ItemInstance::read(reader)?;
        let player_position = Vec3::read(reader)?;
        let clicked_position = Vec3::read(reader)?;
        let block_runtime_id = reader.var_u32()?;
        let client_prediction = reader.u8()?;
        // The client's cooldown state.
        reader.u8()?;
        Ok(Self {
            action,
            trigger,
            block_position,
            face,
            hotbar_slot,
            held_item,
            player_position,
            clicked_position,
            block_runtime_id,
            client_prediction,
        })
    }

    fn write(&self, writer: &mut Writer) {
        writer.var_i32(self.action);
        writer.u8(self.trigger);
        self.block_position.write(writer);
        writer.u8(self.face);
        writer.var_i32(self.hotbar_slot);
        writer.u8(0);
        self.held_item.write(writer);
        self.player_position.write(writer);
        self.clicked_position.write(writer);
        writer.var_u32(self.block_runtime_id);
        writer.u8(self.client_prediction);
        writer.u8(0);
    }

    /// The block next to the clicked one, on the clicked face: where a block
    /// placed by this click goes.
    pub fn target(&self) -> BlockPos {
        let BlockPos { x, y, z } = self.block_position;
        match self.face {
            0 => BlockPos { x, y: y - 1, z },
            1 => BlockPos { x, y: y + 1, z },
            2 => BlockPos { x, y, z: z - 1 },
            3 => BlockPos { x, y, z: z + 1 },
            4 => BlockPos { x: x - 1, y, z },
            _ => BlockPos { x: x + 1, y, z },
        }
    }
}

/// Transaction type of a normal transaction: with server-authoritative
/// inventories, only throwing the held item from the HUD (Q) still uses it.
const NORMAL_TRANSACTION: u32 = 0;
/// Transaction type of an item-use transaction.
const USE_ITEM_TRANSACTION: u32 = 2;

/// Where an inventory action's items come from or go.
pub mod action_source {
    pub const CONTAINER: u32 = 0;
    /// The world: an item thrown out of the inventory.
    pub const WORLD: u32 = 2;
    pub const CREATIVE: u32 = 3;
}

/// One slot's change in a legacy inventory transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryAction {
    /// One of the [`action_source`] values.
    pub source: u32,
    /// For container actions, the window (0 is the player's inventory).
    pub window_id: Option<i8>,
    pub source_flags: Option<u32>,
    pub slot: u32,
    pub old_item: ItemInstance,
    pub new_item: ItemInstance,
}

impl InventoryAction {
    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let source = reader.var_u32()?;
        let window_id = if reader.bool()? {
            Some(reader.u8()? as i8)
        } else {
            None
        };
        let source_flags = if reader.bool()? {
            Some(reader.var_u32()?)
        } else {
            None
        };
        Ok(Self {
            source,
            window_id,
            source_flags,
            slot: reader.var_u32()?,
            old_item: ItemInstance::read(reader)?,
            new_item: ItemInstance::read(reader)?,
        })
    }

    fn write(&self, writer: &mut Writer) {
        writer.var_u32(self.source);
        writer.bool(self.window_id.is_some());
        if let Some(id) = self.window_id {
            writer.u8(id as u8);
        }
        writer.bool(self.source_flags.is_some());
        if let Some(flags) = self.source_flags {
            writer.var_u32(flags);
        }
        writer.var_u32(self.slot);
        self.old_item.write(writer);
        self.new_item.write(writer);
    }
}

/// An inventory transaction. Normal and item-use transactions are decoded;
/// others keep just their type.
#[derive(Debug, Clone, PartialEq)]
pub enum InventoryTransaction {
    /// Items moved by actions alone, such as throwing the held item.
    Normal(Vec<InventoryAction>),
    UseItem(UseItem),
    Other(u32),
}

impl Packet for InventoryTransaction {
    const ID: u32 = id::INVENTORY_TRANSACTION;
}

impl Decode for InventoryTransaction {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        // Legacy request ID, then optional legacy slots: container ID and slot bytes.
        reader.var_i32()?;
        if reader.bool()? {
            for _ in 0..list_len(reader, "legacy slot count")? {
                reader.u8()?;
                reader.byte_array()?;
            }
        }
        let kind = reader.var_u32()?;
        let actions = (0..list_len(reader, "inventory action count")?)
            .map(|_| InventoryAction::read(reader))
            .collect::<Result<Vec<_>, _>>()?;
        match kind {
            NORMAL_TRANSACTION => Ok(Self::Normal(actions)),
            USE_ITEM_TRANSACTION => Ok(Self::UseItem(UseItem::read(reader)?)),
            _ => {
                reader.take(reader.remaining())?;
                Ok(Self::Other(kind))
            }
        }
    }
}

impl Encode for InventoryTransaction {
    /// Writes a transaction as a client would, without legacy slots. Item use
    /// has no inventory actions; other kinds are written without their data.
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i32(0);
        writer.bool(false);
        match self {
            Self::Normal(actions) => {
                writer.var_u32(NORMAL_TRANSACTION);
                writer.var_u32(u32::try_from(actions.len()).expect("a few actions"));
                for action in actions {
                    action.write(writer);
                }
            }
            Self::UseItem(use_item) => {
                writer.var_u32(USE_ITEM_TRANSACTION);
                writer.var_u32(0);
                use_item.write(writer);
            }
            Self::Other(kind) => {
                writer.var_u32(*kind);
                writer.var_u32(0);
            }
        }
    }
}

/// What a player holds: sent by the client when it changes hotbar slot, and
/// by the server to show other players what someone holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MobEquipment {
    pub entity_runtime_id: u64,
    pub item: ItemInstance,
    /// The slot in the window; for the inventory, the hotbar slot.
    pub inventory_slot: u8,
    pub hotbar_slot: u8,
    pub window_id: u8,
}

impl Packet for MobEquipment {
    const ID: u32 = id::MOB_EQUIPMENT;
}

impl Decode for MobEquipment {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            entity_runtime_id: reader.var_u64()?,
            item: ItemInstance::read(reader)?,
            inventory_slot: reader.u8()?,
            hotbar_slot: reader.u8()?,
            window_id: reader.u8()?,
        })
    }
}

impl Encode for MobEquipment {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        self.item.write(writer);
        writer.u8(self.inventory_slot);
        writer.u8(self.hotbar_slot);
        writer.u8(self.window_id);
    }
}

/// Reads a list length item transactions use, rejecting absurd ones.
pub(crate) fn list_len(reader: &mut Reader<'_>, field: &'static str) -> Result<u32, DecodeError> {
    let count = reader.var_u32()?;
    if count > MAX_LIST {
        return Err(DecodeError::InvalidValue {
            field,
            value: count.into(),
        });
    }
    Ok(count)
}

/// Steps over an item-use transaction as embedded in PlayerAuthInput, which
/// carries its legacy slots and actions inline.
pub(crate) fn skip_embedded_use_item(reader: &mut Reader<'_>) -> Result<(), DecodeError> {
    reader.var_i32()?;
    if reader.bool()? {
        for _ in 0..list_len(reader, "legacy slot count")? {
            reader.u8()?;
            reader.byte_array()?;
        }
    }
    for _ in 0..list_len(reader, "inventory action count")? {
        reader.var_u32()?;
        if reader.bool()? {
            reader.u8()?;
        }
        if reader.bool()? {
            reader.var_u32()?;
        }
        reader.var_u32()?;
        ItemInstance::read(reader)?;
        ItemInstance::read(reader)?;
    }
    UseItem::read(reader)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn stone() -> ItemInstance {
        ItemInstance {
            network_id: 1,
            count: 64,
            metadata: 0,
            stack_network_id: Some(1),
            block_runtime_id: 12345,
            shield: false,
            nbt: None,
        }
    }

    #[test]
    fn nbt_goes_in_the_user_data() {
        // An empty little-endian compound: its tag, a nameless root, the end.
        const EMPTY_COMPOUND: &[u8] = &[10, 0, 0, 0];
        let book = ItemInstance {
            stack_network_id: None,
            block_runtime_id: 0,
            nbt: Some(EMPTY_COMPOUND),
            ..stone()
        };
        let mut writer = Writer::new();
        book.write(&mut writer);
        let bytes = writer.into_bytes();
        // ID, count, metadata, no stack ID, no block, then the user data:
        // its length, -1 and version 1, the NBT, and two empty lists.
        assert_eq!(
            bytes[7..],
            [15, 0xFF, 0xFF, 1, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        // What the client says an item carries is not taken.
        let read = ItemInstance::read(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(read, ItemInstance { nbt: None, ..book });
    }

    #[test]
    fn items_round_trip_and_empty_slots_are_short() {
        let mut writer = Writer::new();
        ItemInstance::EMPTY.write(&mut writer);
        assert_eq!(writer.into_bytes(), [0, 0, 0, 0, 0, 0, 0, 0]);

        let mut writer = Writer::new();
        stone().write(&mut writer);
        let bytes = writer.into_bytes();
        // ID 1 and count 64 as little-endian shorts, metadata 0, stack ID 1.
        assert_eq!(bytes[..7], [0x01, 0x00, 0x40, 0x00, 0x00, 0x01, 0x02]);
        let item = ItemInstance::read(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(item, stone());
    }

    #[test]
    fn clearing_the_cursor_is_a_short_packet() {
        let packet = InventorySlot {
            window_id: UI_WINDOW,
            slot: 0,
            item: ItemInstance::EMPTY,
        };
        let bytes = packet.encode();
        let (header, mut payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::INVENTORY_SLOT);
        assert_eq!(
            payload.take(payload.remaining()).unwrap(),
            [124, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn shields_carry_a_blocking_tick() {
        let shield = ItemInstance {
            network_id: 387,
            count: 1,
            shield: true,
            ..stone()
        };
        let mut writer = Writer::new();
        shield.write(&mut writer);
        let bytes = writer.into_bytes();
        // The user data is 18 bytes long: NBT length, two empty lists, the tick.
        assert_eq!(bytes[bytes.len() - 19], 18);
        assert_eq!(
            ItemInstance::read(&mut Reader::new(&bytes)).unwrap().count,
            1
        );
    }

    #[test]
    fn placing_a_block_round_trips_and_targets_the_clicked_face() {
        let use_item = UseItem {
            action: use_item_action::CLICK_BLOCK,
            trigger: 1,
            block_position: BlockPos { x: 8, y: -61, z: 8 },
            face: 1,
            hotbar_slot: 0,
            held_item: stone(),
            player_position: Vec3::default(),
            clicked_position: Vec3 {
                x: 0.5,
                y: 1.0,
                z: 0.5,
            },
            block_runtime_id: 99,
            client_prediction: 1,
        };
        let transaction = InventoryTransaction::UseItem(use_item);
        let bytes = transaction.encode();
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::INVENTORY_TRANSACTION);
        assert_eq!(
            decode::<InventoryTransaction>(payload).unwrap(),
            transaction
        );

        assert_eq!(use_item.target(), BlockPos { x: 8, y: -60, z: 8 });
        let west = UseItem {
            face: 4,
            ..use_item
        };
        assert_eq!(west.target(), BlockPos { x: 7, y: -61, z: 8 });
    }

    #[test]
    fn hud_drops_decode_with_their_actions() {
        let drop = InventoryTransaction::Normal(vec![
            InventoryAction {
                source: action_source::WORLD,
                window_id: None,
                source_flags: Some(0),
                slot: 0,
                old_item: ItemInstance::EMPTY,
                new_item: ItemInstance {
                    count: 1,
                    stack_network_id: None,
                    ..stone()
                },
            },
            InventoryAction {
                source: action_source::CONTAINER,
                window_id: Some(0),
                source_flags: None,
                slot: 3,
                old_item: stone(),
                new_item: ItemInstance {
                    count: 63,
                    ..stone()
                },
            },
        ]);
        let bytes = drop.encode();
        let (_, payload) = read_header(&bytes).unwrap();
        assert_eq!(decode::<InventoryTransaction>(payload).unwrap(), drop);
    }

    #[test]
    fn equipment_round_trips() {
        let held = MobEquipment {
            entity_runtime_id: 4,
            item: stone(),
            inventory_slot: 2,
            hotbar_slot: 2,
            window_id: 0,
        };
        let bytes = held.encode();
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::MOB_EQUIPMENT);
        assert_eq!(decode::<MobEquipment>(payload).unwrap(), held);
    }

    #[test]
    fn other_transactions_keep_only_their_type() {
        let bytes = InventoryTransaction::Other(4).encode();
        let (_, payload) = read_header(&bytes).unwrap();
        assert_eq!(
            decode::<InventoryTransaction>(payload).unwrap(),
            InventoryTransaction::Other(4)
        );
    }
}
