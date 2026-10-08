//! Saved world data: changed chunks, and where players left.
//!
//! [`WorldStorage`] is what the world needs from a storage backend. Blocks
//! cross it as palettes of block names and states, the way vanilla worlds
//! store them, so a LevelDB backend for vanilla worlds can sit beside
//! [`BinStorage`], BedrockRS's own format.
//!
//! [`BinStorage`] keeps one small compressed file per changed chunk column in
//! `<world>/chunks/c.<x>.<z>.bin`, and one JSON file per player in
//! `<world>/players/<uuid>.json`. Files are written to a temporary file and
//! renamed over the old one, so a crash mid-save never leaves half a file.
//!
//! Chunk file layout: the magic `MVCH`, a format version byte, then zlib data.
//! The data is the sub-chunk count, then per sub-chunk from the bottom a
//! presence byte and, if present:
//! - version 2 (written now): a palette (u16 count; per entry the block name, a
//!   state count byte, then per state its name, a type byte — 1 byte, 3 int,
//!   8 string — and value) followed by 4096 u16 palette indices;
//! - version 1 (still read): 4096 u32 block network IDs (state hashes).
//!
//! Strings are a u16 length and UTF-8; numbers are little-endian. Blocks are in
//! x, z, y order, as in paletted storage.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::types::ChunkPos;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAGIC: &[u8; 4] = b"MVCH";
/// The chunk format version written.
const VERSION: u8 = 2;
/// Blocks in a sub-chunk.
pub const BLOCKS: usize = 16 * 16 * 16;
/// Largest decompressed chunk accepted: 64 sub-chunks with the biggest
/// palettes version 1 or 2 allow, rounded up generously.
const MAX_DATA: u64 = 64 * 1024 * 1024;

/// Name of the placeholder for a block known only by its network ID (from
/// version 1 files, or a block the world has no name for). Its `network_id`
/// state holds the ID.
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

/// Why saved data could not be read.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("not a BedrockRS chunk file")]
    NotAChunk,
    #[error("unsupported chunk format version {0}")]
    Version(u8),
    #[error("truncated or malformed chunk data")]
    Malformed,
    #[error("invalid player file: {0}")]
    Json(#[from] serde_json::Error),
}

/// Where a player was when they last left: their feet, where they looked (in
/// degrees), and whether they were flying. Saved as `players/<uuid>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPlayer {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    /// Absent from files saved before flying was remembered.
    #[serde(default)]
    pub flying: bool,
    /// Absent from files saved before inventories were: those players get
    /// the starter kit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<SavedInventory>,
    /// The game mode's name. Absent from files saved before game modes were:
    /// those players get the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_mode: Option<String>,
    /// Absent from files saved before health was: those players have full
    /// health. 0 for a player who left while dead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<f32>,
}

/// A player's inventory, by slot. Items are saved by name, so their network
/// IDs may change between versions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedInventory {
    /// The 36 main slots, hotbar (0 to 8) first.
    #[serde(default)]
    pub main: Vec<SavedStack>,
    #[serde(default)]
    pub armor: Vec<SavedStack>,
    /// At most one stack, in slot 0.
    #[serde(default)]
    pub offhand: Vec<SavedStack>,
}

/// Some of one item in one slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedStack {
    pub slot: u8,
    /// The item's name, such as `minecraft:stone`.
    pub item: String,
    pub count: u8,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub meta: u32,
    /// The item's NBT (an enchanted book's enchantment and the like), as
    /// little-endian NBT, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbt: Option<String>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
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
pub trait WorldStorage: fmt::Debug + Send + Sync {
    /// The chunks with saved changes, listed without reading them.
    fn saved_chunks(&self) -> io::Result<HashSet<ChunkPos>>;
    fn load_chunk(&self, chunk: ChunkPos) -> Result<StoredColumn, StoreError>;
    fn save_chunk(&self, chunk: ChunkPos, column: &StoredColumn) -> io::Result<()>;
    /// The player's saved state, if they have been here before.
    fn load_player(&self, uuid: Uuid) -> Result<Option<SavedPlayer>, StoreError>;
    fn save_player(&self, uuid: Uuid, player: &SavedPlayer) -> io::Result<()>;
}

/// BedrockRS's own format: a directory of chunk and player files.
#[derive(Debug)]
pub struct BinStorage {
    chunks: PathBuf,
    players: PathBuf,
}

impl BinStorage {
    /// Opens the world in `world`, creating its directories if needed.
    pub fn open(world: &Path) -> io::Result<Self> {
        let chunks = world.join("chunks");
        let players = world.join("players");
        fs::create_dir_all(&chunks)?;
        fs::create_dir_all(&players)?;
        Ok(Self { chunks, players })
    }

    fn chunk_path(&self, chunk: ChunkPos) -> PathBuf {
        self.chunks.join(format!("c.{}.{}.bin", chunk.x, chunk.z))
    }

    fn player_path(&self, uuid: Uuid) -> PathBuf {
        // The hyphenated UUID is a safe file name on every platform.
        self.players.join(format!("{}.json", uuid.hyphenated()))
    }
}

impl WorldStorage for BinStorage {
    fn saved_chunks(&self) -> io::Result<HashSet<ChunkPos>> {
        let mut saved = HashSet::new();
        for entry in fs::read_dir(&self.chunks)? {
            if let Some(chunk) = entry?.file_name().to_str().and_then(parse_file_name) {
                saved.insert(chunk);
            }
        }
        Ok(saved)
    }

    fn load_chunk(&self, chunk: ChunkPos) -> Result<StoredColumn, StoreError> {
        decode(&fs::read(self.chunk_path(chunk))?)
    }

    fn save_chunk(&self, chunk: ChunkPos, column: &StoredColumn) -> io::Result<()> {
        write_atomically(&self.chunk_path(chunk), &encode(column)?)
    }

    fn load_player(&self, uuid: Uuid) -> Result<Option<SavedPlayer>, StoreError> {
        match fs::read(self.player_path(uuid)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn save_player(&self, uuid: Uuid, player: &SavedPlayer) -> io::Result<()> {
        write_atomically(&self.player_path(uuid), &serde_json::to_vec_pretty(player)?)
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

/// `c.<x>.<z>.bin` back to its chunk position.
fn parse_file_name(name: &str) -> Option<ChunkPos> {
    let (x, z) = name
        .strip_prefix("c.")?
        .strip_suffix(".bin")?
        .split_once('.')?;
    Some(ChunkPos::new(x.parse().ok()?, z.parse().ok()?))
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

fn encode(column: &StoredColumn) -> io::Result<Vec<u8>> {
    let mut file = MAGIC.to_vec();
    file.push(VERSION);
    let mut data = Vec::new();
    data.push(u8::try_from(column.len()).expect("a column has at most 64 sub-chunks"));
    for sub_chunk in column {
        let Some(sub_chunk) = sub_chunk else {
            data.push(0);
            continue;
        };
        assert_eq!(
            sub_chunk.indices.len(),
            BLOCKS,
            "a sub-chunk has 4096 blocks"
        );
        data.push(1);
        let count = u16::try_from(sub_chunk.palette.len()).expect("at most 4096 palette entries");
        data.extend(count.to_le_bytes());
        for state in &sub_chunk.palette {
            write_string(&mut data, &state.name);
            data.push(u8::try_from(state.states.len()).expect("blocks have few states"));
            for (name, value) in &state.states {
                write_string(&mut data, name);
                match value {
                    StateValue::Byte(byte) => data.extend([1, *byte]),
                    StateValue::Int(int) => {
                        data.push(3);
                        data.extend(int.to_le_bytes());
                    }
                    StateValue::String(text) => {
                        data.push(8);
                        write_string(&mut data, text);
                    }
                }
            }
        }
        for index in &sub_chunk.indices {
            data.extend(index.to_le_bytes());
        }
    }
    let mut encoder = ZlibEncoder::new(file, Compression::default());
    encoder.write_all(&data)?;
    encoder.finish()
}

fn write_string(data: &mut Vec<u8>, text: &str) {
    let len = u16::try_from(text.len()).expect("block names and states are short");
    data.extend(len.to_le_bytes());
    data.extend(text.as_bytes());
}

fn decode(bytes: &[u8]) -> Result<StoredColumn, StoreError> {
    let (header, compressed) = bytes.split_at_checked(5).ok_or(StoreError::NotAChunk)?;
    if &header[..4] != MAGIC {
        return Err(StoreError::NotAChunk);
    }
    let version = header[4];
    if !matches!(version, 1 | 2) {
        return Err(StoreError::Version(version));
    }
    // Refuse to inflate more than a chunk can hold.
    let mut data = Vec::new();
    ZlibDecoder::new(compressed)
        .take(MAX_DATA + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_DATA {
        return Err(StoreError::Malformed);
    }

    let mut input = Input(&data);
    let count = input.u8()?;
    let mut column = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        if input.u8()? == 0 {
            column.push(None);
            continue;
        }
        let sub_chunk = match version {
            1 => read_hashed_sub_chunk(&mut input)?,
            _ => read_named_sub_chunk(&mut input)?,
        };
        column.push(Some(sub_chunk));
    }
    if !input.0.is_empty() {
        return Err(StoreError::Malformed);
    }
    Ok(column)
}

/// Version 2: a palette of names and states, then indices into it.
fn read_named_sub_chunk(input: &mut Input<'_>) -> Result<StoredSubChunk, StoreError> {
    let count = input.u16()?;
    if count == 0 || usize::from(count) > BLOCKS {
        return Err(StoreError::Malformed);
    }
    let mut palette = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let mut state = BlockState::new(input.string()?);
        for _ in 0..input.u8()? {
            let name = input.string()?;
            let value = match input.u8()? {
                1 => StateValue::Byte(input.u8()?),
                3 => StateValue::Int(i32::from_le_bytes(input.array()?)),
                8 => StateValue::String(input.string()?),
                _ => return Err(StoreError::Malformed),
            };
            state = state.with(name, value);
        }
        palette.push(state);
    }
    let indices = (0..BLOCKS)
        .map(|_| input.u16())
        .collect::<Result<Vec<_>, _>>()?;
    if indices
        .iter()
        .any(|index| usize::from(*index) >= palette.len())
    {
        return Err(StoreError::Malformed);
    }
    Ok(StoredSubChunk { palette, indices })
}

/// Version 1: a network ID per block, turned into a palette of raw placeholders.
fn read_hashed_sub_chunk(input: &mut Input<'_>) -> Result<StoredSubChunk, StoreError> {
    let mut slots: HashMap<u32, u16> = HashMap::new();
    let mut palette = Vec::new();
    let mut indices = Vec::with_capacity(BLOCKS);
    for _ in 0..BLOCKS {
        let id = u32::from_le_bytes(input.array()?);
        let index = *slots.entry(id).or_insert_with(|| {
            palette.push(raw_block(id));
            (palette.len() - 1) as u16
        });
        indices.push(index);
    }
    Ok(StoredSubChunk { palette, indices })
}

/// A cursor over decompressed chunk data.
struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], StoreError> {
        let (head, rest) = self.0.split_at_checked(N).ok_or(StoreError::Malformed)?;
        self.0 = rest;
        Ok(head.try_into().expect("N bytes"))
    }

    fn u8(&mut self) -> Result<u8, StoreError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, StoreError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Result<String, StoreError> {
        let len = usize::from(self.u16()?);
        let (text, rest) = self.0.split_at_checked(len).ok_or(StoreError::Malformed)?;
        self.0 = rest;
        String::from_utf8(text.to_vec()).map_err(|_| StoreError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_world(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("bedrockrs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    fn column() -> StoredColumn {
        let bedrock =
            BlockState::new("minecraft:bedrock").with("infiniburn_bit", StateValue::Byte(0));
        let mut bottom = StoredSubChunk {
            palette: vec![BlockState::new("minecraft:air"), bedrock],
            indices: vec![0; BLOCKS],
        };
        bottom.indices[100] = 1;
        let mut column = vec![None; 24];
        column[0] = Some(bottom);
        column[5] = Some(StoredSubChunk {
            palette: vec![
                BlockState::new("minecraft:stone"),
                BlockState::new("bedrockrs:test")
                    .with("number", StateValue::Int(-7))
                    .with("colour", StateValue::String("red".into())),
            ],
            indices: (0..BLOCKS).map(|index| (index % 2) as u16).collect(),
        });
        column
    }

    #[test]
    fn saved_chunks_load_back_and_are_listed() {
        let world = temporary_world("store");
        let storage = BinStorage::open(&world).unwrap();
        assert!(storage.saved_chunks().unwrap().is_empty());
        storage
            .save_chunk(ChunkPos::new(-3, 12), &column())
            .unwrap();
        assert_eq!(storage.load_chunk(ChunkPos::new(-3, 12)).unwrap(), column());

        // Stray files are ignored when listing.
        fs::write(world.join("chunks").join("notes.txt"), "hi").unwrap();
        let storage = BinStorage::open(&world).unwrap();
        assert_eq!(
            storage.saved_chunks().unwrap(),
            HashSet::from([ChunkPos::new(-3, 12)])
        );
        // Names are stored once per palette, so files stay small.
        let size = fs::metadata(storage.chunk_path(ChunkPos::new(-3, 12)))
            .unwrap()
            .len();
        assert!(size < 1024, "{size} bytes");
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn version_1_files_load_as_raw_network_ids() {
        // A version 1 file: one sub-chunk of ID 7 with an ID 42 at index 100.
        let mut data = vec![1, 1];
        for index in 0..BLOCKS {
            let id: u32 = if index == 100 { 42 } else { 7 };
            data.extend(id.to_le_bytes());
        }
        let mut file = MAGIC.to_vec();
        file.push(1);
        let mut encoder = ZlibEncoder::new(file, Compression::default());
        encoder.write_all(&data).unwrap();
        let bytes = encoder.finish().unwrap();

        let column = decode(&bytes).unwrap();
        let sub_chunk = column[0].as_ref().unwrap();
        assert_eq!(sub_chunk.palette, [raw_block(7), raw_block(42)]);
        assert_eq!(sub_chunk.indices[100], 1);
        assert_eq!(raw_network_id(&sub_chunk.palette[1]), Some(42));
        assert_eq!(raw_network_id(&BlockState::new("minecraft:stone")), None);
    }

    #[test]
    fn players_are_saved_by_uuid_and_older_files_still_load() {
        let world = temporary_world("players");
        let storage = BinStorage::open(&world).unwrap();
        let uuid = Uuid::new_v4();
        assert_eq!(storage.load_player(uuid).unwrap(), None, "never seen");

        let player = SavedPlayer {
            x: 120.5,
            y: -60.0,
            z: -33.25,
            pitch: 10.0,
            yaw: -90.0,
            head_yaw: -85.0,
            flying: true,
            inventory: Some(SavedInventory {
                main: vec![SavedStack {
                    nbt: None,
                    slot: 4,
                    item: "minecraft:stone".into(),
                    count: 12,
                    meta: 0,
                }],
                ..SavedInventory::default()
            }),
            game_mode: Some("adventure".into()),
            health: Some(13.0),
        };
        storage.save_player(uuid, &player).unwrap();
        assert_eq!(storage.load_player(uuid).unwrap(), Some(player.clone()));
        assert!(world.join("players").join(format!("{uuid}.json")).is_file());

        // Files from before flying was saved mean "not flying".
        let older = r#"{"x":1.0,"y":-60.0,"z":2.0,"pitch":0.0,"yaw":0.0,"head_yaw":0.0}"#;
        fs::write(storage.player_path(uuid), older).unwrap();
        let older = storage.load_player(uuid).unwrap().unwrap();
        assert!(!older.flying);
        assert_eq!(older.game_mode, None, "the default game mode applies");

        fs::write(storage.player_path(uuid), "{ not json").unwrap();
        assert!(matches!(
            storage.load_player(uuid),
            Err(StoreError::Json(_))
        ));
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        assert!(matches!(decode(b"nope"), Err(StoreError::NotAChunk)));
        assert!(matches!(
            decode(b"MVCH\x09..."),
            Err(StoreError::Version(9))
        ));
        let mut truncated = encode(&column()).unwrap();
        truncated.truncate(truncated.len() / 2);
        assert!(decode(&truncated).is_err());

        // An index past the palette is caught.
        let mut bad = column();
        bad[0].as_mut().unwrap().indices[0] = 9;
        assert!(matches!(
            decode(&encode(&bad).unwrap()),
            Err(StoreError::Malformed)
        ));

        assert!(parse_file_name("c.1.x.bin").is_none());
        assert_eq!(parse_file_name("c.-1.2.bin"), Some(ChunkPos::new(-1, 2)));
    }
}
