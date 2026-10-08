//! Showing players to each other: the player list, and adding and removing
//! player entities.

use uuid::Uuid;

use crate::io::Writer;
use crate::packet::{Encode, Packet, id};
use crate::packets::ItemInstance;
use crate::types::{Vec3, uuid_bytes};

/// Entity metadata keys, as numbered on the wire.
pub mod metadata_key {
    pub const FLAGS: u32 = 0;
    /// An int: for a falling block, the network ID of its block.
    pub const VARIANT: u32 = 2;
    pub const NAME: u32 = 4;
    pub const SCALE: u32 = 38;
    pub const WIDTH: u32 = 53;
    pub const HEIGHT: u32 = 54;
    /// A byte: 1 shows the name tag without the viewer aiming at the entity.
    pub const ALWAYS_SHOW_NAME_TAG: u32 = 81;
}

/// Bits of the [`metadata_key::FLAGS`] long, as gophertunnel and PocketMine
/// both number them.
pub mod entity_flag {
    pub const SNEAKING: u32 = 1;
    pub const SHOW_NAME: u32 = 14;
    pub const ALWAYS_SHOW_NAME: u32 = 15;
    pub const CAN_CLIMB: u32 = 19;
    pub const BREATHING: u32 = 35;
    pub const HAS_COLLISION: u32 = 48;
    /// Without it, the client does not pull the entity down, the local player included.
    pub const HAS_GRAVITY: u32 = 49;

    /// The flags long with each listed bit set.
    pub fn bits(flags: &[u32]) -> i64 {
        flags.iter().fold(0, |bits, flag| bits | (1 << flag))
    }
}

/// Ability bits of an [`AbilityLayer`], as gophertunnel and PocketMine number them.
pub mod ability {
    pub const BUILD: u32 = 1 << 0;
    pub const MINE: u32 = 1 << 1;
    pub const DOORS_AND_SWITCHES: u32 = 1 << 2;
    pub const OPEN_CONTAINERS: u32 = 1 << 3;
    pub const ATTACK_PLAYERS: u32 = 1 << 4;
    pub const ATTACK_MOBS: u32 = 1 << 5;
    pub const OPERATOR_COMMANDS: u32 = 1 << 6;
    pub const TELEPORT: u32 = 1 << 7;
    pub const INVULNERABLE: u32 = 1 << 8;
    pub const FLYING: u32 = 1 << 9;
    pub const MAY_FLY: u32 = 1 << 10;
    pub const INSTANT_BUILD: u32 = 1 << 11;
    pub const NO_CLIP: u32 = 1 << 17;
    /// Every one of the 20 abilities.
    pub const ALL: u32 = (1 << 20) - 1;

    /// Vanilla speeds, in blocks per tick.
    pub const WALK_SPEED: f32 = 0.1;
    pub const FLY_SPEED: f32 = 0.05;
    pub const VERTICAL_FLY_SPEED: f32 = 1.0;
}

/// One layer of a player's abilities. `abilities` says which abilities the
/// layer defines, `values` which of those are granted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbilityLayer {
    /// 1 is the base layer.
    pub layer: u16,
    pub abilities: u32,
    pub values: u32,
    pub fly_speed: f32,
    pub vertical_fly_speed: f32,
    pub walk_speed: f32,
}

impl AbilityLayer {
    /// A base layer defining every ability, granting `values`, at vanilla speeds.
    pub fn base(values: u32) -> Self {
        Self {
            layer: 1,
            abilities: ability::ALL,
            values,
            fly_speed: ability::FLY_SPEED,
            vertical_fly_speed: ability::VERTICAL_FLY_SPEED,
            walk_speed: ability::WALK_SPEED,
        }
    }
}

/// A player's permissions and ability layers.
#[derive(Debug, Clone, PartialEq)]
pub struct AbilityData {
    pub entity_unique_id: i64,
    /// 0 visitor, 1 member, 2 operator.
    pub player_permissions: u8,
    pub command_permissions: u8,
    pub layers: Vec<AbilityLayer>,
}

impl AbilityData {
    fn write(&self, writer: &mut Writer) {
        writer.i64_le(self.entity_unique_id);
        writer.u8(self.player_permissions);
        writer.u8(self.command_permissions);
        writer.var_u32(len_u32(self.layers.len()));
        for layer in &self.layers {
            writer.u16_le(layer.layer);
            writer.u32_le(layer.abilities);
            writer.u32_le(layer.values);
            writer.f32_le(layer.fly_speed);
            writer.f32_le(layer.vertical_fly_speed);
            writer.f32_le(layer.walk_speed);
        }
    }
}

/// Tells the client what its own player may do and how fast it walks and flies.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateAbilities(pub AbilityData);

impl Packet for UpdateAbilities {
    const ID: u32 = id::UPDATE_ABILITIES;
}

impl Encode for UpdateAbilities {
    fn encode_payload(&self, writer: &mut Writer) {
        self.0.write(writer);
    }
}

/// Sets the game mode of the client's own player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetPlayerGameType {
    pub game_type: i32,
}

impl Packet for SetPlayerGameType {
    const ID: u32 = id::SET_PLAYER_GAME_TYPE;
}

impl Encode for SetPlayerGameType {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i32(self.game_type);
    }
}

/// Tells clients another player's game mode changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatePlayerGameType {
    pub game_type: i32,
    pub player_unique_id: i64,
    pub tick: u64,
}

impl Packet for UpdatePlayerGameType {
    const ID: u32 = id::UPDATE_PLAYER_GAME_TYPE;
}

impl Encode for UpdatePlayerGameType {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i32(self.game_type);
        writer.var_i64(self.player_unique_id);
        writer.var_u64(self.tick);
    }
}

/// An entity attribute such as `minecraft:movement`, with its range and default.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub value: f32,
    pub default_min: f32,
    pub default_max: f32,
    pub default: f32,
}

impl Attribute {
    /// An attribute from `min` to `max` currently at its default `value`.
    pub fn at_default(name: impl Into<String>, min: f32, max: f32, value: f32) -> Self {
        Self {
            name: name.into(),
            min,
            max,
            value,
            default_min: min,
            default_max: max,
            default: value,
        }
    }
}

/// Sets attributes of an entity; for the player's own entity, `minecraft:movement`
/// is the speed its client walks at.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateAttributes {
    pub entity_runtime_id: u64,
    pub attributes: Vec<Attribute>,
    pub tick: u64,
}

impl Packet for UpdateAttributes {
    const ID: u32 = id::UPDATE_ATTRIBUTES;
}

impl Encode for UpdateAttributes {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        writer.var_u32(len_u32(self.attributes.len()));
        for attribute in &self.attributes {
            writer.f32_le(attribute.min);
            writer.f32_le(attribute.max);
            writer.f32_le(attribute.value);
            writer.f32_le(attribute.default_min);
            writer.f32_le(attribute.default_max);
            writer.f32_le(attribute.default);
            writer.string(&attribute.name);
            // No modifiers.
            writer.var_u32(0);
        }
        writer.var_u64(self.tick);
    }
}

/// Updates an entity's metadata, including the player's own entity.
#[derive(Debug, Clone, PartialEq)]
pub struct SetActorData {
    pub entity_runtime_id: u64,
    pub metadata: EntityMetadata,
    /// The server tick the data belongs to.
    pub tick: u64,
}

impl Packet for SetActorData {
    const ID: u32 = id::SET_ACTOR_DATA;
}

impl Encode for SetActorData {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        self.metadata.write(writer);
        // No integer or float entity properties.
        writer.var_u32(0);
        writer.var_u32(0);
        writer.var_u64(self.tick);
    }
}

/// A value of entity metadata.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataValue {
    Byte(u8),
    Int(i32),
    Float(f32),
    String(String),
    Long(i64),
}

impl MetadataValue {
    fn type_id(&self) -> u8 {
        match self {
            Self::Byte(_) => 0,
            Self::Int(_) => 2,
            Self::Float(_) => 3,
            Self::String(_) => 4,
            Self::Long(_) => 7,
        }
    }
}

/// An entity's synced data (name, size, flags…), as `(key, value)` pairs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntityMetadata(pub Vec<(u32, MetadataValue)>);

impl EntityMetadata {
    /// Each entry is the key, the value's type (as a varuint32 variant and
    /// again as a byte), then the value. Entries go in key order.
    pub fn write(&self, writer: &mut Writer) {
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort_by_key(|(key, _)| *key);
        writer.var_u32(len_u32(entries.len()));
        for (key, value) in entries {
            writer.var_u32(*key);
            writer.var_u32(value.type_id().into());
            writer.u8(value.type_id());
            match value {
                MetadataValue::Byte(byte) => writer.u8(*byte),
                MetadataValue::Int(int) => writer.var_i32(*int),
                MetadataValue::Float(float) => writer.f32_le(*float),
                MetadataValue::String(text) => writer.string(text),
                MetadataValue::Long(long) => writer.var_i64(*long),
            }
        }
    }
}

/// Shows another player's entity to the client. Send a [`PlayerList`] entry
/// for the same UUID first so the client has their skin.
#[derive(Debug, Clone, PartialEq)]
pub struct AddPlayer {
    pub uuid: Uuid,
    pub username: String,
    pub entity_runtime_id: u64,
    /// Where the player's feet are.
    pub position: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub game_mode: i32,
    pub metadata: EntityMetadata,
    /// What the player holds.
    pub held_item: ItemInstance,
    /// Unique ID for the ability data; BedrockRS uses the runtime ID.
    pub entity_unique_id: i64,
}

impl Packet for AddPlayer {
    const ID: u32 = id::ADD_PLAYER;
}

impl Encode for AddPlayer {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.uuid(uuid_bytes(&self.uuid));
        writer.string(&self.username);
        writer.var_u64(self.entity_runtime_id);
        // Platform chat ID.
        writer.string("");
        self.position.write(writer);
        // Velocity.
        Vec3::default().write(writer);
        writer.f32_le(self.pitch);
        writer.f32_le(self.yaw);
        writer.f32_le(self.head_yaw);
        self.held_item.write(writer);
        writer.var_i32(self.game_mode);
        self.metadata.write(writer);
        // No integer or float entity properties.
        writer.var_u32(0);
        writer.var_u32(0);
        // Ability data: a base layer defining every ability, as Dragonfly sends.
        AbilityData {
            entity_unique_id: self.entity_unique_id,
            player_permissions: 1,
            command_permissions: 0,
            layers: vec![AbilityLayer::base(0)],
        }
        .write(writer);
        // No entity links, no device ID, unknown build platform.
        writer.var_u32(0);
        writer.string("");
        writer.i32_le(-1);
    }
}

/// Removes an entity from the client's world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveActor {
    pub entity_unique_id: i64,
}

impl Packet for RemoveActor {
    const ID: u32 = id::REMOVE_ACTOR;
}

impl Encode for RemoveActor {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i64(self.entity_unique_id);
    }
}

/// Resource patch naming the geometry a classic skin uses.
const RESOURCE_PATCH: &str = r#"{"geometry":{"default":"geometry.humanoid.custom"}}"#;

/// The classic (wide-armed) 64×64 player model, defining the geometry the
/// resource patch names. Clients reject a skin whose geometry data is not JSON;
/// vanilla clients send a full definition like this one for classic skins.
/// Held items are drawn at the `rightItem` and `leftItem` bones: without them,
/// other players' arms rise but their items are invisible (found live on
/// 2026-09-27).
const HUMANOID_GEOMETRY: &str = r#"{"format_version":"1.12.0","minecraft:geometry":[{"description":{"identifier":"geometry.humanoid.custom","texture_width":64,"texture_height":64,"visible_bounds_width":1,"visible_bounds_height":2,"visible_bounds_offset":[0,1,0]},"bones":[
{"name":"root","pivot":[0,0,0]},
{"name":"waist","parent":"root","pivot":[0,12,0]},
{"name":"body","parent":"waist","pivot":[0,24,0],"cubes":[{"origin":[-4,12,-2],"size":[8,12,4],"uv":[16,16]}]},
{"name":"jacket","parent":"body","pivot":[0,24,0],"cubes":[{"origin":[-4,12,-2],"size":[8,12,4],"uv":[16,32],"inflate":0.25}]},
{"name":"head","parent":"body","pivot":[0,24,0],"cubes":[{"origin":[-4,24,-4],"size":[8,8,8],"uv":[0,0]}]},
{"name":"hat","parent":"head","pivot":[0,24,0],"cubes":[{"origin":[-4,24,-4],"size":[8,8,8],"uv":[32,0],"inflate":0.5}]},
{"name":"rightArm","parent":"body","pivot":[-5,22,0],"cubes":[{"origin":[-8,12,-2],"size":[4,12,4],"uv":[40,16]}]},
{"name":"rightSleeve","parent":"rightArm","pivot":[-5,22,0],"cubes":[{"origin":[-8,12,-2],"size":[4,12,4],"uv":[40,32],"inflate":0.25}]},
{"name":"leftArm","parent":"body","pivot":[5,22,0],"cubes":[{"origin":[4,12,-2],"size":[4,12,4],"uv":[32,48]}]},
{"name":"leftSleeve","parent":"leftArm","pivot":[5,22,0],"cubes":[{"origin":[4,12,-2],"size":[4,12,4],"uv":[48,48],"inflate":0.25}]},
{"name":"rightItem","parent":"rightArm","pivot":[-6,15,1]},
{"name":"leftItem","parent":"leftArm","pivot":[6,15,1]},
{"name":"rightLeg","parent":"root","pivot":[-1.9,12,0],"cubes":[{"origin":[-3.9,0,-2],"size":[4,12,4],"uv":[0,16]}]},
{"name":"rightPants","parent":"rightLeg","pivot":[-1.9,12,0],"cubes":[{"origin":[-3.9,0,-2],"size":[4,12,4],"uv":[0,32],"inflate":0.25}]},
{"name":"leftLeg","parent":"root","pivot":[1.9,12,0],"cubes":[{"origin":[-0.1,0,-2],"size":[4,12,4],"uv":[16,48]}]},
{"name":"leftPants","parent":"leftLeg","pivot":[1.9,12,0],"cubes":[{"origin":[-0.1,0,-2],"size":[4,12,4],"uv":[0,48],"inflate":0.25}]}
]}]}"#;

/// Engine version the geometry is written for; clients send `0.0.0` for classic skins.
const GEOMETRY_ENGINE_VERSION: &str = "0.0.0";

/// A player's skin: what their client sent at login, or a plain placeholder.
/// Layout as gophertunnel encodes it for protocol 2193.
#[derive(Debug, Clone, PartialEq)]
pub struct Skin {
    /// Unique per skin; clients cache skins by it.
    pub id: String,
    pub play_fab_id: String,
    /// JSON naming the geometry to use, e.g. `{"geometry":{"default":…}}`.
    pub resource_patch: String,
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes of RGBA.
    pub data: Vec<u8>,
    pub animations: Vec<SkinAnimation>,
    pub cape: Option<Cape>,
    /// JSON geometry definitions the resource patch refers to.
    pub geometry: String,
    pub geometry_engine_version: String,
    pub animation_data: String,
    pub full_id: String,
    /// Wide (classic) arms rather than slim ones.
    pub wide_arms: bool,
    /// The base skin colour, as RGBA.
    pub colour: [u8; 4],
    /// The character-creator pieces the skin is made of.
    pub persona_pieces: Vec<PersonaPiece>,
    /// Colours of some of those pieces.
    pub piece_tints: Vec<PieceTint>,
    /// Made in the character creator.
    pub persona: bool,
    pub premium: bool,
    pub persona_cape_on_classic: bool,
}

/// An animated part of a skin, such as blinking eyes.
#[derive(Debug, Clone, PartialEq)]
pub struct SkinAnimation {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// 1 head, 2 body 32×32, 3 body 128×128.
    pub kind: u32,
    pub frames: f32,
    /// 0 linear, 1 blinking.
    pub expression: u32,
}

/// One piece of a character-creator skin, such as its hair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaPiece {
    pub id: String,
    /// One of [`persona_piece_type`].
    pub kind: u32,
    pub pack_id: Uuid,
    pub default: bool,
    pub product_id: String,
}

/// Character-creator piece types, as Mojang's `persona::PieceType` numbers them.
pub mod persona_piece_type {
    pub const UNKNOWN: u32 = 0;

    /// The type a login's `persona_…` name stands for.
    pub fn from_login(name: &str) -> u32 {
        const NAMES: [&str; 28] = [
            "unknown",
            "skeleton",
            "body",
            "skin",
            "bottom",
            "feet",
            "dress",
            "top",
            "high_pants",
            "hands",
            "outerwear",
            "facial_hair",
            "mouth",
            "eyes",
            "hair",
            "hood",
            "back",
            "face_accessory",
            "head",
            "legs",
            "left_leg",
            "right_leg",
            "arms",
            "left_arm",
            "right_arm",
            "capes",
            "classic_skin",
            "emote",
        ];
        let name = name.strip_prefix("persona_").unwrap_or(name);
        let name = if name == "hand" { "hands" } else { name };
        NAMES
            .iter()
            .position(|known| *known == name)
            .map_or(UNKNOWN, |index| index as u32)
    }
}

/// The four colours of a character-creator piece, as RGBA; unused ones are 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceTint {
    /// The piece type as the wire names it: the login's name without its
    /// `persona_` prefix, and `persona_hand` as `hands` (as gophertunnel sends).
    pub piece_type: String,
    pub colours: [[u8; 4]; 4],
}

impl PieceTint {
    /// The wire name of a login piece type.
    pub fn wire_type(login_type: &str) -> String {
        match login_type {
            "persona_hand" => "hands".to_owned(),
            other => other.strip_prefix("persona_").unwrap_or(other).to_owned(),
        }
    }
}

/// Writes an RGBA colour as a big-endian ARGB integer, as gophertunnel does.
fn write_colour(writer: &mut Writer, [red, green, blue, alpha]: [u8; 4]) {
    writer.u8(blue);
    writer.u8(green);
    writer.u8(red);
    writer.u8(alpha);
}

/// A cape: an RGBA image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cape {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Skin {
    /// A 64×64 skin of a single colour on the classic model, for players
    /// whose own skin could not be used.
    pub fn solid(id: impl Into<String>, rgba: [u8; 4]) -> Self {
        let id = id.into();
        Self {
            full_id: id.clone(),
            id,
            play_fab_id: String::new(),
            resource_patch: RESOURCE_PATCH.to_owned(),
            width: 64,
            height: 64,
            data: rgba.repeat(64 * 64),
            animations: Vec::new(),
            cape: None,
            geometry: HUMANOID_GEOMETRY.to_owned(),
            geometry_engine_version: GEOMETRY_ENGINE_VERSION.to_owned(),
            animation_data: String::new(),
            wide_arms: true,
            colour: [0; 4],
            persona_pieces: Vec::new(),
            piece_tints: Vec::new(),
            persona: false,
            premium: false,
            persona_cape_on_classic: false,
        }
    }

    fn write(&self, writer: &mut Writer) {
        writer.string(&self.id);
        writer.string(&self.play_fab_id);
        writer.string(&self.resource_patch);
        writer.u32_le(self.width);
        writer.u32_le(self.height);
        writer.byte_array(&self.data);
        writer.var_u32(len_u32(self.animations.len()));
        for animation in &self.animations {
            writer.u32_le(animation.width);
            writer.u32_le(animation.height);
            writer.byte_array(&animation.data);
            writer.var_u32(animation.kind);
            writer.f32_le(animation.frames);
            writer.var_u32(animation.expression);
        }
        let cape = self.cape.as_ref();
        writer.u32_le(cape.map_or(0, |cape| cape.width));
        writer.u32_le(cape.map_or(0, |cape| cape.height));
        writer.byte_array(cape.map_or(&[], |cape| &cape.data));
        writer.string(&self.geometry);
        writer.string(&self.geometry_engine_version);
        writer.string(&self.animation_data);
        writer.string(cape.map_or("", |cape| &cape.id));
        writer.string(&self.full_id);
        writer.u8(u8::from(self.wide_arms));
        write_colour(writer, self.colour);
        writer.var_u32(len_u32(self.persona_pieces.len()));
        for piece in &self.persona_pieces {
            writer.string(&piece.id);
            writer.u32_le(piece.kind);
            writer.uuid(uuid_bytes(&piece.pack_id));
            writer.bool(piece.default);
            writer.string(&piece.product_id);
        }
        writer.var_u32(len_u32(self.piece_tints.len()));
        for tint in &self.piece_tints {
            writer.string(&tint.piece_type);
            for colour in tint.colours {
                write_colour(writer, colour);
            }
        }
        writer.bool(self.premium);
        writer.bool(self.persona);
        writer.bool(self.persona_cape_on_classic);
        // Not the primary user; overrides the appearance, as Dragonfly sends.
        writer.bool(false);
        writer.bool(true);
        // Trusted, as a string.
        writer.string("true");
        // Profile hash.
        writer.string("");
    }
}

/// A player's skin, sent again after their entity is added, as Dragonfly
/// does, so the client applies it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSkin {
    pub uuid: Uuid,
    pub skin: Skin,
}

impl Packet for PlayerSkin {
    const ID: u32 = id::PLAYER_SKIN;
}

impl Encode for PlayerSkin {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.uuid(uuid_bytes(&self.uuid));
        self.skin.write(writer);
        // New and old skin names.
        writer.string("");
        writer.string("");
    }
}

/// A player shown in the client's player list, with the skin their entity uses.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerListEntry {
    pub uuid: Uuid,
    pub entity_unique_id: i64,
    pub username: String,
    pub skin: Skin,
}

/// Adds players to, or removes them from, the client's player list.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerList {
    Add(Vec<PlayerListEntry>),
    Remove(Vec<Uuid>),
}

impl Packet for PlayerList {
    const ID: u32 = id::PLAYER_LIST;
}

impl Encode for PlayerList {
    /// Each entry is a variant (1 add, 0 remove), the action byte (0 add, 1
    /// remove) and the UUID; additions carry the rest of the entry.
    fn encode_payload(&self, writer: &mut Writer) {
        match self {
            Self::Add(entries) => {
                writer.var_u32(len_u32(entries.len()));
                for entry in entries {
                    writer.var_u32(1);
                    writer.u8(0);
                    writer.uuid(uuid_bytes(&entry.uuid));
                    writer.var_i64(entry.entity_unique_id);
                    writer.string(&entry.username);
                    // No XUID or platform chat ID; unknown build platform.
                    writer.string("");
                    writer.string("");
                    writer.i32_le(-1);
                    entry.skin.write(writer);
                    // Not a teacher, host or sub-client; no player colour.
                    writer.bool(false);
                    writer.bool(false);
                    writer.bool(false);
                    writer.i32_be(0);
                }
            }
            Self::Remove(uuids) => {
                writer.var_u32(len_u32(uuids.len()));
                for uuid in uuids {
                    writer.var_u32(0);
                    writer.u8(1);
                    writer.uuid(uuid_bytes(uuid));
                }
            }
        }
    }
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("lists are shorter than 4 billion entries")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap()
    }

    #[test]
    fn uuids_are_two_little_endian_halves() {
        assert_eq!(
            uuid_bytes(&uuid()),
            [
                0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA,
                0x99, 0x88
            ]
        );
    }

    #[test]
    fn metadata_is_sorted_and_typed_twice() {
        let metadata = EntityMetadata(vec![
            (metadata_key::SCALE, MetadataValue::Float(1.0)),
            (metadata_key::NAME, MetadataValue::String("Al".into())),
        ]);
        let mut writer = Writer::new();
        metadata.write(&mut writer);
        let mut expected = vec![0x02, 0x04, 0x04, 0x04, 0x02, b'A', b'l', 38, 0x03, 0x03];
        expected.extend(1.0f32.to_le_bytes());
        assert_eq!(writer.into_bytes(), expected);
    }

    #[test]
    fn player_list_removal_layout() {
        let bytes = PlayerList::Remove(vec![uuid()]).encode();
        assert_eq!(bytes[..4], [0x3F, 0x01, 0x00, 0x01]);
        assert_eq!(bytes[4..], uuid_bytes(&uuid()));
    }

    #[test]
    fn player_list_addition_carries_a_whole_skin() {
        let entry = PlayerListEntry {
            uuid: uuid(),
            entity_unique_id: 2,
            username: "Steve".into(),
            skin: Skin::solid("bedrockrs.steve", [255, 0, 0, 255]),
        };
        let bytes = PlayerList::Add(vec![entry]).encode();
        assert_eq!(bytes[..4], [0x3F, 0x01, 0x01, 0x00]);
        assert_eq!(bytes[20..23], [0x04, 0x05, b'S']);
        // The skin image dominates: 64 × 64 RGBA pixels.
        assert!(bytes.len() > 64 * 64 * 4);
    }

    #[test]
    fn skin_geometry_is_json_defining_the_patched_geometry() {
        let patch: serde_json::Value = serde_json::from_str(RESOURCE_PATCH).unwrap();
        let named = &patch["geometry"]["default"];
        let geometry: serde_json::Value = serde_json::from_str(HUMANOID_GEOMETRY).unwrap();
        let definition = &geometry["minecraft:geometry"][0];
        assert_eq!(&definition["description"]["identifier"], named);

        // Every parent is a bone defined in the same geometry.
        let bones = definition["bones"].as_array().unwrap();
        let names: Vec<_> = bones.iter().map(|bone| &bone["name"]).collect();
        // Held items need somewhere to be drawn.
        for item_bone in ["rightItem", "leftItem"] {
            assert!(
                names.contains(&&serde_json::json!(item_bone)),
                "{item_bone}"
            );
        }
        for bone in bones {
            if let Some(parent) = bone.get("parent") {
                assert!(names.contains(&parent), "unknown parent {parent}");
            }
        }

        // The skin carries the definition and its engine version.
        let mut writer = Writer::new();
        Skin::solid("s", [0; 4]).write(&mut writer);
        let bytes = writer.into_bytes();
        let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(contains(HUMANOID_GEOMETRY.as_bytes()));
        assert!(contains(b"\x050.0.0"));
    }

    #[test]
    fn skins_carry_capes_and_animations() {
        let mut skin = Skin::solid("s", [1, 2, 3, 4]);
        skin.cape = Some(Cape {
            id: "cape".into(),
            width: 64,
            height: 32,
            data: vec![9; 64 * 32 * 4],
        });
        skin.animations.push(SkinAnimation {
            width: 64,
            height: 32,
            data: vec![7; 64 * 32 * 4],
            kind: 1,
            frames: 2.0,
            expression: 1,
        });
        let bytes = PlayerSkin { uuid: uuid(), skin }.encode();
        assert_eq!(bytes[..1], [93]);
        assert!(bytes.len() > 64 * 64 * 4 + 2 * 64 * 32 * 4);
        assert!(bytes.windows(4).any(|w| w == b"cape"));
        // Ends with the trusted flag, profile hash and skin names.
        assert!(bytes.ends_with(b"\x04true\x00\x00\x00"));
    }

    #[test]
    fn persona_pieces_and_tints_follow_the_skin_colour() {
        let mut skin = Skin::solid("s", [0; 4]);
        skin.colour = [0xB3, 0x7B, 0x62, 0xFF];
        skin.persona_pieces.push(PersonaPiece {
            id: "hair".into(),
            kind: persona_piece_type::from_login("persona_hair"),
            pack_id: uuid(),
            default: true,
            product_id: String::new(),
        });
        skin.piece_tints.push(PieceTint {
            piece_type: PieceTint::wire_type("persona_hand"),
            colours: [[1, 2, 3, 4], [0; 4], [0; 4], [0; 4]],
        });
        let mut writer = Writer::new();
        skin.write(&mut writer);
        let bytes = writer.into_bytes();
        let find = |needle: &[u8]| bytes.windows(needle.len()).position(|w| w == needle);
        // Wide arms, then the colour as B, G, R, A, then one piece.
        let colour = find(&[1, 0x62, 0x7B, 0xB3, 0xFF, 1]).expect("arm size and colour");
        assert_eq!(bytes[colour + 6..colour + 11], [4, b'h', b'a', b'i', b'r']);
        assert_eq!(bytes[colour + 11..colour + 15], 14u32.to_le_bytes());
        // One tint, named "hands", its first colour as B, G, R, A.
        let tint = find(b"\x01\x05hands").expect("the tint");
        assert_eq!(bytes[tint + 7..tint + 11], [3, 2, 1, 4]);
    }

    #[test]
    fn piece_types_are_numbered_like_mojangs() {
        assert_eq!(persona_piece_type::from_login("persona_skeleton"), 1);
        assert_eq!(persona_piece_type::from_login("persona_hand"), 9);
        assert_eq!(persona_piece_type::from_login("persona_eyes"), 13);
        assert_eq!(persona_piece_type::from_login("persona_emote"), 27);
        assert_eq!(persona_piece_type::from_login("persona_hats"), 0);
    }

    #[test]
    fn update_attributes_layout() {
        let packet = UpdateAttributes {
            entity_runtime_id: 1,
            attributes: vec![Attribute::at_default(
                "minecraft:movement",
                0.0,
                f32::MAX,
                0.1,
            )],
            tick: 0,
        };
        let mut expected = vec![0x1D, 0x01, 0x01];
        for value in [0.0f32, f32::MAX, 0.1, 0.0, f32::MAX, 0.1] {
            expected.extend(value.to_le_bytes());
        }
        expected.push(18);
        expected.extend(b"minecraft:movement");
        expected.extend([0x00, 0x00]);
        assert_eq!(packet.encode(), expected);
    }

    #[test]
    fn update_abilities_layout() {
        let packet = UpdateAbilities(AbilityData {
            entity_unique_id: 2,
            player_permissions: 1,
            command_permissions: 0,
            layers: vec![AbilityLayer::base(ability::MAY_FLY)],
        });
        let bytes = packet.encode();
        assert_eq!(bytes[..2], [0xBB, 0x01]);
        assert_eq!(bytes[2..10], 2i64.to_le_bytes());
        assert_eq!(bytes[10..13], [0x01, 0x00, 0x01]);
        assert_eq!(bytes[13..15], 1u16.to_le_bytes());
        assert_eq!(bytes[15..19], 0x000F_FFFFu32.to_le_bytes());
        assert_eq!(bytes[19..23], (1u32 << 10).to_le_bytes());
        // Fly, vertical fly and walk speeds.
        assert_eq!(bytes[23..27], 0.05f32.to_le_bytes());
        assert_eq!(bytes[27..31], 1.0f32.to_le_bytes());
        assert_eq!(bytes[31..], 0.1f32.to_le_bytes());
    }

    #[test]
    fn set_actor_data_layout() {
        let packet = SetActorData {
            entity_runtime_id: 1,
            metadata: EntityMetadata(vec![(
                metadata_key::FLAGS,
                MetadataValue::Long(entity_flag::bits(&[entity_flag::HAS_GRAVITY])),
            )]),
            tick: 0,
        };
        let mut expected = vec![0x27, 0x01, 0x01, 0x00, 0x07, 0x07];
        // 1 << 49, zigzagged: 1 << 50 as a varint.
        expected.extend([0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02]);
        expected.extend([0x00, 0x00, 0x00]);
        assert_eq!(packet.encode(), expected);
    }

    #[test]
    fn remove_actor_is_a_zigzag_unique_id() {
        assert_eq!(
            RemoveActor {
                entity_unique_id: 2
            }
            .encode(),
            [0x0E, 0x04]
        );
    }
}
