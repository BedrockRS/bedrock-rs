//! [`WorldStorage`]: a world's LevelDB database, `<world>/db/`, opened with
//! `bedrock-leveldb`, a pure Rust LevelDB that reads the zlib and raw
//! deflate table blocks Bedrock writes (compression IDs 2 and 4) and writes
//! zlib ones, which Bedrock reads.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use bedrock_leveldb::{CompressionPolicy, Db, OpenOptions, WriteBatch, WriteOptions};
use bedrockrs_protocol::nbt::Compound;
use bedrockrs_protocol::types::ChunkPos;
use uuid::Uuid;

use super::chunk_format::{
    CHUNK_VERSION, FINALIZED, decode_sub_chunk, encode_sub_chunk, generated_data_3d,
};
use super::keys::{ChunkKey, ChunkRecord, player_key};
use super::{SavedPlayer, StoreError, StoredColumn, WorldProvider, player_format};
use crate::world::{LOWEST_SUB_CHUNK, SUB_CHUNKS};

/// Where a world keeps its database, inside its folder.
pub const DATABASE_DIR: &str = "db";

/// A world's database. It is safe to share between threads: reads and
/// writes may come from any session and from the game loop at once.
pub struct WorldStorage {
    db: Db,
    path: PathBuf,
    /// Something was written since the last flush. Flushing nothing is an
    /// error to bedrock-leveldb (found 2026-10-08, stopping a server nobody
    /// had changed anything on).
    unflushed: AtomicBool,
}

impl fmt::Debug for WorldStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorldStorage")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl WorldStorage {
    /// Opens the database of the world in `world`, creating the folder and
    /// an empty database if there are none yet.
    pub fn open(world: &Path) -> Result<Self, StoreError> {
        fs::create_dir_all(world)?;
        let path = world.join(DATABASE_DIR);
        let options = OpenOptions {
            create_if_missing: true,
            compression_policy: CompressionPolicy::Zlib,
            ..OpenOptions::default()
        };
        let db = Db::open(&path, options)?;
        Ok(Self {
            db,
            path,
            unflushed: AtomicBool::new(false),
        })
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.db.get(key)?.map(|value| value.to_vec()))
    }

    fn has(&self, key: &[u8]) -> Result<bool, StoreError> {
        Ok(self.db.get(key)?.is_some())
    }
}

/// The key of `record` of the overworld chunk `chunk`.
fn key(chunk: ChunkPos, record: ChunkRecord) -> Vec<u8> {
    ChunkKey::overworld(chunk, record).to_bytes()
}

/// The vertical index of the `offset`th sub-chunk from the bottom.
fn y_index(offset: usize) -> i8 {
    LOWEST_SUB_CHUNK + i8::try_from(offset).expect("a column has 24 sub-chunks")
}

impl WorldProvider for WorldStorage {
    fn load_chunk(&self, chunk: ChunkPos) -> Result<Option<StoredColumn>, StoreError> {
        // Every chunk vanilla saved has a version record, current or legacy.
        if !self.has(&key(chunk, ChunkRecord::Version))?
            && !self.has(&key(chunk, ChunkRecord::LegacyVersion))?
        {
            return Ok(None);
        }
        let column = (0..SUB_CHUNKS)
            .map(|offset| {
                let record = ChunkRecord::SubChunk { y: y_index(offset) };
                match self.get(&key(chunk, record))? {
                    Some(bytes) => decode_sub_chunk(&bytes),
                    None => Ok(None),
                }
            })
            .collect::<Result<StoredColumn, _>>()?;
        // Worlds from before 1.0 kept a whole column in one record instead.
        if column.iter().all(Option::is_none)
            && self.has(&key(chunk, ChunkRecord::LegacyTerrain))?
        {
            return Err(StoreError::Malformed("terrain from before Bedrock 1.0"));
        }
        Ok(Some(column))
    }

    fn save_chunk(&self, chunk: ChunkPos, column: &StoredColumn) -> Result<(), StoreError> {
        // One batch, so a crash keeps either the old column or the new one.
        let mut batch = WriteBatch::new();
        // A chunk new to the database is marked as vanilla marks a generated
        // one; one it already had keeps its version, state and biomes.
        if !self.has(&key(chunk, ChunkRecord::Version))? {
            batch.put(key(chunk, ChunkRecord::Version), vec![CHUNK_VERSION]);
        }
        if !self.has(&key(chunk, ChunkRecord::FinalizedState))? {
            batch.put(
                key(chunk, ChunkRecord::FinalizedState),
                FINALIZED.to_le_bytes().to_vec(),
            );
        }
        if !self.has(&key(chunk, ChunkRecord::Data3D))?
            && !self.has(&key(chunk, ChunkRecord::Data2D))?
        {
            batch.put(key(chunk, ChunkRecord::Data3D), generated_data_3d());
        }
        for (offset, sub_chunk) in column.iter().enumerate() {
            let y = y_index(offset);
            let sub_chunk_key = key(chunk, ChunkRecord::SubChunk { y });
            match sub_chunk {
                Some(sub_chunk) => batch.put(sub_chunk_key, encode_sub_chunk(y, sub_chunk)),
                // All air: vanilla leaves the record out.
                None => batch.delete(sub_chunk_key),
            }
        }
        self.db.write(batch, WriteOptions::default())?;
        self.unflushed.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn load_player(&self, uuid: Uuid) -> Result<Option<SavedPlayer>, StoreError> {
        let Some(bytes) = self.get(player_key(uuid).as_bytes())? else {
            return Ok(None);
        };
        let compound = Compound::from_le_bytes(&bytes)?;
        player_format::decode(&compound).map(Some)
    }

    fn save_player(&self, uuid: Uuid, player: &SavedPlayer) -> Result<(), StoreError> {
        let bytes = player_format::encode(player).to_le_bytes();
        self.db.put(
            player_key(uuid).into_bytes(),
            bytes,
            WriteOptions::default(),
        )?;
        self.unflushed.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn flush(&self) -> Result<(), StoreError> {
        if self.unflushed.swap(false, Ordering::Relaxed)
            && let Err(err) = self.db.flush()
        {
            self.unflushed.store(true, Ordering::Relaxed);
            return Err(err.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bedrockrs_protocol::block::{BlockState, StateValue};

    use super::*;
    use crate::storage::tests::temporary_world;
    use crate::storage::{BLOCKS, StoredSubChunk};

    fn column() -> StoredColumn {
        let bedrock =
            BlockState::new("minecraft:bedrock").with("infiniburn_bit", StateValue::Byte(0));
        let mut bottom = StoredSubChunk {
            palette: vec![BlockState::new("minecraft:air"), bedrock],
            indices: vec![0; BLOCKS],
        };
        bottom.indices[100] = 1;
        let mut column = vec![None; SUB_CHUNKS];
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
    fn chunks_are_saved_as_vanilla_records_and_load_back() {
        let world = temporary_world("leveldb-chunks");
        let storage = WorldStorage::open(&world).unwrap();
        storage.flush().unwrap();
        let chunk = ChunkPos::new(-3, 12);
        assert_eq!(storage.load_chunk(chunk).unwrap(), None, "never saved");

        storage.save_chunk(chunk, &column()).unwrap();
        assert_eq!(storage.load_chunk(chunk).unwrap(), Some(column()));
        // The records vanilla expects, and none for the all-air sub-chunks.
        assert_eq!(
            storage.get(&key(chunk, ChunkRecord::Version)).unwrap(),
            Some(vec![CHUNK_VERSION])
        );
        assert_eq!(
            storage
                .get(&key(chunk, ChunkRecord::FinalizedState))
                .unwrap(),
            Some(vec![2, 0, 0, 0])
        );
        assert!(storage.has(&key(chunk, ChunkRecord::Data3D)).unwrap());
        assert!(
            storage
                .has(&key(chunk, ChunkRecord::SubChunk { y: -4 }))
                .unwrap()
        );
        assert!(
            !storage
                .has(&key(chunk, ChunkRecord::SubChunk { y: -3 }))
                .unwrap()
        );

        // A sub-chunk that became air loses its record.
        let mut emptied = column();
        emptied[5] = None;
        storage.save_chunk(chunk, &emptied).unwrap();
        assert!(
            !storage
                .has(&key(chunk, ChunkRecord::SubChunk { y: 1 }))
                .unwrap()
        );

        // Reopened, as after a restart.
        drop(storage);
        let storage = WorldStorage::open(&world).unwrap();
        assert_eq!(storage.load_chunk(chunk).unwrap(), Some(emptied));
        assert!(world.join(DATABASE_DIR).join("CURRENT").is_file());
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn a_vanilla_chunk_keeps_its_own_records() {
        let world = temporary_world("leveldb-vanilla");
        let storage = WorldStorage::open(&world).unwrap();
        let chunk = ChunkPos::new(0, 0);
        // A chunk vanilla saved, with its own version and biomes.
        let biomes = vec![7; 600];
        storage
            .db
            .put(
                key(chunk, ChunkRecord::Version),
                vec![40],
                WriteOptions::default(),
            )
            .unwrap();
        storage
            .db
            .put(
                key(chunk, ChunkRecord::Data3D),
                biomes.clone(),
                WriteOptions::default(),
            )
            .unwrap();
        assert_eq!(
            storage.load_chunk(chunk).unwrap(),
            Some(vec![None; SUB_CHUNKS])
        );

        storage.save_chunk(chunk, &column()).unwrap();
        assert_eq!(
            storage.get(&key(chunk, ChunkRecord::Version)).unwrap(),
            Some(vec![40])
        );
        assert_eq!(
            storage.get(&key(chunk, ChunkRecord::Data3D)).unwrap(),
            Some(biomes)
        );
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn damaged_records_are_errors_not_panics() {
        let world = temporary_world("leveldb-damaged");
        let storage = WorldStorage::open(&world).unwrap();
        let chunk = ChunkPos::new(5, 5);
        storage.save_chunk(chunk, &column()).unwrap();
        storage
            .db
            .put(
                key(chunk, ChunkRecord::SubChunk { y: -4 }),
                b"garbage".to_vec(),
                WriteOptions::default(),
            )
            .unwrap();
        assert!(storage.load_chunk(chunk).is_err());

        let uuid = Uuid::new_v4();
        storage
            .db
            .put(
                player_key(uuid).into_bytes(),
                b"{ not nbt".to_vec(),
                WriteOptions::default(),
            )
            .unwrap();
        assert!(storage.load_player(uuid).is_err());
        fs::remove_dir_all(&world).unwrap();
    }

    /// Reads every chunk and player of a real vanilla world, then changes a
    /// chunk and reads it back. Run it on a copy of a world (the database is
    /// opened for writing), named in `BEDROCKRS_VANILLA_WORLD`:
    /// `cargo test -p bedrockrs_core vanilla_world -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a copy of a vanilla world in BEDROCKRS_VANILLA_WORLD"]
    fn a_vanilla_world_reads_and_takes_changes() {
        let Some(path) = std::env::var_os("BEDROCKRS_VANILLA_WORLD") else {
            panic!("set BEDROCKRS_VANILLA_WORLD to a copy of a vanilla world folder");
        };
        let world = Path::new(&path);
        let storage = WorldStorage::open(world).unwrap();
        let keys = storage
            .db
            .collect_keys_owned(bedrock_leveldb::ReadOptions::default())
            .unwrap();
        let mut chunks = Vec::new();
        let mut players = Vec::new();
        for key in &keys {
            if key.len() == 9 && key[8] == ChunkRecord::Version.tag() {
                let x = i32::from_le_bytes(key[0..4].try_into().unwrap());
                let z = i32::from_le_bytes(key[4..8].try_into().unwrap());
                chunks.push(ChunkPos::new(x, z));
            }
            if let Some(uuid) = key.strip_prefix(b"player_server_") {
                players.push(Uuid::parse_str(std::str::from_utf8(uuid).unwrap()).unwrap());
            }
            // Records that are not a chunk's (their keys are text).
            if key.len() > 14 || key.iter().all(|byte| byte.is_ascii_graphic()) {
                let value = storage.get(key).unwrap().unwrap_or_default();
                let shown = if value
                    .iter()
                    .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
                {
                    String::from_utf8_lossy(&value).into_owned()
                } else {
                    format!("{} bytes", value.len())
                };
                println!("record {:?}: {shown}", String::from_utf8_lossy(key));
            }
        }
        let mut sub_chunks = 0;
        let mut names = std::collections::BTreeSet::new();
        let mut failed = Vec::new();
        for chunk in &chunks {
            match storage.load_chunk(*chunk) {
                Ok(Some(column)) => {
                    for sub_chunk in column.iter().flatten() {
                        sub_chunks += 1;
                        names.extend(sub_chunk.palette.iter().map(|state| state.name.clone()));
                    }
                }
                Ok(None) => failed.push((*chunk, "no version".to_owned())),
                Err(err) => failed.push((*chunk, err.to_string())),
            }
        }
        println!(
            "{} keys, {} overworld chunks ({} sub-chunks, {} kinds of block), {} failed: {:?}",
            keys.len(),
            chunks.len(),
            sub_chunks,
            names.len(),
            failed.len(),
            failed.iter().take(5).collect::<Vec<_>>()
        );
        for uuid in &players {
            println!("player {uuid}: {:?}", storage.load_player(*uuid));
        }
        // A single-player world's player is `~local_player`, in the same format.
        if let Some(bytes) = storage.get(b"~local_player").unwrap() {
            let compound = Compound::from_le_bytes(&bytes).unwrap();
            let names: Vec<&str> = compound.0.iter().map(|(name, _)| name.as_str()).collect();
            println!("~local_player has {names:?}");
            for name in ["Pos", "Rotation", "PlayerGameMode", "abilities", "Armor"] {
                println!("  {name}: {:?}", compound.get(name));
            }
            println!(
                "  as a saved player: {:?}",
                player_format::decode(&compound)
            );
        }
        assert!(failed.is_empty());
        println!(
            "levelname.txt: {:?}",
            crate::storage::display_name(world, "?")
        );

        // A change to a vanilla chunk is saved and reads back.
        if let Some(chunk) = chunks.first() {
            let mut column = storage.load_chunk(*chunk).unwrap().unwrap();
            column[0] = Some(StoredSubChunk {
                palette: vec![BlockState::new("minecraft:gold_block")],
                indices: vec![0; BLOCKS],
            });
            storage.save_chunk(*chunk, &column).unwrap();
            storage.flush().unwrap();
            drop(storage);
            let storage = WorldStorage::open(world).unwrap();
            assert_eq!(storage.load_chunk(*chunk).unwrap(), Some(column));
            println!("changed chunk {chunk:?}: its bottom sub-chunk is gold");
        }
    }

    #[test]
    fn players_are_kept_under_their_uuid() {
        let world = temporary_world("leveldb-players");
        let storage = WorldStorage::open(&world).unwrap();
        let uuid = Uuid::new_v4();
        assert_eq!(storage.load_player(uuid).unwrap(), None, "never seen");
        let player = SavedPlayer {
            x: 0.5,
            y: -60.0,
            z: 0.5,
            pitch: 0.0,
            yaw: 45.0,
            head_yaw: 45.0,
            flying: false,
            inventory: None,
            game_mode: Some("survival".into()),
            health: Some(20.0),
        };
        storage.save_player(uuid, &player).unwrap();
        storage.flush().unwrap();
        drop(storage);

        let storage = WorldStorage::open(&world).unwrap();
        let loaded = storage.load_player(uuid).unwrap().unwrap();
        assert!((loaded.y - player.y).abs() < 1e-4);
        assert_eq!(loaded.game_mode, player.game_mode);
        assert!(storage.has(player_key(uuid).as_bytes()).unwrap());
        fs::remove_dir_all(&world).unwrap();
    }
}
