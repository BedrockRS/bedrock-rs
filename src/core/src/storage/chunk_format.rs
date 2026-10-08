//! Chunk records in vanilla's on-disk format.
//!
//! A sub-chunk record (version 9) is the version byte, the number of block
//! layers, the sub-chunk's vertical index, then each layer: a header byte
//! (index width << 1; the low bit is 0 on disk, 1 on the network), the
//! packed indices as little-endian `u32` words (x, z, y order), the palette
//! size as an `i32` (left out when the width is 0: one entry), and the
//! palette, a root NBT compound `{name, states, version}` per block state.
//! Versions 8 (without the index byte) and 1 (one layer, no count) are read
//! too; older ones are numeric block IDs, not supported.
//!
//! Layer 1 holds the water around waterlogged blocks. It is not kept yet:
//! waterlogged blocks of a vanilla world load dry.
//!
//! As Dragonfly writes them (and vanilla opens them): a chunk new to the
//! database also gets its version (42), `FinalizedState` 2 (generated), and
//! `Data3D` with an empty height map (vanilla works it out again) and plains
//! for biomes. A chunk the database had keeps its own version, state and
//! biomes.

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::chunk::{
    BIT_SIZES, index_bits, pack_indices, packed_words, unpack_indices,
};
use bedrockrs_protocol::io::{Reader, Writer};
use bedrockrs_protocol::nbt::{Compound, Tag};

use super::{BLOCKS, StoreError, StoredSubChunk};
use crate::world::SUB_CHUNKS;

/// The chunk format version written for new chunks, as Dragonfly writes for
/// 1.21.
pub const CHUNK_VERSION: u8 = 42;

/// The block state version of the palette the server knows: 1.21.60.33,
/// what every state in Dragonfly's 1.26.50 palette carries.
pub const BLOCK_VERSION: i32 = 18_168_865;

/// `FinalizedState`: the chunk is fully generated.
pub const FINALIZED: i32 = 2;

const SUB_CHUNK_VERSION: u8 = 9;

/// Biome ID of plains.
const PLAINS: i32 = 1;

/// A sub-chunk's record: one layer of blocks.
pub fn encode_sub_chunk(y: i8, sub_chunk: &StoredSubChunk) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.u8(SUB_CHUNK_VERSION);
    writer.u8(1);
    writer.u8(y as u8);
    let bits = index_bits(sub_chunk.palette.len());
    writer.u8((bits as u8) << 1);
    for word in pack_indices(&sub_chunk.indices, bits) {
        writer.u32_le(word);
    }
    if bits > 0 {
        let len = i32::try_from(sub_chunk.palette.len()).expect("at most 4096 palette entries");
        writer.i32_le(len);
    }
    for state in &sub_chunk.palette {
        state_to_nbt(state).write_le(&mut writer);
    }
    writer.into_bytes()
}

/// A sub-chunk record's blocks (layer 0), or `None` if it has no layers.
pub fn decode_sub_chunk(bytes: &[u8]) -> Result<Option<StoredSubChunk>, StoreError> {
    let mut reader = Reader::new(bytes);
    let layers = match reader.u8()? {
        1 => 1,
        8 => reader.u8()?,
        9 => {
            let layers = reader.u8()?;
            let _y = reader.u8()?;
            layers
        }
        version => return Err(StoreError::SubChunkVersion(version)),
    };
    if layers == 0 {
        return Ok(None);
    }
    read_layer(&mut reader).map(Some)
}

fn read_layer(reader: &mut Reader<'_>) -> Result<StoredSubChunk, StoreError> {
    let header = reader.u8()?;
    if header & 1 == 1 {
        return Err(StoreError::Malformed("a block layer in the network format"));
    }
    let bits = u32::from(header >> 1);
    if !BIT_SIZES.contains(&bits) {
        return Err(StoreError::Malformed("an unsupported index width"));
    }
    let words = (0..packed_words(bits))
        .map(|_| reader.u32_le())
        .collect::<Result<Vec<_>, _>>()?;
    let palette_len = if bits == 0 {
        1
    } else {
        let len = reader.i32_le()?;
        usize::try_from(len)
            .ok()
            .filter(|len| (1..=BLOCKS).contains(len))
            .ok_or(StoreError::Malformed("a palette size out of range"))?
    };
    let palette = (0..palette_len)
        .map(|_| {
            let (_, compound) = Compound::read_le(reader)?;
            state_from_nbt(&compound)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let indices = unpack_indices(&words, bits);
    if indices
        .iter()
        .any(|index| usize::from(*index) >= palette.len())
    {
        return Err(StoreError::Malformed("a block index past the palette"));
    }
    Ok(StoredSubChunk { palette, indices })
}

/// A block state as vanilla stores it in palettes.
fn state_to_nbt(state: &BlockState) -> Compound {
    let states = state
        .states
        .iter()
        .map(|(name, value)| {
            let tag = match value {
                StateValue::Byte(byte) => Tag::Byte(*byte as i8),
                StateValue::Int(int) => Tag::Int(*int),
                StateValue::String(text) => Tag::String(text.clone()),
            };
            (name.clone(), tag)
        })
        .collect();
    Compound::new()
        .with("name", Tag::String(state.name.clone()))
        .with("states", Tag::Compound(Compound(states)))
        .with("version", Tag::Int(BLOCK_VERSION))
}

/// A palette entry back to a block state. States of a kind blocks do not
/// use are left out; the server's palette fills in any it misses.
fn state_from_nbt(compound: &Compound) -> Result<BlockState, StoreError> {
    let Some(Tag::String(name)) = compound.get("name") else {
        return Err(StoreError::Malformed("a palette entry without a name"));
    };
    let mut state = BlockState::new(name.clone());
    if let Some(Tag::Compound(states)) = compound.get("states") {
        for (key, tag) in &states.0 {
            let value = match tag {
                Tag::Byte(byte) => StateValue::Byte(*byte as u8),
                Tag::Int(int) => StateValue::Int(*int),
                Tag::String(text) => StateValue::String(text.clone()),
                _ => continue,
            };
            state = state.with(key.clone(), value);
        }
    }
    Ok(state)
}

/// `Data3D` for a chunk new to the database: an empty height map (256
/// `i16`s), then a biome layer per sub-chunk, each all plains (index width
/// 0, so no indices and no palette size, then the one biome as an `i32`).
pub fn generated_data_3d() -> Vec<u8> {
    let mut writer = Writer::new();
    writer.raw(&[0; 512]);
    for _ in 0..SUB_CHUNKS {
        writer.u8(0);
        writer.i32_le(PLAINS);
    }
    writer.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub_chunk(palette: Vec<BlockState>) -> StoredSubChunk {
        let len = palette.len();
        StoredSubChunk {
            palette,
            indices: (0..BLOCKS).map(|index| (index % len) as u16).collect(),
        }
    }

    #[test]
    fn sub_chunks_survive_the_disk_format_at_every_width() {
        // Palettes of 1, 2, 5 (3 bits, padded words), 33 (6 bits) and 300
        // (16 bits) entries.
        for len in [1, 2, 5, 33, 300] {
            let palette = (0..len)
                .map(|n| {
                    BlockState::new(format!("bedrockrs:test_{n}"))
                        .with("number", StateValue::Int(n))
                        .with("on", StateValue::Byte(1))
                        .with("colour", StateValue::String("red".into()))
                })
                .collect();
            let original = sub_chunk(palette);
            let bytes = encode_sub_chunk(-4, &original);
            assert_eq!(bytes[..3], [9, 1, 0xFC], "version, one layer, y = -4");
            assert_eq!(decode_sub_chunk(&bytes).unwrap(), Some(original), "{len}");
        }
    }

    #[test]
    fn a_one_block_palette_has_no_indices_or_size() {
        let stone = sub_chunk(vec![BlockState::new("minecraft:stone")]);
        let bytes = encode_sub_chunk(0, &stone);
        // Header, layer header 0, then straight to the one palette entry.
        assert_eq!(bytes[..5], [9, 1, 0, 0, 10]);
        let entry = Compound::from_le_bytes(&bytes[4..]).unwrap();
        assert_eq!(
            entry.get("name"),
            Some(&Tag::String("minecraft:stone".into()))
        );
        assert_eq!(entry.get("version"), Some(&Tag::Int(BLOCK_VERSION)));
    }

    #[test]
    fn older_vanilla_versions_and_extra_layers_are_read() {
        let original = sub_chunk(vec![
            BlockState::new("minecraft:air"),
            BlockState::new("minecraft:kelp").with("kelp_age", StateValue::Int(3)),
        ]);
        let current = encode_sub_chunk(2, &original);
        // Version 8: the same without the vertical index.
        let mut version_8 = vec![8, 1];
        version_8.extend(&current[3..]);
        assert_eq!(
            decode_sub_chunk(&version_8).unwrap(),
            Some(original.clone())
        );
        // Version 1: one layer, no count.
        let mut version_1 = vec![1];
        version_1.extend(&current[3..]);
        assert_eq!(
            decode_sub_chunk(&version_1).unwrap(),
            Some(original.clone())
        );
        // A second layer (water) is skipped; no layers is all air.
        let mut two_layers = vec![9, 2, 2];
        two_layers.extend(&current[3..]);
        two_layers.extend(&current[3..]);
        assert_eq!(decode_sub_chunk(&two_layers).unwrap(), Some(original));
        assert_eq!(decode_sub_chunk(&[9, 0, 2]).unwrap(), None);
    }

    #[test]
    fn damaged_or_unsupported_records_are_errors_not_panics() {
        assert!(matches!(
            decode_sub_chunk(&[0, 1, 2]),
            Err(StoreError::SubChunkVersion(0))
        ));
        assert!(decode_sub_chunk(&[]).is_err());
        let bytes = encode_sub_chunk(
            0,
            &sub_chunk(vec![
                BlockState::new("minecraft:air"),
                BlockState::new("minecraft:dirt"),
            ]),
        );
        for len in 0..bytes.len() {
            assert!(decode_sub_chunk(&bytes[..len]).is_err(), "{len}");
        }
        // A network-format layer, and an index past the palette: with three
        // entries (2-bit indices), index 3.
        assert!(decode_sub_chunk(&[9, 1, 0, 1]).is_err());
        let mut past = encode_sub_chunk(
            0,
            &sub_chunk(vec![
                BlockState::new("minecraft:air"),
                BlockState::new("minecraft:dirt"),
                BlockState::new("minecraft:stone"),
            ]),
        );
        past[4] = 0xFF;
        assert!(matches!(
            decode_sub_chunk(&past),
            Err(StoreError::Malformed(_))
        ));
    }

    #[test]
    fn new_chunks_get_plains_and_an_empty_height_map() {
        let data = generated_data_3d();
        assert_eq!(data.len(), 512 + SUB_CHUNKS * 5);
        assert!(data[..512].iter().all(|byte| *byte == 0));
        assert_eq!(data[512..517], [0, 1, 0, 0, 0]);
    }
}
