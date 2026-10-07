//! Block states and their network IDs.
//!
//! With StartGame's `use_block_network_id_hashes` set, a block state's network
//! ID is the FNV-1a-32 hash of its little-endian NBT `{"name": …, "states":
//! {…}}` with states sorted by name and no version field. The client hashes its
//! own vanilla states the same way, so no block palette has to be sent. The byte
//! layout follows Dragonfly's `network_block_hash.go`.

/// A block state property value, as typed in vanilla block states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateValue {
    /// Also used for boolean states such as `infiniburn_bit`.
    Byte(u8),
    Int(i32),
    String(String),
}

/// A block and its state properties, e.g. `minecraft:bedrock` with `infiniburn_bit = 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockState {
    pub name: String,
    pub states: Vec<(String, StateValue)>,
}

impl BlockState {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            states: Vec::new(),
        }
    }

    pub fn with(mut self, state: impl Into<String>, value: StateValue) -> Self {
        self.states.push((state.into(), value));
        self
    }

    /// The hashed network ID used in chunk palettes.
    pub fn network_id(&self) -> u32 {
        if self.name == "minecraft:unknown" {
            return 0xFFFF_FFFE;
        }
        fnv1a_32(&self.hash_input())
    }

    /// Little-endian NBT with u16 string lengths: `{"name": name, "states": {sorted states}}`.
    fn hash_input(&self) -> Vec<u8> {
        fn string(data: &mut Vec<u8>, value: &str) {
            let len = u16::try_from(value.len()).expect("block state strings are short");
            data.extend_from_slice(&len.to_le_bytes());
            data.extend_from_slice(value.as_bytes());
        }

        let mut states: Vec<_> = self.states.iter().collect();
        states.sort_by(|(a, _), (b, _)| a.cmp(b));

        let mut data = vec![10, 0, 0];
        data.push(8);
        string(&mut data, "name");
        string(&mut data, &self.name);
        data.push(10);
        string(&mut data, "states");
        for (name, value) in states {
            match value {
                StateValue::Byte(byte) => {
                    data.push(1);
                    string(&mut data, name);
                    data.push(*byte);
                }
                StateValue::Int(int) => {
                    data.push(3);
                    string(&mut data, name);
                    data.extend_from_slice(&int.to_le_bytes());
                }
                StateValue::String(text) => {
                    data.push(8);
                    string(&mut data, name);
                    string(&mut data, text);
                }
            }
        }
        data.extend([0, 0]);
        data
    }
}

/// 32-bit FNV-1a.
pub fn fnv1a_32(data: &[u8]) -> u32 {
    data.iter().fold(0x811C_9DC5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_reference_vectors() {
        assert_eq!(fnv1a_32(b""), 0x811C_9DC5);
        assert_eq!(fnv1a_32(b"a"), 0xE40C_292C);
        assert_eq!(fnv1a_32(b"foobar"), 0xBF9C_F968);
    }

    #[test]
    fn hash_input_of_a_stateless_block() {
        let mut expected = vec![10, 0, 0, 8, 4, 0];
        expected.extend(b"name");
        expected.extend([13, 0]);
        expected.extend(b"minecraft:air");
        expected.extend([10, 6, 0]);
        expected.extend(b"states");
        expected.extend([0, 0]);
        let air = BlockState::new("minecraft:air");
        assert_eq!(air.hash_input(), expected);
        assert_eq!(air.network_id(), fnv1a_32(&expected));
    }

    #[test]
    fn states_are_sorted_and_typed() {
        let block = BlockState::new("minecraft:x")
            .with("b", StateValue::Int(-1))
            .with("a", StateValue::Byte(1))
            .with("c", StateValue::String("n".into()));
        let mut expected = vec![10, 0, 0, 8, 4, 0];
        expected.extend(b"name");
        expected.extend([11, 0]);
        expected.extend(b"minecraft:x");
        expected.extend([10, 6, 0]);
        expected.extend(b"states");
        expected.extend([1, 1, 0, b'a', 1]);
        expected.extend([3, 1, 0, b'b', 0xFF, 0xFF, 0xFF, 0xFF]);
        expected.extend([8, 1, 0, b'c', 1, 0, b'n']);
        expected.extend([0, 0]);
        assert_eq!(block.hash_input(), expected);
    }

    #[test]
    fn unknown_block_has_a_reserved_id() {
        assert_eq!(
            BlockState::new("minecraft:unknown").network_id(),
            0xFFFF_FFFE
        );
    }
}
