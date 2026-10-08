//! Saved worlds, in vanilla Bedrock's own layout, so a world folder from
//! Bedrock Dedicated Server or an unzipped `.mcworld` opens as it is.
//!
//! A world folder (`worlds/<level-name>/`) holds:
//! - `levelname.txt`: the world's display name ([`display_name`]);
//! - `level.dat`: its settings ([`LevelFile`]): spawn, game rules, the flat
//!   layers, versions; what vanilla needs to open the world;
//! - `db/`: a LevelDB database ([`WorldStorage`]) with a record per chunk
//!   part ([`keys`]: version, terrain, each sub-chunk) and one per player
//!   (`player_server_<uuid>`, little-endian NBT).
//!
//! [`WorldProvider`] is what the world needs from a backend: chunks, as
//! palettes of block names and states the way vanilla stores them, and
//! players. [`WorldStorage`] is the LevelDB backend.
//!
//! Not yet: the Nether and the End, entities and block entities, and biomes
//! (a vanilla chunk's are kept, but not read).

mod chunk_format;
pub mod keys;
mod level_dat;
mod leveldb;
mod player_format;

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::io::DecodeError;
use bedrockrs_protocol::types::ChunkPos;
use uuid::Uuid;

pub use level_dat::{LEVEL_DAT, LEVEL_DAT_OLD, LevelDat, LevelFile, NewWorld};
pub use leveldb::WorldStorage;

/// Blocks in a sub-chunk.
pub const BLOCKS: usize = 16 * 16 * 16;

/// The file in a world folder that names the world.
pub const LEVEL_NAME_FILE: &str = "levelname.txt";

/// Name of the placeholder for a block known only by its network ID (a block
/// the world has no name for). Its `network_id` state holds the ID.
pub const RAW_BLOCK: &str = "bedrockrs:raw_network_id";

/// One sub-chunk's blocks as stored: a palette of block states and an index
/// into it for each of the 4096 blocks, in x, z, y order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSubChunk {
    pub palette: Vec<BlockState>,
    pub indices: Vec<u16>,
}

/// A chunk column as stored: its sub-chunks from the bottom up, `None` where a
/// sub-chunk is all air.
pub type StoredColumn = Vec<Option<StoredSubChunk>>;

/// Why saved data could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("world database: {0}")]
    Database(#[from] bedrock_leveldb::LevelDbError),
    #[error("malformed NBT: {0}")]
    Nbt(#[from] DecodeError),
    #[error("sub-chunk format version {0} is not supported")]
    SubChunkVersion(u8),
    #[error("malformed chunk data: {0}")]
    Malformed(&'static str),
    #[error("malformed player data: {0}")]
    Player(&'static str),
}

/// Where a player was when they last left: their feet, where they looked (in
/// degrees), and whether they were flying. Saved as NBT under
/// `player_server_<uuid>`.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedPlayer {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub flying: bool,
    /// Absent for players saved without one: they get the starter kit.
    pub inventory: Option<SavedInventory>,
    /// The game mode's name. Absent for players saved without one (or with
    /// vanilla's "default"): those players get the default.
    pub game_mode: Option<String>,
    /// Absent for players saved without it: they have full health. 0 for a
    /// player who left while dead.
    pub health: Option<f32>,
}

/// A player's inventory, by slot. Items are saved by name, so their network
/// IDs may change between versions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedInventory {
    /// The 36 main slots, hotbar (0 to 8) first.
    pub main: Vec<SavedStack>,
    /// Head, chest, legs, feet.
    pub armor: Vec<SavedStack>,
    /// At most one stack, in slot 0.
    pub offhand: Vec<SavedStack>,
}

/// Some of one item in one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedStack {
    pub slot: u8,
    /// The item's name, such as `minecraft:stone`.
    pub item: String,
    pub count: u8,
    pub meta: u32,
    /// The item's NBT (an enchanted book's enchantment and the like), as
    /// little-endian NBT.
    pub nbt: Option<Vec<u8>>,
}

impl SavedPlayer {
    /// Whether every value is a real number, so the client can use it.
    pub fn is_finite(&self) -> bool {
        [self.x, self.y, self.z, self.pitch, self.yaw, self.head_yaw]
            .iter()
            .all(|value| value.is_finite())
    }
}

/// What the world needs from a storage backend.
pub trait WorldProvider: fmt::Debug + Send + Sync {
    /// The saved column at `chunk`, or `None` if it was never saved (it is
    /// generated).
    fn load_chunk(&self, chunk: ChunkPos) -> Result<Option<StoredColumn>, StoreError>;
    fn save_chunk(&self, chunk: ChunkPos, column: &StoredColumn) -> Result<(), StoreError>;
    /// The player's saved state, if they have been here before.
    fn load_player(&self, uuid: Uuid) -> Result<Option<SavedPlayer>, StoreError>;
    fn save_player(&self, uuid: Uuid, player: &SavedPlayer) -> Result<(), StoreError>;
    /// Makes everything saved so far durable, as the server stops.
    fn flush(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

/// The display name of the world in `directory`: the first line of its
/// `levelname.txt`, as vanilla names worlds, rather than its folder's name.
/// A world without one (a new world) gets `default`, written there.
pub fn display_name(directory: &Path, default: &str) -> io::Result<String> {
    let path = directory.join(LEVEL_NAME_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => {
            // Vanilla writes the name alone; tolerate a newline or a BOM.
            let name = text.trim_start_matches('\u{feff}').lines().next();
            Ok(name
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(default)
                .to_owned())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
            fs::write(&path, default)?;
            Ok(default.to_owned())
        }
        Err(err) => Err(err),
    }
}

/// The placeholder state for a block known only by its network ID.
pub fn raw_block(network_id: u32) -> BlockState {
    BlockState::new(RAW_BLOCK).with("network_id", StateValue::Int(network_id as i32))
}

/// The network ID a raw placeholder stands for, if `state` is one.
pub fn raw_network_id(state: &BlockState) -> Option<u32> {
    if state.name != RAW_BLOCK {
        return None;
    }
    match state.states.as_slice() {
        [(name, StateValue::Int(id))] if name == "network_id" => Some(*id as u32),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use super::*;

    /// An empty directory for a test, under the system's temporary one.
    pub(crate) fn temporary_world(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("bedrockrs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn worlds_are_named_by_their_levelname_file() {
        let world = temporary_world("levelname");
        // A new world is given the default, written for next time.
        assert_eq!(
            display_name(&world, "Bedrock level").unwrap(),
            "Bedrock level"
        );
        assert_eq!(
            fs::read_to_string(world.join(LEVEL_NAME_FILE)).unwrap(),
            "Bedrock level"
        );
        // A vanilla world's own name wins over the folder's.
        fs::write(world.join(LEVEL_NAME_FILE), "\u{feff}My Survival World\r\n").unwrap();
        assert_eq!(
            display_name(&world, "ignored").unwrap(),
            "My Survival World"
        );
        // An empty file falls back to the default.
        fs::write(world.join(LEVEL_NAME_FILE), "  \n").unwrap();
        assert_eq!(display_name(&world, "Fallback").unwrap(), "Fallback");
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn raw_placeholders_carry_their_network_id() {
        assert_eq!(raw_network_id(&raw_block(42)), Some(42));
        assert_eq!(raw_network_id(&BlockState::new("minecraft:stone")), None);
    }
}
