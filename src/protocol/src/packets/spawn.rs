//! Packets that bring a logged-in player into the world.
//!
//! The server sends JigsawStructureData, VoxelShapes, StartGame and ItemRegistry,
//! answers the client's RequestChunkRadius with the chunks around it, then sends
//! PlayStatus(PlayerSpawn); the client confirms with SetLocalPlayerAsInitialized.

use crate::io::{DecodeError, Reader, Writer};
use crate::nbt::{Compound, Tag, TagKind};
use crate::packet::{Decode, Encode, Packet, id};
use crate::types::{BlockPos, Vec3};

/// The world and player settings a client needs before it can load terrain.
///
/// Fields are in wire order. Nested structures BedrockRS does not use yet are
/// always written as absent: the server join information and the education
/// shared resource URI.
#[derive(Debug, Clone, PartialEq)]
pub struct StartGame {
    pub entity_unique_id: i64,
    pub entity_runtime_id: u64,
    pub player_game_mode: i32,
    /// Eye position: feet plus 1.62.
    pub player_position: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub world_seed: i64,
    pub spawn_biome_type: i16,
    pub user_defined_biome_name: String,
    pub dimension: i32,
    /// 0 legacy, 1 overworld, 2 flat, 3 nether, 4 end, 5 void.
    pub generator: i32,
    pub world_game_mode: i32,
    pub hardcore: bool,
    pub difficulty: i32,
    pub world_spawn: BlockPos,
    pub achievements_disabled: bool,
    pub editor_world_type: i32,
    pub created_in_editor: bool,
    pub exported_from_editor: bool,
    pub day_cycle_lock_time: i32,
    pub education_edition_offer: u32,
    pub education_features_enabled: bool,
    pub education_product_id: String,
    pub rain_level: f32,
    pub lightning_level: f32,
    pub confirmed_platform_locked_content: bool,
    pub multiplayer_game: bool,
    pub lan_broadcast_enabled: bool,
    pub xbl_broadcast_mode: i32,
    pub platform_broadcast_mode: i32,
    pub commands_enabled: bool,
    pub texture_pack_required: bool,
    pub game_rules: Vec<GameRule>,
    pub experiments: Vec<super::Experiment>,
    pub experiments_previously_toggled: bool,
    pub bonus_chest_enabled: bool,
    pub start_with_map_enabled: bool,
    /// 0 visitor, 1 member, 2 operator, 3 custom.
    pub player_permissions: u8,
    pub server_chunk_tick_radius: i32,
    pub has_locked_behaviour_pack: bool,
    pub has_locked_texture_pack: bool,
    pub from_locked_world_template: bool,
    pub msa_gamertags_only: bool,
    pub from_world_template: bool,
    pub world_template_settings_locked: bool,
    pub only_spawn_v1_villagers: bool,
    pub persona_disabled: bool,
    pub custom_skins_disabled: bool,
    pub emote_chat_muted: bool,
    pub base_game_version: String,
    pub limited_world_width: i32,
    pub limited_world_depth: i32,
    pub new_nether: bool,
    pub force_experimental_gameplay: Option<bool>,
    pub chat_restriction_level: u8,
    pub disable_player_interactions: bool,
    pub server_editor_connection_policy: i32,
    pub allow_anonymous_block_drops_in_editor_worlds: bool,
    pub level_id: String,
    pub world_name: String,
    pub template_content_identity: String,
    pub trial: bool,
    pub player_movement_settings: PlayerMovementSettings,
    pub time: i64,
    pub enchantment_seed: i32,
    /// Custom (data-driven) blocks; vanilla blocks are never listed.
    pub blocks: Vec<BlockEntry>,
    pub multiplayer_correlation_id: String,
    pub server_authoritative_inventory: bool,
    pub game_version: String,
    pub property_data: Compound,
    pub server_block_state_checksum: u64,
    pub world_template_id: [u8; 16],
    pub client_side_generation: bool,
    /// Chunk palettes carry hashed block network IDs (see [`crate::block`]).
    pub use_block_network_id_hashes: bool,
    pub server_authoritative_sound: bool,
    pub server_id: String,
    pub scenario_id: String,
    pub world_id: String,
    pub owner_id: String,
}

impl Packet for StartGame {
    const ID: u32 = id::START_GAME;
}

impl Encode for StartGame {
    fn encode_payload(&self, w: &mut Writer) {
        w.var_i64(self.entity_unique_id);
        w.var_u64(self.entity_runtime_id);
        w.var_i32(self.player_game_mode);
        self.player_position.write(w);
        w.f32_le(self.pitch);
        w.f32_le(self.yaw);
        w.i64_le(self.world_seed);
        w.i16_le(self.spawn_biome_type);
        w.string(&self.user_defined_biome_name);
        w.var_i32(self.dimension);
        w.var_i32(self.generator);
        w.var_i32(self.world_game_mode);
        w.bool(self.hardcore);
        w.var_i32(self.difficulty);
        self.world_spawn.write(w);
        w.bool(self.achievements_disabled);
        w.var_i32(self.editor_world_type);
        w.bool(self.created_in_editor);
        w.bool(self.exported_from_editor);
        w.var_i32(self.day_cycle_lock_time);
        w.var_u32(self.education_edition_offer);
        w.bool(self.education_features_enabled);
        w.string(&self.education_product_id);
        w.f32_le(self.rain_level);
        w.f32_le(self.lightning_level);
        w.bool(self.confirmed_platform_locked_content);
        w.bool(self.multiplayer_game);
        w.bool(self.lan_broadcast_enabled);
        w.var_i32(self.xbl_broadcast_mode);
        w.var_i32(self.platform_broadcast_mode);
        w.bool(self.commands_enabled);
        w.bool(self.texture_pack_required);
        w.var_u32(len_u32(self.game_rules.len()));
        for rule in &self.game_rules {
            rule.write(w);
        }
        w.u32_le(len_u32(self.experiments.len()));
        for experiment in &self.experiments {
            w.string(&experiment.name);
            w.bool(experiment.enabled);
        }
        w.bool(self.experiments_previously_toggled);
        w.bool(self.bonus_chest_enabled);
        w.bool(self.start_with_map_enabled);
        w.u8(self.player_permissions);
        w.i32_le(self.server_chunk_tick_radius);
        w.bool(self.has_locked_behaviour_pack);
        w.bool(self.has_locked_texture_pack);
        w.bool(self.from_locked_world_template);
        w.bool(self.msa_gamertags_only);
        w.bool(self.from_world_template);
        w.bool(self.world_template_settings_locked);
        w.bool(self.only_spawn_v1_villagers);
        w.bool(self.persona_disabled);
        w.bool(self.custom_skins_disabled);
        w.bool(self.emote_chat_muted);
        w.string(&self.base_game_version);
        w.i32_le(self.limited_world_width);
        w.i32_le(self.limited_world_depth);
        w.bool(self.new_nether);
        // Education shared resource URI: button name and link, both empty.
        w.string("");
        w.string("");
        w.bool(self.force_experimental_gameplay.is_some());
        if let Some(force) = self.force_experimental_gameplay {
            w.bool(force);
        }
        w.u8(self.chat_restriction_level);
        w.bool(self.disable_player_interactions);
        w.var_i32(self.server_editor_connection_policy);
        w.bool(self.allow_anonymous_block_drops_in_editor_worlds);
        w.string(&self.level_id);
        w.string(&self.world_name);
        w.string(&self.template_content_identity);
        w.bool(self.trial);
        w.var_i32(self.player_movement_settings.rewind_history_size);
        w.bool(
            self.player_movement_settings
                .server_authoritative_block_breaking,
        );
        w.i64_le(self.time);
        w.var_i32(self.enchantment_seed);
        w.var_u32(len_u32(self.blocks.len()));
        for block in &self.blocks {
            w.string(&block.name);
            block.properties.write_network(w);
        }
        w.string(&self.multiplayer_correlation_id);
        w.bool(self.server_authoritative_inventory);
        w.string(&self.game_version);
        self.property_data.write_network(w);
        w.u64_le(self.server_block_state_checksum);
        w.uuid(self.world_template_id);
        w.bool(self.client_side_generation);
        w.bool(self.use_block_network_id_hashes);
        w.bool(self.server_authoritative_sound);
        // No server join information.
        w.bool(false);
        w.string(&self.server_id);
        w.string(&self.scenario_id);
        w.string(&self.world_id);
        w.string(&self.owner_id);
    }
}

/// A game rule and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct GameRule {
    pub name: String,
    pub editable: bool,
    pub value: GameRuleValue,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GameRuleValue {
    Bool(bool),
    Int(u32),
    Float(f32),
}

impl GameRule {
    fn write(&self, w: &mut Writer) {
        w.string(&self.name);
        w.bool(self.editable);
        match self.value {
            GameRuleValue::Bool(value) => {
                w.var_u32(1);
                w.bool(value);
            }
            GameRuleValue::Int(value) => {
                w.var_u32(2);
                w.u32_le(value);
            }
            GameRuleValue::Float(value) => {
                w.var_u32(3);
                w.f32_le(value);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayerMovementSettings {
    pub rewind_history_size: i32,
    pub server_authoritative_block_breaking: bool,
}

/// A custom block definition.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockEntry {
    pub name: String,
    pub properties: Compound,
}

/// The items the client knows, by name and network ID.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemRegistry {
    pub items: Vec<ItemEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemEntry {
    pub name: String,
    pub runtime_id: i16,
    pub component_based: bool,
    pub version: i32,
    /// The item's components as an encoded network NBT compound; `None` sends
    /// an empty compound.
    pub components: Option<Vec<u8>>,
}

impl Packet for ItemRegistry {
    const ID: u32 = id::ITEM_REGISTRY;
}

impl Encode for ItemRegistry {
    fn encode_payload(&self, w: &mut Writer) {
        w.var_u32(len_u32(self.items.len()));
        for item in &self.items {
            w.string(&item.name);
            w.i16_le(item.runtime_id);
            w.bool(item.component_based);
            w.var_i32(item.version);
            match &item.components {
                Some(components) => w.raw(components),
                None => Compound::new().write_network(w),
            }
        }
    }
}

/// Jigsaw structure data, which the client requires before StartGame.
#[derive(Debug, Clone, PartialEq)]
pub struct JigsawStructureData {
    pub structure_data: Compound,
}

impl JigsawStructureData {
    /// No structures, as gophertunnel's server sends.
    pub fn empty() -> Self {
        let empty = || Tag::List(TagKind::Compound, Vec::new());
        Self {
            structure_data: Compound::new()
                .with("processors", empty())
                .with("template_pools", empty())
                .with("jigsaws", empty())
                .with("structure_sets", empty()),
        }
    }
}

impl Packet for JigsawStructureData {
    const ID: u32 = id::JIGSAW_STRUCTURE_DATA;
}

impl Encode for JigsawStructureData {
    fn encode_payload(&self, w: &mut Writer) {
        self.structure_data.write_network(w);
    }
}

/// Custom voxel shapes. BedrockRS defines none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoxelShapes;

impl Packet for VoxelShapes {
    const ID: u32 = id::VOXEL_SHAPES;
}

impl Encode for VoxelShapes {
    fn encode_payload(&self, w: &mut Writer) {
        // No shapes, no name map, no custom shapes.
        w.var_u32(0);
        w.var_u32(0);
        w.u16_le(0);
    }
}

/// The chunk view distance the client asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestChunkRadius {
    pub radius: i32,
    pub max_radius: u8,
}

impl Packet for RequestChunkRadius {
    const ID: u32 = id::REQUEST_CHUNK_RADIUS;
}

impl Decode for RequestChunkRadius {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            radius: reader.var_i32()?,
            max_radius: reader.u8()?,
        })
    }
}

impl Encode for RequestChunkRadius {
    fn encode_payload(&self, w: &mut Writer) {
        w.var_i32(self.radius);
        w.u8(self.max_radius);
    }
}

/// The view distance the server grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRadiusUpdated {
    pub radius: i32,
}

impl Packet for ChunkRadiusUpdated {
    const ID: u32 = id::CHUNK_RADIUS_UPDATED;
}

impl Encode for ChunkRadiusUpdated {
    fn encode_payload(&self, w: &mut Writer) {
        w.var_i32(self.radius);
    }
}

/// Where the client should load and keep chunks: a centre and a radius in blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkChunkPublisherUpdate {
    pub position: BlockPos,
    pub radius: u32,
}

impl Packet for NetworkChunkPublisherUpdate {
    const ID: u32 = id::NETWORK_CHUNK_PUBLISHER_UPDATE;
}

impl Encode for NetworkChunkPublisherUpdate {
    fn encode_payload(&self, w: &mut Writer) {
        self.position.write(w);
        w.var_u32(self.radius);
        // No saved chunks.
        w.u32_le(0);
    }
}

impl Decode for NetworkChunkPublisherUpdate {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let update = Self {
            position: BlockPos::read(reader)?,
            radius: reader.var_u32()?,
        };
        // Saved chunks: a count, then that many chunk positions (two varints each).
        for _ in 0..reader.u32_le()? {
            reader.var_i32()?;
            reader.var_i32()?;
        }
        Ok(update)
    }
}

/// One chunk column, sent in full without the blob cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelChunk {
    pub x: i32,
    pub z: i32,
    pub dimension: i32,
    /// Sub-chunks in the payload, counted from the bottom of the dimension.
    pub sub_chunk_count: u32,
    /// See [`crate::chunk::level_chunk_payload`].
    pub payload: Vec<u8>,
}

impl Packet for LevelChunk {
    const ID: u32 = id::LEVEL_CHUNK;
}

impl Encode for LevelChunk {
    fn encode_payload(&self, w: &mut Writer) {
        w.var_i32(self.x);
        w.var_i32(self.z);
        w.var_i32(self.dimension);
        w.var_u32(self.sub_chunk_count);
        // No sub-chunk request limit: the payload is complete.
        w.bool(false);
        // Blob cache disabled, so no blob hashes.
        w.bool(false);
        w.var_u32(0);
        w.byte_array(&self.payload);
    }
}

/// Sent by the client once it has loaded in and can move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetLocalPlayerAsInitialized {
    pub entity_runtime_id: u64,
}

impl Packet for SetLocalPlayerAsInitialized {
    const ID: u32 = id::SET_LOCAL_PLAYER_AS_INITIALIZED;
}

impl Decode for SetLocalPlayerAsInitialized {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            entity_runtime_id: reader.var_u64()?,
        })
    }
}

impl Encode for SetLocalPlayerAsInitialized {
    fn encode_payload(&self, w: &mut Writer) {
        w.var_u64(self.entity_runtime_id);
    }
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("lists are shorter than 4 billion entries")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn payload<P: Encode>(packet: &P) -> Vec<u8> {
        let bytes = packet.encode();
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, P::ID);
        bytes[bytes.len() - payload.remaining()..].to_vec()
    }

    #[test]
    fn small_spawn_packets_match_the_wire() {
        assert_eq!(payload(&crate::packets::CreativeContent::default()), [0, 0]);
        assert_eq!(payload(&VoxelShapes), [0, 0, 0, 0]);
        assert_eq!(payload(&ChunkRadiusUpdated { radius: 8 }), [16]);
        assert_eq!(
            payload(&NetworkChunkPublisherUpdate {
                position: BlockPos { x: 8, y: -60, z: 8 },
                radius: 128,
            }),
            [16, 119, 16, 0x80, 0x01, 0, 0, 0, 0]
        );
        assert_eq!(
            payload(&LevelChunk {
                x: -1,
                z: 2,
                dimension: 0,
                sub_chunk_count: 1,
                payload: vec![0xAB],
            }),
            [1, 4, 0, 1, 0, 0, 0, 1, 0xAB]
        );
    }

    #[test]
    fn jigsaw_structure_data_lists_are_empty_compound_lists() {
        let mut expected = vec![10, 0];
        for name in ["processors", "template_pools", "jigsaws", "structure_sets"] {
            expected.extend([9, name.len() as u8]);
            expected.extend(name.as_bytes());
            expected.extend([10, 0]);
        }
        expected.push(0);
        assert_eq!(payload(&JigsawStructureData::empty()), expected);
    }

    #[test]
    fn client_packets_decode() {
        let bytes = RequestChunkRadius {
            radius: 12,
            max_radius: 32,
        }
        .encode();
        let (_, reader) = read_header(&bytes).unwrap();
        assert_eq!(
            decode::<RequestChunkRadius>(reader).unwrap(),
            RequestChunkRadius {
                radius: 12,
                max_radius: 32
            }
        );

        let bytes = SetLocalPlayerAsInitialized {
            entity_runtime_id: 1,
        }
        .encode();
        assert_eq!(bytes, [113, 1]);
    }

    #[test]
    fn game_rules_are_tagged_by_type() {
        let mut w = Writer::new();
        GameRule {
            name: "showcoordinates".into(),
            editable: false,
            value: GameRuleValue::Bool(true),
        }
        .write(&mut w);
        let mut expected = vec![15];
        expected.extend(b"showcoordinates");
        expected.extend([0, 1, 1]);
        assert_eq!(w.into_bytes(), expected);
    }
}
