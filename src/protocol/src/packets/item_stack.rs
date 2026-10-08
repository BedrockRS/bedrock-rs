//! Server-authoritative inventories: the client asks to move items with
//! ItemStackRequest, and the server accepts or rejects each request with
//! ItemStackResponse, saying what the changed slots now hold.
//!
//! Wire layouts follow gophertunnel for protocol 2193.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::packets::inventory::list_len;

/// Containers the player's own inventory screen uses, as `FullContainerName`
/// IDs.
///
/// These are gophertunnel's and PocketMine's values, which a 1.26.51 client was
/// seen using on 2026-09-27. Mojang's schema lists the container names in
/// declaration order, which puts these three higher (hotbar 31, cursor 62),
/// but `RecipeFood`, `RecipeBlocks` and `RecipeFurnaceItems` were given the
/// values 64 to 66 at the end.
pub mod container {
    pub const ARMOR: u8 = 6;
    /// The crafting grid: slots 28 to 31 of the inventory screen's 2x2 grid.
    pub const CRAFTING_INPUT: u8 = 13;
    pub const COMBINED_HOTBAR_AND_INVENTORY: u8 = 12;
    pub const HOTBAR: u8 = 28;
    pub const INVENTORY: u8 = 29;
    pub const OFFHAND: u8 = 34;
    /// The item held on the pointer while moving items around.
    pub const CURSOR: u8 = 59;
    /// Where crafted and creative items appear before they are taken.
    pub const CREATED_OUTPUT: u8 = 60;
}

/// A container: its ID from [`container`], and for dynamic containers (such as
/// bundles) which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FullContainerName {
    pub container: u8,
    pub dynamic_id: Option<u32>,
}

impl FullContainerName {
    pub const fn new(container: u8) -> Self {
        Self {
            container,
            dynamic_id: None,
        }
    }

    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let container = reader.u8()?;
        let dynamic_id = if reader.bool()? {
            Some(reader.u32_le()?)
        } else {
            None
        };
        Ok(Self {
            container,
            dynamic_id,
        })
    }

    pub fn write(&self, writer: &mut Writer) {
        writer.u8(self.container);
        writer.bool(self.dynamic_id.is_some());
        if let Some(id) = self.dynamic_id {
            writer.u32_le(id);
        }
    }
}

/// A slot an action refers to, and the stack the client believes is in it:
/// a server stack ID, or, for an item created earlier in the same request,
/// that request's (negative) ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackSlot {
    pub container: FullContainerName,
    pub slot: u8,
    pub stack_id: i32,
}

impl StackSlot {
    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            container: FullContainerName::read(reader)?,
            slot: reader.u8()?,
            stack_id: reader.i32_le()?,
        })
    }

    fn write(&self, writer: &mut Writer) {
        self.container.write(writer);
        writer.u8(self.slot);
        writer.i32_le(self.stack_id);
    }
}

/// One step of an item stack request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackAction {
    /// Moves `count` items from `source` to `destination`, usually the cursor.
    Take {
        count: u8,
        source: StackSlot,
        destination: StackSlot,
    },
    /// Moves `count` items from `source`, usually the cursor, to `destination`.
    Place {
        count: u8,
        source: StackSlot,
        destination: StackSlot,
    },
    Swap {
        source: StackSlot,
        destination: StackSlot,
    },
    /// Throws items out of the inventory into the world.
    Drop {
        count: u8,
        source: StackSlot,
        randomly: bool,
    },
    /// Deletes items, as the creative inventory does when items are put back.
    Destroy {
        count: u8,
        source: StackSlot,
    },
    Consume {
        count: u8,
        source: StackSlot,
    },
    Create {
        results_slot: u8,
    },
    /// Takes a creative item, by its creative network ID, into the created
    /// output container.
    CraftCreative {
        creative_item: u32,
        crafts: u8,
    },
    /// Mining with a tool, carrying its predicted durability.
    MineBlock {
        hotbar_slot: i32,
        predicted_durability: i32,
        stack_id: i32,
    },
    /// What the client expects a craft to produce; informational.
    CraftResults,
    /// An action BedrockRS reads past but does not carry out, by its type.
    Other(u32),
}

/// Action types, in the order the protocol numbers them.
mod action_type {
    pub const TAKE: u32 = 0;
    pub const PLACE: u32 = 1;
    pub const SWAP: u32 = 2;
    pub const DROP: u32 = 3;
    pub const DESTROY: u32 = 4;
    pub const CONSUME: u32 = 5;
    pub const CREATE: u32 = 6;
    pub const LAB_TABLE_COMBINE: u32 = 7;
    pub const BEACON_PAYMENT: u32 = 8;
    pub const MINE_BLOCK: u32 = 9;
    pub const CRAFT_RECIPE: u32 = 10;
    pub const CRAFT_CREATIVE: u32 = 12;
    pub const CRAFT_RECIPE_OPTIONAL: u32 = 13;
    pub const CRAFT_GRINDSTONE: u32 = 14;
    pub const CRAFT_LOOM: u32 = 15;
    pub const CRAFT_NON_IMPLEMENTED: u32 = 16;
    pub const CRAFT_RESULTS: u32 = 17;
}

impl StackAction {
    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        use action_type::*;
        let kind = reader.var_u32()?;
        // The type again, as the legacy byte ID; the variant above decides.
        reader.u8()?;
        Ok(match kind {
            TAKE | PLACE => {
                let count = reader.u8()?;
                let source = StackSlot::read(reader)?;
                let destination = StackSlot::read(reader)?;
                if kind == TAKE {
                    Self::Take {
                        count,
                        source,
                        destination,
                    }
                } else {
                    Self::Place {
                        count,
                        source,
                        destination,
                    }
                }
            }
            SWAP => Self::Swap {
                source: StackSlot::read(reader)?,
                destination: StackSlot::read(reader)?,
            },
            DROP => Self::Drop {
                count: reader.u8()?,
                source: StackSlot::read(reader)?,
                randomly: reader.bool()?,
            },
            DESTROY => Self::Destroy {
                count: reader.u8()?,
                source: StackSlot::read(reader)?,
            },
            CONSUME => Self::Consume {
                count: reader.u8()?,
                source: StackSlot::read(reader)?,
            },
            CREATE => Self::Create {
                results_slot: reader.u8()?,
            },
            LAB_TABLE_COMBINE | CRAFT_NON_IMPLEMENTED => Self::Other(kind),
            BEACON_PAYMENT => {
                reader.var_i32()?;
                reader.var_i32()?;
                Self::Other(kind)
            }
            MINE_BLOCK => Self::MineBlock {
                hotbar_slot: reader.var_i32()?,
                predicted_durability: reader.var_i32()?,
                stack_id: reader.i32_le()?,
            },
            CRAFT_RECIPE => {
                reader.var_u32()?;
                reader.u8()?;
                Self::Other(kind)
            }
            CRAFT_CREATIVE => Self::CraftCreative {
                creative_item: reader.var_u32()?,
                crafts: reader.u8()?,
            },
            CRAFT_RECIPE_OPTIONAL => {
                reader.var_u32()?;
                reader.i32_le()?;
                Self::Other(kind)
            }
            CRAFT_GRINDSTONE => {
                reader.i32_le()?;
                reader.u8()?;
                reader.var_i32()?;
                Self::Other(kind)
            }
            CRAFT_LOOM => {
                reader.string()?;
                reader.u8()?;
                Self::Other(kind)
            }
            CRAFT_RESULTS => {
                for _ in 0..list_len(reader, "craft result count")? {
                    skip_descriptor_item(reader)?;
                }
                reader.u8()?;
                Self::CraftResults
            }
            // Auto-crafting (11) carries ingredient descriptors; crafting is
            // not supported, so neither is reading past it.
            _ => {
                return Err(DecodeError::InvalidValue {
                    field: "item stack request action type",
                    value: kind.into(),
                });
            }
        })
    }

    fn write(&self, writer: &mut Writer) {
        use action_type::*;
        let header = |writer: &mut Writer, kind: u32| {
            writer.var_u32(kind);
            // The legacy ID skips the two retired container actions.
            writer.u8(if kind >= 7 {
                kind as u8 + 2
            } else {
                kind as u8
            });
        };
        match self {
            Self::Take {
                count,
                source,
                destination,
            }
            | Self::Place {
                count,
                source,
                destination,
            } => {
                header(
                    writer,
                    if matches!(self, Self::Take { .. }) {
                        TAKE
                    } else {
                        PLACE
                    },
                );
                writer.u8(*count);
                source.write(writer);
                destination.write(writer);
            }
            Self::Swap {
                source,
                destination,
            } => {
                header(writer, SWAP);
                source.write(writer);
                destination.write(writer);
            }
            Self::Drop {
                count,
                source,
                randomly,
            } => {
                header(writer, DROP);
                writer.u8(*count);
                source.write(writer);
                writer.bool(*randomly);
            }
            Self::Destroy { count, source } | Self::Consume { count, source } => {
                header(
                    writer,
                    if matches!(self, Self::Destroy { .. }) {
                        DESTROY
                    } else {
                        CONSUME
                    },
                );
                writer.u8(*count);
                source.write(writer);
            }
            Self::Create { results_slot } => {
                header(writer, CREATE);
                writer.u8(*results_slot);
            }
            Self::CraftCreative {
                creative_item,
                crafts,
            } => {
                header(writer, CRAFT_CREATIVE);
                writer.var_u32(*creative_item);
                writer.u8(*crafts);
            }
            Self::MineBlock {
                hotbar_slot,
                predicted_durability,
                stack_id,
            } => {
                header(writer, MINE_BLOCK);
                writer.var_i32(*hotbar_slot);
                writer.var_i32(*predicted_durability);
                writer.i32_le(*stack_id);
            }
            Self::CraftResults => {
                header(writer, CRAFT_RESULTS);
                writer.var_u32(0);
                writer.u8(1);
            }
            // Only actions without a payload can be written back.
            Self::Other(kind) => header(writer, *kind),
        }
    }
}

/// Steps over an item in the descriptor format of craft-result actions.
fn skip_descriptor_item(reader: &mut Reader<'_>) -> Result<(), DecodeError> {
    let variant = reader.var_u32()?;
    reader.u8()?;
    match variant {
        // Invalid: no item.
        0 => {}
        // Default: a name and metadata.
        1 => {
            reader.string()?;
            reader.var_i32()?;
        }
        other => {
            return Err(DecodeError::InvalidValue {
                field: "item descriptor type",
                value: other.into(),
            });
        }
    }
    // Count, block runtime ID and user data.
    reader.u16_le()?;
    reader.var_u32()?;
    reader.byte_array()?;
    Ok(())
}

/// One request: actions the client already carried out on its side, which
/// the server must accept or reject as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackRequest {
    /// Negative and odd; echoed in the response.
    pub id: i32,
    pub actions: Vec<StackAction>,
    /// Text for the profanity filter, such as an anvil rename.
    pub filter_strings: Vec<String>,
    pub filter_cause: i32,
}

impl StackRequest {
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let id = reader.var_i32()?;
        let actions = (0..list_len(reader, "item stack action count")?)
            .map(|_| StackAction::read(reader))
            .collect::<Result<_, _>>()?;
        let filter_strings = (0..list_len(reader, "filter string count")?)
            .map(|_| reader.string().map(str::to_owned))
            .collect::<Result<_, _>>()?;
        let filter_cause = reader.i32_le()?;
        Ok(Self {
            id,
            actions,
            filter_strings,
            filter_cause,
        })
    }

    pub fn write(&self, writer: &mut Writer) {
        writer.var_i32(self.id);
        writer.var_u32(len_u32(self.actions.len()));
        for action in &self.actions {
            action.write(writer);
        }
        writer.var_u32(len_u32(self.filter_strings.len()));
        for string in &self.filter_strings {
            writer.string(string);
        }
        writer.i32_le(self.filter_cause);
    }
}

/// Requests to move items, which the client buffers and sends together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemStackRequest {
    pub requests: Vec<StackRequest>,
}

impl Packet for ItemStackRequest {
    const ID: u32 = id::ITEM_STACK_REQUEST;
}

impl Decode for ItemStackRequest {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let requests = (0..list_len(reader, "item stack request count")?)
            .map(|_| StackRequest::read(reader))
            .collect::<Result<_, _>>()?;
        Ok(Self { requests })
    }
}

impl Encode for ItemStackRequest {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u32(len_u32(self.requests.len()));
        for request in &self.requests {
            request.write(writer);
        }
    }
}

/// What a slot holds after a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotResponse {
    pub slot: u8,
    pub count: u8,
    /// 0 for an empty slot.
    pub stack_id: i32,
}

impl SlotResponse {
    fn write(&self, writer: &mut Writer) {
        writer.u8(self.slot);
        // The "hotbar slot", always the same slot.
        writer.u8(self.slot);
        writer.u8(self.count);
        writer.bool(self.stack_id > 0);
        if self.stack_id > 0 {
            writer.var_i32(self.stack_id);
        }
        // No custom name, no filtered name, no durability correction.
        writer.string("");
        writer.bool(false);
        writer.var_i32(0);
    }
}

/// The changed slots of one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerResponse {
    pub container: FullContainerName,
    pub slots: Vec<SlotResponse>,
}

/// The server's verdict on one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackResponse {
    /// 0 accepts the request; anything else rejects it, and the client undoes it.
    pub status: u8,
    pub request_id: i32,
    /// For an accepted request, what the changed slots hold now.
    pub containers: Vec<ContainerResponse>,
}

impl StackResponse {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;

    pub fn rejected(request_id: i32) -> Self {
        Self {
            status: Self::ERROR,
            request_id,
            containers: Vec::new(),
        }
    }
}

/// The server's answers to item stack requests, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemStackResponse {
    pub responses: Vec<StackResponse>,
}

impl Packet for ItemStackResponse {
    const ID: u32 = id::ITEM_STACK_RESPONSE;
}

impl Encode for ItemStackResponse {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u32(len_u32(self.responses.len()));
        for response in &self.responses {
            writer.u8(response.status);
            writer.var_i32(response.request_id);
            writer.bool(!response.containers.is_empty());
            if !response.containers.is_empty() {
                writer.var_u32(len_u32(response.containers.len()));
                for container in &response.containers {
                    container.container.write(writer);
                    writer.var_u32(len_u32(container.slots.len()));
                    for slot in &container.slots {
                        slot.write(writer);
                    }
                }
            }
        }
    }
}

/// An item as the creative inventory lists it: without a stack ID, and with
/// a varint item ID, unlike [`ItemInstance`](crate::packets::ItemInstance).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CreativeItemStack {
    pub network_id: i16,
    pub count: u16,
    pub metadata: u32,
    pub block_runtime_id: u32,
    /// Shields carry an extra field in their user data.
    pub shield: bool,
    /// The item's NBT, as in [`ItemInstance`](crate::packets::ItemInstance).
    pub nbt: Option<&'static [u8]>,
}

impl CreativeItemStack {
    fn write(&self, writer: &mut Writer) {
        writer.var_i32(self.network_id.into());
        writer.u16_le(self.count);
        writer.var_u32(self.metadata);
        // The block runtime ID is signed here; hashed IDs keep their bits.
        writer.var_i32(self.block_runtime_id as i32);
        crate::packets::inventory::write_user_data(
            writer,
            self.network_id != 0,
            self.shield,
            self.nbt,
        );
    }
}

/// A group of the creative inventory, such as "planks".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreativeGroup {
    /// 1 construction, 2 nature, 3 equipment, 4 items.
    pub category: u8,
    /// A translation key; empty for items outside any group.
    pub name: String,
    pub icon: CreativeItemStack,
}

/// An item of the creative inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreativeItem {
    /// Unique and above 0; the client picks items by it.
    pub network_id: u32,
    pub item: CreativeItemStack,
    /// Index into the groups.
    pub group: u32,
}

/// The creative inventory: its groups and items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreativeContent {
    pub groups: Vec<CreativeGroup>,
    pub items: Vec<CreativeItem>,
}

impl Packet for CreativeContent {
    const ID: u32 = id::CREATIVE_CONTENT;
}

impl Encode for CreativeContent {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u32(len_u32(self.groups.len()));
        for group in &self.groups {
            writer.u8(group.category);
            writer.string(&group.name);
            group.icon.write(writer);
        }
        writer.var_u32(len_u32(self.items.len()));
        for item in &self.items {
            writer.var_u32(item.network_id);
            item.item.write(writer);
            writer.var_u32(item.group);
        }
    }
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("lists are far shorter than 4 billion entries")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn slot(container: u8, slot: u8, stack_id: i32) -> StackSlot {
        StackSlot {
            container: FullContainerName::new(container),
            slot,
            stack_id,
        }
    }

    #[test]
    fn requests_round_trip() {
        let packet = ItemStackRequest {
            requests: vec![
                StackRequest {
                    id: -1,
                    actions: vec![StackAction::Take {
                        count: 32,
                        source: slot(container::HOTBAR, 0, 1),
                        destination: slot(container::CURSOR, 0, 0),
                    }],
                    filter_strings: Vec::new(),
                    filter_cause: 0,
                },
                StackRequest {
                    id: -3,
                    actions: vec![
                        StackAction::CraftCreative {
                            creative_item: 7,
                            crafts: 1,
                        },
                        StackAction::CraftResults,
                        StackAction::Place {
                            count: 64,
                            source: slot(container::CREATED_OUTPUT, 50, -3),
                            destination: slot(container::INVENTORY, 12, 0),
                        },
                        StackAction::Destroy {
                            count: 1,
                            source: slot(container::CURSOR, 0, 9),
                        },
                    ],
                    filter_strings: vec!["name".into()],
                    filter_cause: 4,
                },
            ],
        };
        let bytes = packet.encode();
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::ITEM_STACK_REQUEST);
        let decoded = decode::<ItemStackRequest>(payload).unwrap();
        // Craft results lose their contents; the rest is kept.
        assert_eq!(decoded, packet);
    }

    #[test]
    fn take_actions_have_the_documented_layout() {
        let request = StackRequest {
            id: -5,
            actions: vec![StackAction::Take {
                count: 2,
                source: slot(container::HOTBAR, 3, 7),
                destination: slot(container::CURSOR, 0, 0),
            }],
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        let mut writer = Writer::new();
        request.write(&mut writer);
        assert_eq!(
            writer.into_bytes(),
            [
                9, // request ID -5, zigzag
                1, // one action
                0, 0, // type 0 (take), legacy ID 0
                2, // count
                28, 0, 3, 7, 0, 0, 0, // hotbar, no dynamic ID, slot 3, stack 7
                59, 0, 0, 0, 0, 0, 0, // cursor slot 0, no stack
                0, // no filter strings
                0, 0, 0, 0, // filter cause
            ]
        );
    }

    #[test]
    fn responses_list_containers_only_when_accepted() {
        let response = ItemStackResponse {
            responses: vec![
                StackResponse {
                    status: StackResponse::OK,
                    request_id: -1,
                    containers: vec![ContainerResponse {
                        container: FullContainerName::new(container::CURSOR),
                        slots: vec![SlotResponse {
                            slot: 0,
                            count: 32,
                            stack_id: 4,
                        }],
                    }],
                },
                StackResponse::rejected(-3),
            ],
        };
        let bytes = response.encode();
        let (_, mut payload) = read_header(&bytes).unwrap();
        assert_eq!(
            payload.take(payload.remaining()).unwrap(),
            [
                2, // two responses
                0, 1, 1, // OK, request -1, containers present
                1, 59, 0, // one container: the cursor
                1, 0, 0, 32, 1, 8, 0, 0,
                0, // slot 0 ×2, 32 items, stack 4, names, durability
                1, 5, 0, // error, request -3, no containers
            ]
        );
    }

    #[test]
    fn unsupported_actions_are_errors() {
        let mut writer = Writer::new();
        writer.var_i32(-1);
        writer.var_u32(1);
        writer.var_u32(11); // auto-craft
        writer.u8(13);
        let bytes = writer.into_bytes();
        assert!(StackRequest::read(&mut Reader::new(&bytes)).is_err());
    }
}
