//! Players as vanilla saves them: a root NBT compound, little-endian, with
//! vanilla's names for what BedrockRS remembers.
//!
//! - `Pos`: three floats, the position at eye level, 1.62 above the feet, as
//!   Bedrock positions players on the network (a vanilla 1.21 world's player
//!   standing at y = 86 had 87.62);
//! - `Rotation`: yaw, then pitch;
//! - `abilities`: `{flying}`;
//! - `PlayerGameMode`: the game mode's number (0, 1, 2, 6);
//! - `Attributes`: `minecraft:health`, whose `Current` is the health;
//! - `Inventory` (with each item's `Slot`), `Armor` (head, chest, legs, feet,
//!   and the body slot vanilla now has too, always empty for players) and
//!   `Offhand`: item stacks, `{Name, Count, Damage, WasPickedUp, tag}`, an
//!   empty slot `{Name: "", Count: 0}`.
//!
//! What BedrockRS does not track (a vanilla player's XP, effects, spawn
//! point and the like) is not written, and is ignored when read.

use bedrockrs_protocol::nbt::{Compound, Tag, TagKind};

use super::{SavedInventory, SavedPlayer, SavedStack, StoreError};
use crate::game_mode::GameMode;
use crate::players::EYE_HEIGHT;

const HEALTH: &str = "minecraft:health";
const MAX_HEALTH: f32 = 20.0;
/// Armour slots as vanilla saves them: head, chest, legs, feet, body.
const ARMOR_SLOTS: u8 = 5;

/// `player` as vanilla's player NBT.
pub fn encode(player: &SavedPlayer) -> Compound {
    let mut compound = Compound::new()
        .with("Pos", floats(&[player.x, player.y + EYE_HEIGHT, player.z]))
        .with("Rotation", floats(&[player.yaw, player.pitch]))
        .with(
            "abilities",
            Tag::Compound(Compound::new().with("flying", Tag::Byte(player.flying.into()))),
        );
    if let Some(mode) = player.game_mode.as_deref().and_then(GameMode::from_name) {
        compound = compound.with("PlayerGameMode", Tag::Int(mode.id()));
    }
    if let Some(health) = player.health {
        compound = compound.with(
            "Attributes",
            Tag::List(
                TagKind::Compound,
                vec![Tag::Compound(health_attribute(health))],
            ),
        );
    }
    if let Some(inventory) = &player.inventory {
        let slot = |stacks: &[SavedStack], slot: u8| {
            stacks
                .iter()
                .find(|stack| stack.slot == slot)
                .map_or_else(empty_item, |stack| item(stack, false))
        };
        compound = compound
            .with(
                "Inventory",
                compounds(inventory.main.iter().map(|stack| item(stack, true))),
            )
            .with(
                "Armor",
                compounds((0..ARMOR_SLOTS).map(|index| slot(&inventory.armor, index))),
            )
            .with("Offhand", compounds([slot(&inventory.offhand, 0)]));
    }
    compound
}

/// Vanilla's player NBT back to what BedrockRS remembers.
pub fn decode(compound: &Compound) -> Result<SavedPlayer, StoreError> {
    let [x, eyes, z] =
        float_list(compound.get("Pos")).ok_or(StoreError::Player("no position (Pos)"))?;
    let [yaw, pitch] = float_list(compound.get("Rotation")).unwrap_or([0.0, 0.0]);
    let flying = match compound.get("abilities") {
        Some(Tag::Compound(abilities)) => {
            matches!(abilities.get("flying"), Some(Tag::Byte(flying)) if *flying != 0)
        }
        _ => false,
    };
    let game_mode = match compound.get("PlayerGameMode") {
        Some(Tag::Int(id)) => GameMode::from_id(*id).map(|mode| mode.name().to_owned()),
        _ => None,
    };
    let health = match compound.get("Attributes") {
        Some(Tag::List(_, attributes)) => attributes.iter().find_map(|attribute| {
            let Tag::Compound(attribute) = attribute else {
                return None;
            };
            let is_health =
                matches!(attribute.get("Name"), Some(Tag::String(name)) if name == HEALTH);
            match attribute.get("Current") {
                Some(Tag::Float(current)) if is_health => Some(*current),
                _ => None,
            }
        }),
        _ => None,
    };
    let inventory = match compound.get("Inventory") {
        Some(Tag::List(_, main)) => Some(SavedInventory {
            main: stacks(main, |_, item| match item.get("Slot") {
                Some(Tag::Byte(slot)) => Some(*slot as u8),
                _ => None,
            }),
            armor: stacks(list(compound.get("Armor")), |index, _| {
                u8::try_from(index).ok()
            }),
            offhand: stacks(list(compound.get("Offhand")), |index, _| {
                (index == 0).then_some(0)
            }),
        }),
        _ => None,
    };
    Ok(SavedPlayer {
        x,
        y: eyes - EYE_HEIGHT,
        z,
        pitch,
        yaw,
        // Vanilla keeps one yaw for the body and head.
        head_yaw: yaw,
        flying,
        inventory,
        game_mode,
        health,
    })
}

fn floats(values: &[f32]) -> Tag {
    Tag::List(
        TagKind::Float,
        values.iter().map(|value| Tag::Float(*value)).collect(),
    )
}

fn float_list<const N: usize>(tag: Option<&Tag>) -> Option<[f32; N]> {
    let Some(Tag::List(_, items)) = tag else {
        return None;
    };
    let values: Vec<f32> = items
        .iter()
        .map(|item| match item {
            Tag::Float(value) => Some(*value),
            _ => None,
        })
        .collect::<Option<_>>()?;
    values.try_into().ok()
}

fn compounds(items: impl IntoIterator<Item = Compound>) -> Tag {
    Tag::List(
        TagKind::Compound,
        items.into_iter().map(Tag::Compound).collect(),
    )
}

fn list(tag: Option<&Tag>) -> &[Tag] {
    match tag {
        Some(Tag::List(_, items)) => items,
        _ => &[],
    }
}

fn health_attribute(health: f32) -> Compound {
    Compound::new()
        .with("Base", Tag::Float(MAX_HEALTH))
        .with("Current", Tag::Float(health))
        .with("DefaultMax", Tag::Float(MAX_HEALTH))
        .with("DefaultMin", Tag::Float(0.0))
        .with("Max", Tag::Float(MAX_HEALTH))
        .with("Min", Tag::Float(0.0))
        .with("Name", Tag::String(HEALTH.into()))
}

/// An item stack as vanilla saves it, with its slot if `with_slot`.
fn item(stack: &SavedStack, with_slot: bool) -> Compound {
    let mut item = Compound::new()
        .with("Count", Tag::Byte(stack.count as i8))
        .with("Damage", Tag::Short(stack.meta as i16))
        .with("Name", Tag::String(stack.item.clone()))
        .with("WasPickedUp", Tag::Byte(0));
    if with_slot {
        item = item.with("Slot", Tag::Byte(stack.slot as i8));
    }
    if let Some(nbt) = &stack.nbt {
        match Compound::from_le_bytes(nbt) {
            Ok(tag) => item = item.with("tag", Tag::Compound(tag)),
            Err(err) => tracing::warn!("Left out the unreadable NBT of a {}: {err}", stack.item),
        }
    }
    item
}

/// What vanilla saves in an empty armour or offhand slot.
fn empty_item() -> Compound {
    Compound::new()
        .with("Count", Tag::Byte(0))
        .with("Damage", Tag::Short(0))
        .with("Name", Tag::String(String::new()))
        .with("WasPickedUp", Tag::Byte(0))
}

/// The stacks in a list of items, each in the slot `slot` gives it (from
/// its place in the list and the item), skipping empty ones.
fn stacks(items: &[Tag], slot: impl Fn(usize, &Compound) -> Option<u8>) -> Vec<SavedStack> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let Tag::Compound(item) = item else {
                return None;
            };
            let Some(Tag::String(name)) = item.get("Name") else {
                return None;
            };
            let count = match item.get("Count") {
                Some(Tag::Byte(count)) if *count > 0 => *count as u8,
                _ => return None,
            };
            if name.is_empty() {
                return None;
            }
            let meta = match item.get("Damage") {
                Some(Tag::Short(damage)) => u32::from(*damage as u16),
                _ => 0,
            };
            let nbt = match item.get("tag") {
                Some(Tag::Compound(tag)) => Some(tag.to_le_bytes()),
                _ => None,
            };
            Some(SavedStack {
                slot: slot(index, item)?,
                item: name.clone(),
                count,
                meta,
                nbt,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player() -> SavedPlayer {
        let stack = |slot, item: &str, count| SavedStack {
            slot,
            item: item.into(),
            count,
            meta: 0,
            nbt: None,
        };
        SavedPlayer {
            x: 120.5,
            y: -60.0,
            z: -33.25,
            pitch: 10.0,
            yaw: -90.0,
            head_yaw: -90.0,
            flying: true,
            inventory: Some(SavedInventory {
                main: vec![
                    stack(4, "minecraft:stone", 12),
                    SavedStack {
                        meta: 3,
                        nbt: Some(
                            Compound::new()
                                .with("RepairCost", Tag::Int(1))
                                .to_le_bytes(),
                        ),
                        ..stack(30, "minecraft:enchanted_book", 1)
                    },
                ],
                armor: vec![stack(1, "minecraft:iron_chestplate", 1)],
                offhand: vec![stack(0, "minecraft:shield", 1)],
            }),
            game_mode: Some("adventure".into()),
            health: Some(13.0),
        }
    }

    #[test]
    fn players_survive_vanilla_nbt() {
        let player = player();
        let compound = encode(&player);
        let bytes = compound.to_le_bytes();
        let mut loaded = decode(&Compound::from_le_bytes(&bytes).unwrap()).unwrap();
        // The eye height comes off again, give or take a rounding.
        assert!((loaded.y - player.y).abs() < 1e-4, "{}", loaded.y);
        loaded.y = player.y;
        assert_eq!(loaded, player);
    }

    #[test]
    fn players_are_saved_with_vanilla_names() {
        let compound = encode(&player());
        assert_eq!(
            compound.get("Pos"),
            Some(&floats(&[120.5, -60.0 + EYE_HEIGHT, -33.25])),
            "at eye level"
        );
        assert_eq!(compound.get("Rotation"), Some(&floats(&[-90.0, 10.0])));
        assert_eq!(compound.get("PlayerGameMode"), Some(&Tag::Int(2)));
        // Armour is always five slots, empty ones included, as vanilla's.
        let Some(Tag::List(TagKind::Compound, armor)) = compound.get("Armor") else {
            panic!("no armour list");
        };
        assert_eq!(armor.len(), 5);
        assert_eq!(armor[0], Tag::Compound(empty_item()));
        let Some(Tag::List(_, inventory)) = compound.get("Inventory") else {
            panic!("no inventory");
        };
        let Tag::Compound(book) = &inventory[1] else {
            panic!("not an item");
        };
        assert_eq!(book.get("Slot"), Some(&Tag::Byte(30)));
        assert_eq!(book.get("Damage"), Some(&Tag::Short(3)));
        assert!(matches!(book.get("tag"), Some(Tag::Compound(_))));
    }

    #[test]
    fn every_creative_items_nbt_comes_back_byte_for_byte() {
        // Stacks are the same item only with the same NBT bytes, so what a
        // player carried must read back exactly as it was.
        let mut checked = 0;
        for id in 1.. {
            let Some(entry) = crate::items::items().creative(id) else {
                break;
            };
            let Some(nbt) = entry.nbt else { continue };
            let tag = Compound::from_le_bytes(nbt).unwrap();
            assert_eq!(tag.to_le_bytes(), nbt, "creative item {id}");
            checked += 1;
        }
        assert!(checked > 100, "only {checked} items with NBT");
    }

    #[test]
    fn what_vanilla_leaves_out_takes_the_defaults() {
        // Only a position: not flying, no inventory, default mode, full health.
        let bare = Compound::new().with("Pos", floats(&[1.0, 70.62, 2.0]));
        let player = decode(&bare).unwrap();
        assert!((player.y - 69.0).abs() < 1e-4);
        assert!(!player.flying);
        assert_eq!(player.inventory, None);
        assert_eq!(player.game_mode, None);
        assert_eq!(player.health, None);
        // Vanilla's "default" game mode (5) is the server's default.
        let default = bare.clone().with("PlayerGameMode", Tag::Int(5));
        assert_eq!(decode(&default).unwrap().game_mode, None);
        // Spectators are 6.
        let spectator = bare.with("PlayerGameMode", Tag::Int(6));
        assert_eq!(
            decode(&spectator).unwrap().game_mode.as_deref(),
            Some("spectator")
        );
        assert!(matches!(
            decode(&Compound::new()),
            Err(StoreError::Player(_))
        ));
    }
}
