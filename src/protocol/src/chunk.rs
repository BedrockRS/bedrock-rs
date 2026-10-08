//! Chunk data in the network format carried by LevelChunk.
//!
//! A chunk column is a stack of 16×16×16 sub-chunks. Each sub-chunk holds block
//! layers (layer 0 for blocks, layer 1 for waterlogging), and each layer is a
//! [`PalettedStorage`]: a palette of values plus a packed index per block.
//! Biomes use the same storage, one per sub-chunk of the dimension's height.
//! Layouts follow Dragonfly's `world/chunk` encoder.

use crate::io::Writer;

/// Sub-chunk format version written before each sub-chunk.
pub const SUB_CHUNK_VERSION: u8 = 9;

/// Header of a biome storage that repeats the previous sub-chunk's biomes (`0x7F << 1 | 1`).
const SAME_AS_PREVIOUS: u8 = 0xFF;

/// Values in a storage: 16×16×16.
pub const VALUES: usize = 16 * 16 * 16;

/// Bits per packed index the format supports; indices never span two words.
pub const BIT_SIZES: [u32; 9] = [0, 1, 2, 3, 4, 5, 6, 8, 16];

/// Smallest supported index width that can address a palette of
/// `palette_len` entries (at least one).
pub fn index_bits(palette_len: usize) -> u32 {
    let needed = usize::BITS - palette_len.saturating_sub(1).leading_zeros();
    *BIT_SIZES
        .iter()
        .find(|bits| **bits >= needed)
        .expect("16 bits address any palette")
}

/// Words that hold 4096 indices of `bits` bits, as many as fit in each word
/// (the rest of a word is padding, for 3, 5 and 6 bits). None for 0 bits.
pub fn packed_words(bits: u32) -> usize {
    if bits == 0 {
        0
    } else {
        VALUES.div_ceil(32 / bits as usize)
    }
}

/// Packs 4096 indices of `bits` bits into little-endian-ordered words, the
/// first index in the lowest bits of the first word.
pub fn pack_indices(indices: &[u16], bits: u32) -> Vec<u32> {
    let mut words = vec![0u32; packed_words(bits)];
    if bits == 0 {
        return words;
    }
    let per_word = 32 / bits as usize;
    for (position, index) in indices.iter().enumerate() {
        let shift = (position % per_word) as u32 * bits;
        words[position / per_word] |= u32::from(*index) << shift;
    }
    words
}

/// The 4096 indices of `bits` bits packed in `words`, which must be
/// [`packed_words`] long.
pub fn unpack_indices(words: &[u32], bits: u32) -> Vec<u16> {
    if bits == 0 {
        return vec![0; VALUES];
    }
    let per_word = 32 / bits as usize;
    let mask = (1u32 << bits) - 1;
    (0..VALUES)
        .map(|position| {
            let shift = (position % per_word) as u32 * bits;
            ((words[position / per_word] >> shift) & mask) as u16
        })
        .collect()
}

/// 4096 values (block network IDs or biome IDs) stored as indices into a palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PalettedStorage {
    palette: Vec<u32>,
    indices: Vec<u16>,
}

impl PalettedStorage {
    /// A storage where every position holds `value`.
    pub fn filled(value: u32) -> Self {
        Self {
            palette: vec![value],
            indices: vec![0; VALUES],
        }
    }

    pub fn get(&self, x: u8, y: u8, z: u8) -> u32 {
        self.palette[usize::from(self.indices[index(x, y, z)])]
    }

    pub fn set(&mut self, x: u8, y: u8, z: u8, value: u32) {
        let slot = match self.palette.iter().position(|v| *v == value) {
            Some(slot) => slot,
            None => {
                self.palette.push(value);
                self.palette.len() - 1
            }
        };
        self.indices[index(x, y, z)] =
            u16::try_from(slot).expect("a storage holds at most 4096 distinct values");
    }

    /// Smallest supported index width that can address the whole palette.
    fn bits_per_index(&self) -> u32 {
        index_bits(self.palette.len())
    }

    /// Network encoding: a header with the index width and a network flag,
    /// the packed indices as little-endian words, then the palette as zigzag
    /// varints. A zero-width storage omits the palette size.
    pub fn write_network(&self, writer: &mut Writer) {
        let bits = self.bits_per_index();
        writer.u8(((bits as u8) << 1) | 1);
        if bits > 0 {
            for word in pack_indices(&self.indices, bits) {
                writer.u32_le(word);
            }
            writer.var_i32(self.palette.len() as i32);
        }
        for value in &self.palette {
            writer.var_i32(*value as i32);
        }
    }
}

/// Index of a position within a storage: x, then z, then y.
fn index(x: u8, y: u8, z: u8) -> usize {
    (usize::from(x & 15) << 8) | (usize::from(z & 15) << 4) | usize::from(y & 15)
}

/// A 16×16×16 section of a chunk column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubChunk {
    /// Layer 0 holds blocks; layer 1, if present, holds waterlogging liquids.
    pub layers: Vec<PalettedStorage>,
}

impl SubChunk {
    /// Writes the sub-chunk with its vertical index (`y >> 4`, e.g. -4 at y = -64).
    pub fn write_network(&self, y_index: i8, writer: &mut Writer) {
        writer.u8(SUB_CHUNK_VERSION);
        writer.u8(u8::try_from(self.layers.len()).expect("a sub-chunk has few layers"));
        writer.u8(y_index as u8);
        for layer in &self.layers {
            layer.write_network(writer);
        }
    }
}

/// Builds a LevelChunk payload without the blob cache: the sub-chunks from the
/// bottom of the column up, one biome storage per sub-chunk of the dimension's
/// height, an empty border block list and no block entities.
pub fn level_chunk_payload(
    lowest_y_index: i8,
    sub_chunks: &[SubChunk],
    biomes: &[PalettedStorage],
) -> Vec<u8> {
    let mut writer = Writer::new();
    for (offset, sub_chunk) in sub_chunks.iter().enumerate() {
        let offset = i8::try_from(offset).expect("a column has at most 64 sub-chunks");
        sub_chunk.write_network(lowest_y_index + offset, &mut writer);
    }
    let mut previous = None;
    for biome in biomes {
        if previous == Some(biome) {
            writer.u8(SAME_AS_PREVIOUS);
        } else {
            biome.write_network(&mut writer);
        }
        previous = Some(biome);
    }
    // No border blocks.
    writer.u8(0);
    writer.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_pack_and_unpack_at_every_width() {
        for bits in BIT_SIZES {
            let largest = if bits == 0 { 0 } else { (1u32 << bits) - 1 };
            let indices: Vec<u16> = (0..VALUES as u32)
                .map(|position| (position.wrapping_mul(2_654_435_761) % (largest + 1)) as u16)
                .collect();
            let words = pack_indices(&indices, bits);
            assert_eq!(words.len(), packed_words(bits), "{bits} bits");
            assert_eq!(unpack_indices(&words, bits), indices, "{bits} bits");
        }
        // 3 bits: ten to a word, the last two bits padding.
        assert_eq!(packed_words(3), 410);
        assert_eq!((index_bits(1), index_bits(2), index_bits(5)), (0, 1, 3));
        assert_eq!((index_bits(33), index_bits(257)), (6, 16));
    }

    fn network(storage: &PalettedStorage) -> Vec<u8> {
        let mut writer = Writer::new();
        storage.write_network(&mut writer);
        writer.into_bytes()
    }

    #[test]
    fn single_value_storage_has_no_indices_or_palette_size() {
        // Header 0 << 1 | 1, then the value 1 as a zigzag varint.
        assert_eq!(network(&PalettedStorage::filled(1)), [0x01, 0x02]);
    }

    #[test]
    fn indices_pack_lsb_first_in_x_z_y_order() {
        let mut storage = PalettedStorage::filled(0);
        storage.set(0, 1, 0, 7);
        storage.set(0, 0, 1, 7);
        let bytes = network(&storage);

        // Two palette entries need 1 bit per index: 32 indices per word, 128 words.
        assert_eq!(bytes[0], (1 << 1) | 1);
        assert_eq!(bytes.len(), 1 + 128 * 4 + 1 + 2);
        // (x=0, z=0, y=1) is index 1 and (x=0, z=1, y=0) is index 16, both in word 0.
        let word = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
        assert_eq!(word, (1 << 1) | (1 << 16));
        // Palette: size 2, then values 0 and 7 as zigzag varints.
        assert_eq!(&bytes[bytes.len() - 3..], [4, 0, 14]);
        assert_eq!(storage.get(0, 1, 0), 7);
        assert_eq!(storage.get(1, 1, 1), 0);
    }

    #[test]
    fn index_widths_round_up_to_supported_sizes() {
        let mut storage = PalettedStorage::filled(0);
        for value in 1..=8 {
            storage.set(value as u8, 0, 0, value);
        }
        // Nine entries need 4 bits: 8 indices per word, 512 words.
        let bytes = network(&storage);
        assert_eq!(bytes[0] >> 1, 4);
        assert_eq!(bytes.len(), 1 + 512 * 4 + 1 + 9);

        for value in 9..=32 {
            storage.set((value % 16) as u8, 1 + (value / 16) as u8, 0, value);
        }
        // 33 entries need 6 bits: 5 indices per word, the last word partly unused.
        let bytes = network(&storage);
        assert_eq!(bytes[0] >> 1, 6);
        assert_eq!(bytes.len(), 1 + 820 * 4 + 1 + 33);
    }

    #[test]
    fn payload_lists_sub_chunks_then_deduplicated_biomes() {
        let sub_chunk = SubChunk {
            layers: vec![PalettedStorage::filled(5)],
        };
        let biomes = vec![PalettedStorage::filled(1); 3];
        let payload = level_chunk_payload(-4, &[sub_chunk], &biomes);
        assert_eq!(
            payload,
            [
                SUB_CHUNK_VERSION,
                1,
                0xFC,
                0x01,
                0x0A, // sub-chunk -4, one layer of value 5
                0x01,
                0x02, // first biome storage: all plains (1)
                SAME_AS_PREVIOUS,
                SAME_AS_PREVIOUS,
                0, // no border blocks
            ]
        );
    }
}
