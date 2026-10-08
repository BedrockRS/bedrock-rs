//! Keys of the records in a world's LevelDB database, as vanilla writes them.
//!
//! A chunk record's key is the chunk's x and z (each `i32`, little-endian),
//! its dimension (`i32`, little-endian; left out for the overworld), a tag
//! byte saying what the record holds, and, for sub-chunks only, the
//! sub-chunk's vertical index (`i8`: -4 for y = -64..-49). Players are kept
//! under text keys: `player_server_<uuid>`.

use bedrockrs_protocol::types::ChunkPos;
use uuid::Uuid;

/// A dimension, as chunk keys number it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Overworld,
    Nether,
    End,
}

impl Dimension {
    pub const fn id(self) -> i32 {
        match self {
            Self::Overworld => 0,
            Self::Nether => 1,
            Self::End => 2,
        }
    }
}

/// What a chunk record holds, by its tag byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkRecord {
    /// `0x2B`: the height map and 3D biomes (since 1.18).
    Data3D,
    /// `0x2C`: the chunk format version, one byte. Every saved chunk has it.
    Version,
    /// `0x2D`: the height map and 2D biomes (before 1.18).
    Data2D,
    /// `0x2E`: the even older height map and biome colours.
    Data2DLegacy,
    /// `0x2F`: one sub-chunk's blocks, at vertical index `y`.
    SubChunk { y: i8 },
    /// `0x30`: a whole column's blocks, in worlds from before 1.0.
    LegacyTerrain,
    /// `0x31`: block entities (chests' contents, signs' text), NBT.
    BlockEntity,
    /// `0x32`: entities, NBT (before actor digests).
    Entity,
    /// `0x36`: how far generation got; 2 is done.
    FinalizedState,
    /// `0x76`: the chunk format version of worlds from before 1.16.100.
    LegacyVersion,
}

impl ChunkRecord {
    pub const fn tag(self) -> u8 {
        match self {
            Self::Data3D => 0x2B,
            Self::Version => 0x2C,
            Self::Data2D => 0x2D,
            Self::Data2DLegacy => 0x2E,
            Self::SubChunk { .. } => 0x2F,
            Self::LegacyTerrain => 0x30,
            Self::BlockEntity => 0x31,
            Self::Entity => 0x32,
            Self::FinalizedState => 0x36,
            Self::LegacyVersion => 0x76,
        }
    }
}

/// The key of one record of one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkKey {
    pub chunk: ChunkPos,
    pub dimension: Dimension,
    pub record: ChunkRecord,
}

impl ChunkKey {
    /// The key of `record` of the overworld chunk `chunk`.
    pub const fn overworld(chunk: ChunkPos, record: ChunkRecord) -> Self {
        Self {
            chunk,
            dimension: Dimension::Overworld,
            record,
        }
    }

    pub fn to_bytes(self) -> Vec<u8> {
        let mut key = Vec::with_capacity(14);
        key.extend(self.chunk.x.to_le_bytes());
        key.extend(self.chunk.z.to_le_bytes());
        if self.dimension != Dimension::Overworld {
            key.extend(self.dimension.id().to_le_bytes());
        }
        key.push(self.record.tag());
        if let ChunkRecord::SubChunk { y } = self.record {
            key.push(y as u8);
        }
        key
    }
}

/// The key of a player's data: `player_server_<uuid>`, the UUID lower-case
/// and hyphenated, as vanilla writes it.
pub fn player_key(uuid: Uuid) -> String {
    format!("player_server_{}", uuid.hyphenated())
}

#[cfg(test)]
mod tests {
    use bedrock_leveldb::{ChunkCoordinates, ChunkRecordTag, SubChunkIndex};

    use super::*;

    #[test]
    fn chunk_keys_follow_the_vanilla_layout() {
        let chunk = ChunkPos::new(1, -2);
        assert_eq!(
            ChunkKey::overworld(chunk, ChunkRecord::Version).to_bytes(),
            [1, 0, 0, 0, 0xFE, 0xFF, 0xFF, 0xFF, 0x2C]
        );
        assert_eq!(
            ChunkKey::overworld(chunk, ChunkRecord::SubChunk { y: -4 }).to_bytes(),
            [1, 0, 0, 0, 0xFE, 0xFF, 0xFF, 0xFF, 0x2F, 0xFC]
        );
        let nether = ChunkKey {
            chunk,
            dimension: Dimension::Nether,
            record: ChunkRecord::Data3D,
        };
        assert_eq!(
            nether.to_bytes(),
            [1, 0, 0, 0, 0xFE, 0xFF, 0xFF, 0xFF, 1, 0, 0, 0, 0x2B]
        );
    }

    #[test]
    fn chunk_keys_match_the_database_crate() {
        // bedrock-leveldb's own key helpers, as a second opinion.
        let theirs = |x, z, dimension: i32, tag: ChunkRecordTag, y: Option<i8>| {
            let coordinates = ChunkCoordinates { x, z };
            let dimension = dimension.into();
            match y {
                Some(y) => bedrock_leveldb::ChunkKey::new_subchunk(
                    coordinates,
                    dimension,
                    SubChunkIndex::from_raw(y),
                ),
                None => bedrock_leveldb::ChunkKey::new(coordinates, dimension, tag),
            }
            .encode()
        };
        for (x, z) in [(0, 0), (-1, 1), (i32::MAX, i32::MIN), (300, -9000)] {
            let chunk = ChunkPos::new(x, z);
            for (dimension, id) in [
                (Dimension::Overworld, 0),
                (Dimension::Nether, 1),
                (Dimension::End, 2),
            ] {
                let ours = |record| {
                    ChunkKey {
                        chunk,
                        dimension,
                        record,
                    }
                    .to_bytes()
                };
                // The crate names the terrain records only.
                for (record, tag) in [
                    (ChunkRecord::Data3D, ChunkRecordTag::Data3D),
                    (ChunkRecord::Data2D, ChunkRecordTag::Data2D),
                    (ChunkRecord::Data2DLegacy, ChunkRecordTag::Data2DLegacy),
                    (ChunkRecord::LegacyTerrain, ChunkRecordTag::LegacyTerrain),
                    (ChunkRecord::Version, ChunkRecordTag::Unknown(0x2C)),
                ] {
                    assert_eq!(ours(record), theirs(x, z, id, tag, None), "{record:?}");
                }
                for y in [-4, 0, 19] {
                    assert_eq!(
                        ours(ChunkRecord::SubChunk { y }),
                        theirs(x, z, id, ChunkRecordTag::SubChunkPrefix, Some(y))
                    );
                }
            }
        }
    }

    #[test]
    fn records_have_vanilla_tags() {
        let tags = [
            (ChunkRecord::Data3D, b'+'),
            (ChunkRecord::Version, b','),
            (ChunkRecord::Data2D, b'-'),
            (ChunkRecord::Data2DLegacy, b'.'),
            (ChunkRecord::SubChunk { y: 0 }, b'/'),
            (ChunkRecord::LegacyTerrain, b'0'),
            (ChunkRecord::BlockEntity, b'1'),
            (ChunkRecord::Entity, b'2'),
            (ChunkRecord::FinalizedState, b'6'),
            (ChunkRecord::LegacyVersion, b'v'),
        ];
        for (record, tag) in tags {
            assert_eq!(record.tag(), tag, "{record:?}");
        }
    }

    #[test]
    fn player_keys_hold_the_hyphenated_uuid() {
        let uuid = Uuid::parse_str("5D6F2A01-0000-4000-8000-00000000ABCD").unwrap();
        assert_eq!(
            player_key(uuid),
            "player_server_5d6f2a01-0000-4000-8000-00000000abcd"
        );
    }
}
