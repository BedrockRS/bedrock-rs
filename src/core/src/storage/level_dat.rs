//! `level.dat`: the world's settings, as vanilla keeps them beside the
//! database, and what vanilla needs to open a world at all.
//!
//! The file is an 8-byte header, the storage version (`i32` LE, 10 now) and
//! the length of what follows (`i32` LE), then a little-endian NBT compound.
//! Vanilla writes `level.dat_old`, the file before its last write, beside it,
//! and so does [`LevelFile`].
//!
//! A world BedrockRS creates gets every field a vanilla 1.26.52 superflat
//! world has (read from one on 2026-10-08), with the server's own values for
//! what it decides: the name, the flat layers BedrockRS generates (as
//! post-1.18 layers, from y = -64), spawn, time, difficulty, game mode and
//! game rules. A world that already has one keeps every field; the server
//! only updates the last-played time, the default game mode, its game rules,
//! and the versions it was opened with if they are older.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use bedrockrs_protocol::io::Reader;
use bedrockrs_protocol::nbt::{Compound, Tag, TagKind};
use bedrockrs_protocol::types::BlockPos;
use bedrockrs_protocol::{GAME_VERSION, PROTOCOL_VERSION};

use super::StoreError;
use crate::world::{MAX_Y, MIN_Y};

pub const LEVEL_DAT: &str = "level.dat";
pub const LEVEL_DAT_OLD: &str = "level.dat_old";

/// The storage version vanilla writes in the header and `StorageVersion`.
const STORAGE_VERSION: i32 = 10;

/// `SpawnY` while vanilla has not chosen a spawn yet.
const SPAWN_NOT_CHOSEN: i32 = 32767;

/// The oldest client that may open a world BedrockRS created: the release
/// whose block palette it uses.
const MINIMUM_CLIENT_VERSION: [i32; 5] = [1, 26, 50, 0, 0];

/// The layers BedrockRS generates, as vanilla describes a superflat world:
/// the classic preset (bedrock, two dirt, grass, plains) from y = -64, which
/// the post-1.18 `world_version` says. Vanilla ends the text with a newline.
const FLAT_WORLD_LAYERS: &str = concat!(
    r#"{"biome_id":1,"block_layers":[{"block_name":"minecraft:bedrock","count":1},"#,
    r#"{"block_name":"minecraft:dirt","count":2},{"block_name":"minecraft:grass_block","count":1}],"#,
    r#""encoding_version":6,"preset_id":"ClassicFlat","world_version":"version.post_1_18"}"#,
    "\n"
);

/// What a new world starts with.
#[derive(Debug, Clone, PartialEq)]
pub struct NewWorld<'a> {
    pub name: &'a str,
    pub spawn: BlockPos,
    /// The default game mode's number.
    pub game_type: i32,
    /// The game rules BedrockRS keeps, by vanilla name.
    pub game_rules: &'a [(&'a str, bool)],
}

/// A world's `level.dat`: every field, in vanilla's order.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelDat(pub Compound);

impl LevelDat {
    /// The `level.dat` of a world BedrockRS creates.
    pub fn new_world(world: &NewWorld<'_>) -> Self {
        let byte = |value: bool| Tag::Byte(value.into());
        let ints = |values: &[i32]| {
            Tag::List(
                TagKind::Int,
                values.iter().map(|value| Tag::Int(*value)).collect(),
            )
        };
        let version = game_version();
        let creative = world.game_type == 1;
        let abilities = Compound::new()
            .with("attackmobs", byte(true))
            .with("attackplayers", byte(true))
            .with("build", byte(true))
            .with("doorsandswitches", byte(true))
            .with("flySpeed", Tag::Float(0.05))
            .with("flying", byte(false))
            .with("instabuild", byte(false))
            .with("invulnerable", byte(false))
            .with("lightning", byte(false))
            .with("mayfly", byte(false))
            .with("mine", byte(true))
            .with("op", byte(false))
            .with("opencontainers", byte(true))
            .with("teleport", byte(false))
            .with("verticalFlySpeed", Tag::Float(1.0))
            .with("walkSpeed", Tag::Float(0.1));
        let compound = Compound::new()
            .with("BiomeOverride", Tag::String(String::new()))
            .with("CenterMapsToOrigin", byte(false))
            .with("ConfirmedPlatformLockedContent", byte(false))
            // Peaceful, as the server tells clients.
            .with("Difficulty", Tag::Int(0))
            .with("FlatWorldLayers", Tag::String(FLAT_WORLD_LAYERS.into()))
            .with("ForceGameType", byte(false))
            .with("GameType", Tag::Int(world.game_type))
            // Superflat.
            .with("Generator", Tag::Int(2))
            .with("HasUncompleteWorldFileOnDisk", byte(false))
            .with("InventoryVersion", Tag::String(GAME_VERSION.into()))
            .with("IsHardcore", byte(false))
            .with("LANBroadcast", byte(true))
            .with("LANBroadcastIntent", byte(true))
            .with("LastPlayed", Tag::Long(now()))
            .with("LevelName", Tag::String(world.name.into()))
            .with("LimitedWorldOriginX", Tag::Int(0))
            .with("LimitedWorldOriginY", Tag::Int(SPAWN_NOT_CHOSEN))
            .with("LimitedWorldOriginZ", Tag::Int(0))
            .with(
                "MinimumCompatibleClientVersion",
                ints(&MINIMUM_CLIENT_VERSION),
            )
            .with("MultiplayerGame", byte(true))
            .with("MultiplayerGameIntent", byte(true))
            .with("NetherScale", Tag::Int(8))
            .with("NetworkVersion", Tag::Int(PROTOCOL_VERSION))
            .with("Platform", Tag::Int(2))
            .with("PlatformBroadcastIntent", Tag::Int(2))
            .with("PlayerHasDied", byte(false))
            .with("RandomSeed", Tag::Long(0))
            .with("SpawnV1Villagers", byte(false))
            .with("SpawnX", Tag::Int(world.spawn.x))
            .with("SpawnY", Tag::Int(world.spawn.y))
            .with("SpawnZ", Tag::Int(world.spawn.z))
            .with("StorageVersion", Tag::Int(STORAGE_VERSION))
            // Noon, as the server tells clients.
            .with("Time", Tag::Long(6000))
            // Post-1.18: the overworld goes down to y = -64.
            .with("WorldVersion", Tag::Int(1))
            .with("XBLBroadcastIntent", Tag::Int(1))
            .with("abilities", Tag::Compound(abilities))
            .with("allowAnonymousBlockDropsInEditorWorlds", byte(false))
            .with("baseGameVersion", Tag::String("*".into()))
            .with("bonusChestEnabled", byte(false))
            .with("bonusChestSpawned", byte(false))
            .with("cheatsEnabled", byte(false))
            .with("commandblockoutput", byte(true))
            .with("commandblocksenabled", byte(true))
            .with("commandsEnabled", byte(true))
            .with("currentTick", Tag::Long(0))
            .with("daylightCycle", Tag::Int(0))
            .with("dodaylightcycle", byte(true))
            .with("doentitydrops", byte(true))
            .with("dofiretick", byte(true))
            .with("doimmediaterespawn", byte(false))
            .with("doinsomnia", byte(true))
            .with("dolimitedcrafting", byte(false))
            .with("domobloot", byte(true))
            .with("domobspawning", byte(true))
            .with("dotiledrops", byte(true))
            .with("doweathercycle", byte(true))
            .with("drowningdamage", byte(true))
            .with("editorWorldType", Tag::Int(0))
            .with("eduOffer", Tag::Int(0))
            .with("educationFeaturesEnabled", byte(false))
            .with("experiments", Tag::Compound(Compound::new()))
            .with("falldamage", byte(true))
            .with("firedamage", byte(true))
            .with("freezedamage", byte(true))
            .with("functioncommandlimit", Tag::Int(10000))
            .with("hasBeenLoadedInCreative", byte(creative))
            .with("hasLockedBehaviorPack", byte(false))
            .with("hasLockedResourcePack", byte(false))
            .with("immutableWorld", byte(false))
            .with("isCreatedInEditor", byte(false))
            .with("isExportedFromEditor", byte(false))
            .with("isFromLockedTemplate", byte(false))
            .with("isFromWorldTemplate", byte(false))
            .with("isRandomSeedAllowed", byte(false))
            .with("isSingleUseWorld", byte(false))
            .with("isWorldTemplateOptionLocked", byte(false))
            .with("keepinventory", byte(false))
            .with("lastOpenedWithVersion", ints(&version))
            .with("lightningLevel", Tag::Float(0.0))
            .with("lightningTime", Tag::Int(100_000))
            .with("limitedWorldDepth", Tag::Int(16))
            .with("limitedWorldWidth", Tag::Int(16))
            .with("maxcommandchainlength", Tag::Int(65535))
            .with("mobgriefing", byte(true))
            .with("naturalregeneration", byte(true))
            .with("permissionsLevel", Tag::Int(0))
            .with("playerPermissionsLevel", Tag::Int(1))
            .with("playerssleepingpercentage", Tag::Int(100))
            .with("playerwaypoints", Tag::Int(1))
            .with("prid", Tag::String(String::new()))
            .with("projectilescanbreakblocks", byte(true))
            .with("pvp", byte(true))
            .with("rainLevel", Tag::Float(0.0))
            .with("rainTime", Tag::Int(100_000))
            .with("randomtickspeed", Tag::Int(1))
            .with("recipesunlock", byte(true))
            .with("requiresCopiedPackRemovalCheck", byte(false))
            .with("respawnblocksexplode", byte(true))
            .with("sendcommandfeedback", byte(true))
            .with("serverChunkTickRange", Tag::Int(4))
            .with("serverEditorConnectionPolicy", Tag::Int(0))
            .with("showbordereffect", byte(true))
            .with("showcoordinates", byte(false))
            .with("showdaysplayed", byte(false))
            .with("showdeathmessages", byte(true))
            .with("showrecipemessages", byte(true))
            .with("showtags", byte(true))
            .with("spawnMobs", byte(true))
            .with("spawnradius", Tag::Int(5))
            .with("startWithMapEnabled", byte(false))
            .with("texturePacksRequired", byte(false))
            .with("tntexplodes", byte(true))
            .with("tntexplosiondropdecay", byte(false))
            .with("useMsaGamertagsOnly", byte(false))
            .with("worldStartCount", Tag::Long(i64::from(u32::MAX)))
            .with("world_policies", Tag::Compound(Compound::new()));
        let mut level = Self(compound);
        for (rule, value) in world.game_rules {
            level.set_game_rule(rule, *value);
        }
        level
    }

    /// Reads a `level.dat`'s bytes: the header, then the compound.
    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        let mut reader = Reader::new(bytes);
        let _storage_version = reader.i32_le()?;
        let length = reader.i32_le()?;
        if usize::try_from(length).ok() != Some(reader.remaining()) {
            return Err(StoreError::Malformed(
                "level.dat's length is not its header's",
            ));
        }
        let (_, compound) = Compound::read_le(&mut reader)?;
        reader.finish()?;
        Ok(Self(compound))
    }

    /// The file's bytes: the header, then the compound.
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.0.to_le_bytes();
        let length = i32::try_from(payload.len()).expect("level.dat is far smaller than 2 GiB");
        let storage_version = match self.0.get("StorageVersion") {
            Some(Tag::Int(version)) => *version,
            _ => STORAGE_VERSION,
        };
        let mut bytes = Vec::with_capacity(8 + payload.len());
        bytes.extend(storage_version.to_le_bytes());
        bytes.extend(length.to_le_bytes());
        bytes.extend(payload);
        bytes
    }

    /// Where players spawn, if vanilla (or the server) chose a spot inside
    /// the world's height.
    pub fn spawn(&self) -> Option<BlockPos> {
        let int = |name| match self.0.get(name) {
            Some(Tag::Int(value)) => Some(*value),
            _ => None,
        };
        let y = int("SpawnY")?;
        if y == SPAWN_NOT_CHOSEN || !(MIN_Y..=MAX_Y).contains(&y) {
            return None;
        }
        Some(BlockPos {
            x: int("SpawnX")?,
            y,
            z: int("SpawnZ")?,
        })
    }

    /// A true-or-false game rule, by its vanilla name, if the file has it.
    pub fn game_rule(&self, rule: &str) -> Option<bool> {
        match self.0.get(rule) {
            Some(Tag::Byte(value)) => Some(*value != 0),
            _ => None,
        }
    }

    pub fn set_game_rule(&mut self, rule: &str, value: bool) {
        self.0.set(rule, Tag::Byte(value.into()));
    }

    /// Updates what the server changes about a world it opens: the time it
    /// was last played, its default game mode, and the versions it was
    /// opened with, if they are older than the server's.
    pub fn opened(&mut self, game_type: i32) {
        self.0.set("LastPlayed", Tag::Long(now()));
        self.0.set("GameType", Tag::Int(game_type));
        let version = game_version();
        let older = match self.0.get("lastOpenedWithVersion") {
            Some(Tag::List(_, items)) => {
                let theirs: Vec<i32> = items
                    .iter()
                    .map(|item| match item {
                        Tag::Int(value) => *value,
                        _ => 0,
                    })
                    .collect();
                theirs.as_slice() < version.as_slice()
            }
            _ => true,
        };
        if older {
            let ints = version.iter().map(|value| Tag::Int(*value)).collect();
            self.0
                .set("lastOpenedWithVersion", Tag::List(TagKind::Int, ints));
        }
    }
}

/// The server's game version as `level.dat` lists versions: major, minor,
/// patch, revision, beta.
fn game_version() -> [i32; 5] {
    let mut version = [0; 5];
    for (slot, part) in version.iter_mut().zip(GAME_VERSION.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    version
}

/// Seconds since 1970, as `LastPlayed` holds them.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// A world's `level.dat`, kept in memory and written back whenever the
/// server changes it.
#[derive(Debug)]
pub struct LevelFile {
    path: PathBuf,
    level: Mutex<LevelDat>,
    /// The file did not exist and was created when the world was opened.
    created: bool,
}

impl LevelFile {
    /// The `level.dat` in the world folder `world`. A damaged one is read
    /// from `level.dat_old`, as vanilla does; a world without either gets
    /// `new_world` (written at once, so vanilla can open the world).
    pub fn open(world: &Path, new_world: impl FnOnce() -> LevelDat) -> Result<Self, StoreError> {
        let path = world.join(LEVEL_DAT);
        let read = |path: &Path| -> Result<Option<LevelDat>, StoreError> {
            match fs::read(path) {
                Ok(bytes) => LevelDat::decode(&bytes).map(Some),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err.into()),
            }
        };
        let (level, created) = match read(&path) {
            Ok(Some(level)) => (level, false),
            Ok(None) => (new_world(), true),
            Err(err) => match read(&world.join(LEVEL_DAT_OLD)) {
                Ok(Some(level)) => {
                    tracing::warn!(
                        "{} is damaged ({err}); using {LEVEL_DAT_OLD}",
                        path.display()
                    );
                    (level, false)
                }
                _ => return Err(err),
            },
        };
        let file = Self {
            path,
            level: Mutex::new(level),
            created,
        };
        if created {
            file.write(&file.lock())?;
        }
        Ok(file)
    }

    /// Whether the file was created as the world opened.
    pub fn created(&self) -> bool {
        self.created
    }

    /// A copy of what the file holds.
    pub fn get(&self) -> LevelDat {
        self.lock().clone()
    }

    /// Changes the file with `change`, and writes it.
    pub fn update(&self, change: impl FnOnce(&mut LevelDat)) -> Result<(), StoreError> {
        let mut level = self.lock();
        change(&mut level);
        self.write(&level)
    }

    /// Writes `level`: to a temporary file, renamed over `level.dat` once
    /// the old one is kept as `level.dat_old`, so a crash never leaves half a
    /// file. A damaged old one is not kept over a good backup.
    fn write(&self, level: &LevelDat) -> Result<(), StoreError> {
        let temporary = self.path.with_extension("dat.tmp");
        fs::write(&temporary, level.encode())?;
        let old_is_good = fs::read(&self.path)
            .ok()
            .is_some_and(|bytes| LevelDat::decode(&bytes).is_ok());
        if old_is_good {
            fs::copy(&self.path, self.path.with_file_name(LEVEL_DAT_OLD))?;
        }
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, LevelDat> {
        self.level.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::temporary_world;

    fn new_world() -> LevelDat {
        LevelDat::new_world(&NewWorld {
            name: "Test World",
            spawn: BlockPos { x: 0, y: -60, z: 0 },
            game_type: 1,
            game_rules: &[("keepinventory", true), ("showcoordinates", true)],
        })
    }

    #[test]
    fn new_worlds_say_what_vanilla_needs() {
        let level = new_world();
        let get = |name| level.0.get(name).cloned();
        assert_eq!(get("LevelName"), Some(Tag::String("Test World".into())));
        assert_eq!(get("Generator"), Some(Tag::Int(2)), "superflat");
        assert_eq!(get("WorldVersion"), Some(Tag::Int(1)), "post-1.18");
        assert_eq!(get("StorageVersion"), Some(Tag::Int(STORAGE_VERSION)));
        assert_eq!(get("NetworkVersion"), Some(Tag::Int(PROTOCOL_VERSION)));
        assert_eq!(get("GameType"), Some(Tag::Int(1)));
        assert_eq!(level.spawn(), Some(BlockPos { x: 0, y: -60, z: 0 }));
        assert_eq!(level.game_rule("keepinventory"), Some(true));
        assert_eq!(level.game_rule("showcoordinates"), Some(true));
        assert_eq!(
            level.game_rule("falldamage"),
            Some(true),
            "vanilla's default"
        );
        // The layers are JSON with the post-1.18 marker, ending in a newline.
        let Some(Tag::String(layers)) = get("FlatWorldLayers") else {
            panic!("no layers");
        };
        let json: serde_json::Value = serde_json::from_str(&layers).unwrap();
        assert_eq!(json["world_version"], "version.post_1_18");
        assert_eq!(json["encoding_version"], 6);
        assert_eq!(json["block_layers"][0]["block_name"], "minecraft:bedrock");
        assert_eq!(json["block_layers"][1]["count"], 2);
        assert!(layers.ends_with('\n'));
        // Its versions are the server's.
        let Some(Tag::List(TagKind::Int, version)) = get("lastOpenedWithVersion") else {
            panic!("no version");
        };
        assert_eq!(version.len(), 5);
        assert_eq!(version[0], Tag::Int(1));
    }

    #[test]
    fn the_file_is_a_header_then_the_compound() {
        let level = new_world();
        let bytes = level.encode();
        assert_eq!(bytes[..4], STORAGE_VERSION.to_le_bytes());
        assert_eq!(
            i32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        assert_eq!(LevelDat::decode(&bytes).unwrap(), level);
        // A length that is not the file's is damage.
        let mut short = bytes.clone();
        short.pop();
        assert!(LevelDat::decode(&short).is_err());
    }

    #[test]
    fn a_spawn_vanilla_has_not_chosen_is_none() {
        let mut level = new_world();
        level.0.set("SpawnY", Tag::Int(SPAWN_NOT_CHOSEN));
        assert_eq!(level.spawn(), None);
        level.0.set("SpawnY", Tag::Int(-1000));
        assert_eq!(level.spawn(), None, "outside the world");
    }

    #[test]
    fn opening_keeps_vanilla_fields_and_only_moves_versions_forward() {
        let mut level = new_world();
        level.0.set("SomethingVanilla", Tag::Int(7));
        level.0.set(
            "lastOpenedWithVersion",
            Tag::List(TagKind::Int, [9, 0, 0, 0, 0].map(Tag::Int).to_vec()),
        );
        level.opened(0);
        assert_eq!(level.0.get("SomethingVanilla"), Some(&Tag::Int(7)));
        assert_eq!(level.0.get("GameType"), Some(&Tag::Int(0)));
        assert_eq!(
            level.0.get("lastOpenedWithVersion"),
            Some(&Tag::List(
                TagKind::Int,
                [9, 0, 0, 0, 0].map(Tag::Int).to_vec()
            )),
            "a newer version stays"
        );
        level.0.set(
            "lastOpenedWithVersion",
            Tag::List(TagKind::Int, [1, 0, 0, 0, 0].map(Tag::Int).to_vec()),
        );
        level.opened(0);
        let ours = game_version().map(Tag::Int).to_vec();
        assert_eq!(
            level.0.get("lastOpenedWithVersion"),
            Some(&Tag::List(TagKind::Int, ours))
        );
    }

    #[test]
    fn files_are_created_kept_and_recovered_from_their_backup() {
        let world = temporary_world("level-dat");
        fs::create_dir_all(&world).unwrap();
        let file = LevelFile::open(&world, new_world).unwrap();
        assert!(file.created());
        assert!(world.join(LEVEL_DAT).is_file());
        file.update(|level| level.set_game_rule("falldamage", false))
            .unwrap();
        assert!(world.join(LEVEL_DAT_OLD).is_file(), "the file before");
        drop(file);

        let file = LevelFile::open(&world, || panic!("the file exists")).unwrap();
        assert!(!file.created());
        assert_eq!(file.get().game_rule("falldamage"), Some(false));
        drop(file);

        // A damaged level.dat falls back to level.dat_old.
        fs::write(world.join(LEVEL_DAT), b"damaged").unwrap();
        let file = LevelFile::open(&world, || panic!("there is a backup")).unwrap();
        assert_eq!(file.get().game_rule("falldamage"), Some(true));
        // Writing replaces the damaged file, and keeps the good backup.
        file.update(|level| level.set_game_rule("pvp", false))
            .unwrap();
        let backup = LevelDat::decode(&fs::read(world.join(LEVEL_DAT_OLD)).unwrap()).unwrap();
        assert_eq!(backup.game_rule("falldamage"), Some(true));
        drop(file);
        // With no backup either, it is an error, not a new world over it.
        fs::write(world.join(LEVEL_DAT), b"damaged").unwrap();
        fs::remove_file(world.join(LEVEL_DAT_OLD)).unwrap();
        assert!(LevelFile::open(&world, new_world).is_err());
        fs::remove_dir_all(&world).unwrap();
    }
}
