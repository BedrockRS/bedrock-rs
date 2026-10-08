//! Player permissions, in vanilla's `permissions.json`: who is an operator,
//! a member or a visitor.
//!
//! The file is a list of entries, each a permission and the player it is
//! for:
//!
//! ```json
//! [
//!   { "permission": "operator", "pfid": "4A1B2C3D5E6F7A8B" }
//! ]
//! ```
//!
//! Vanilla's Bedrock Dedicated Server names players by XUID. BedrockRS names
//! them by PlayFab ID (`pfid`), the persistent ID Mojang wants servers to use
//! rather than the XUID, which a signed-in player's verified login carries
//! (the multiplayer token's `mid` claim). Entries with an `xuid` instead, as
//! in a vanilla file copied in, work too.
//!
//! - **operator**: everything, and operator commands;
//! - **member**: builds, mines, opens doors and fights;
//! - **visitor**: only looks around.
//!
//! Players without an entry get `default-player-permission-level` from
//! `server.properties` (member unless set). `op` adds an entry and `deop`
//! removes it; nothing else writes the file, so entries written by hand (a
//! visitor, say) stay as they are.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

/// Where the server keeps permissions, relative to where it runs.
pub const PERMISSIONS_FILE: &str = "permissions.json";

/// Where earlier versions kept their operators, by UUID; no longer read.
pub const OLD_OPS_FILE: &str = "ops.json";

/// What a player may do, as vanilla's permission levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Visitor,
    Member,
    Operator,
}

impl Permission {
    pub const ALL: [Self; 3] = [Self::Visitor, Self::Member, Self::Operator];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Visitor => "visitor",
            Self::Member => "member",
            Self::Operator => "operator",
        }
    }

    /// The level by name, in any case.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|permission| permission.name().eq_ignore_ascii_case(name))
    }

    /// The level's number in packets: 0 visitor, 1 member, 2 operator.
    pub const fn id(self) -> u8 {
        self as u8
    }

    pub const fn is_operator(self) -> bool {
        matches!(self, Self::Operator)
    }

    /// Whether the player may change the world: break and place blocks, use
    /// doors and switches, open containers and attack. Visitors may not.
    pub const fn may_build(self) -> bool {
        !matches!(self, Self::Visitor)
    }
}

/// What names a player in the permissions, from their verified login.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerIds {
    /// The PlayFab ID, which entries are written with.
    pub pfid: Option<String>,
    /// The XUID, for entries from a vanilla file.
    pub xuid: Option<String>,
}

/// Why the permissions could not be read or changed.
#[derive(Debug, thiserror::Error)]
pub enum PermissionsError {
    #[error("failed to read {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to create {}: {source}", .path.display())]
    Create { path: PathBuf, source: io::Error },
    #[error("invalid {}: {source}", .path.display())]
    Invalid {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{0} has no PlayFab ID (they did not sign in), so their permission can't be saved")]
    NoPlayFabId(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    permission: Permission,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pfid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    xuid: Option<String>,
}

impl Entry {
    fn is_for(&self, ids: &PlayerIds) -> bool {
        let same = |entry: &Option<String>, id: &Option<String>| matches!((entry, id), (Some(entry), Some(id)) if entry.eq_ignore_ascii_case(id));
        same(&self.pfid, &ids.pfid) || same(&self.xuid, &ids.xuid)
    }
}

/// The permissions, saved to their file whenever they change.
#[derive(Debug)]
pub struct Permissions {
    /// `None` keeps them in memory only, as in tests.
    path: Option<PathBuf>,
    /// The level of players without an entry.
    default: Permission,
    /// In file order.
    entries: Mutex<Vec<Entry>>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            path: None,
            default: Permission::Member,
            entries: Mutex::default(),
        }
    }
}

impl Permissions {
    /// Permissions kept in memory only.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The permissions saved at `path`. Without a file, an empty one is
    /// written there, as vanilla ships it, ready to edit.
    pub fn open(path: &Path) -> Result<Self, PermissionsError> {
        let entries: Vec<Entry> = match fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Vec::new(),
            Ok(text) => {
                serde_json::from_str(&text).map_err(|source| PermissionsError::Invalid {
                    path: path.to_owned(),
                    source,
                })?
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::write(
                    path, "[]
",
                )
                .map_err(|source| PermissionsError::Create {
                    path: path.to_owned(),
                    source,
                })?;
                Vec::new()
            }
            Err(source) => {
                return Err(PermissionsError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let unnamed = entries
            .iter()
            .filter(|entry| entry.pfid.is_none() && entry.xuid.is_none())
            .count();
        if unnamed > 0 {
            tracing::warn!(
                "{unnamed} entries in {} name no player (no \"pfid\" or \"xuid\"), so they do nothing",
                path.display()
            );
        }
        Ok(Self {
            path: Some(path.to_owned()),
            entries: Mutex::new(entries),
            ..Self::default()
        })
    }

    /// Gives players without an entry `default` rather than member.
    pub fn with_default(mut self, default: Permission) -> Self {
        self.default = default;
        self
    }

    /// What the player may do: their entry's level, or the default.
    pub fn of(&self, ids: &PlayerIds) -> Permission {
        self.entries()
            .iter()
            .find(|entry| entry.is_for(ids))
            .map_or(self.default, |entry| entry.permission)
    }

    /// The level of players without an entry, which a player gets back when
    /// they stop being an operator.
    pub fn default_level(&self) -> Permission {
        self.default
    }

    /// Makes the player named `name` an operator. Returns whether they were
    /// not one already. Their entry becomes an operator's; without one, an
    /// entry is added under their PlayFab ID, which they need to have.
    pub fn op(&self, ids: &PlayerIds, name: &str) -> Result<bool, PermissionsError> {
        let mut entries = self.entries();
        match entries.iter_mut().find(|entry| entry.is_for(ids)) {
            Some(entry) if entry.permission.is_operator() => return Ok(false),
            Some(entry) => entry.permission = Permission::Operator,
            None => {
                let pfid = ids
                    .pfid
                    .clone()
                    .ok_or_else(|| PermissionsError::NoPlayFabId(name.to_owned()))?;
                entries.push(Entry {
                    permission: Permission::Operator,
                    pfid: Some(pfid),
                    xuid: None,
                });
            }
        }
        self.save(&entries);
        Ok(true)
    }

    /// Takes away the player's operator status by removing their entry, so
    /// they get the default level. Returns whether they were an operator.
    pub fn deop(&self, ids: &PlayerIds) -> bool {
        let mut entries = self.entries();
        let before = entries.len();
        entries.retain(|entry| !(entry.is_for(ids) && entry.permission.is_operator()));
        if entries.len() == before {
            return false;
        }
        self.save(&entries);
        true
    }

    /// Writes the list, logging a failure: the change still holds until the
    /// server stops.
    fn save(&self, entries: &[Entry]) {
        let Some(path) = &self.path else {
            return;
        };
        let text = serde_json::to_string_pretty(entries).expect("permissions serialize");
        if let Err(err) = fs::write(path, text + "\n") {
            tracing::error!("Couldn't save the permissions to {}: {err}", path.display());
        }
    }

    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(test: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "bedrockrs-permissions-{test}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn player(pfid: &str, xuid: &str) -> PlayerIds {
        PlayerIds {
            pfid: Some(pfid.into()),
            xuid: Some(xuid.into()),
        }
    }

    #[test]
    fn operators_are_saved_by_playfab_id_and_read_back() {
        let directory = directory("saved");
        let path = directory.join(PERMISSIONS_FILE);
        let steve = player("4A1B2C3D5E6F7A8B", "2535400000000000");

        let permissions = Permissions::open(&path).unwrap();
        assert_eq!(permissions.of(&steve), Permission::Member);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[]
",
            "created empty"
        );
        assert!(permissions.op(&steve, "Steve").unwrap());
        assert!(
            !permissions.op(&steve, "Steve").unwrap(),
            "already an operator"
        );
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!([{ "permission": "operator", "pfid": "4A1B2C3D5E6F7A8B" }])
        );

        let reopened = Permissions::open(&path).unwrap();
        assert_eq!(reopened.of(&steve), Permission::Operator);
        assert!(reopened.deop(&steve));
        assert!(!reopened.deop(&steve));
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "[]");
        assert_eq!(
            Permissions::open(&path).unwrap().of(&steve),
            Permission::Member
        );

        fs::write(&path, r#"[{ "permission": "admin", "pfid": "X" }]"#).unwrap();
        assert!(matches!(
            Permissions::open(&path),
            Err(PermissionsError::Invalid { .. })
        ));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn vanilla_entries_by_xuid_and_hand_written_levels_apply() {
        let directory = directory("vanilla");
        let path = directory.join(PERMISSIONS_FILE);
        fs::write(
            &path,
            r#"[
                { "permission": "operator", "xuid": "2535400000000001" },
                { "permission": "visitor", "pfid": "aaaa0000bbbb1111" }
            ]"#,
        )
        .unwrap();
        let permissions = Permissions::open(&path)
            .unwrap()
            .with_default(Permission::Visitor);
        let admin = player("1111", "2535400000000001");
        let guest = player("AAAA0000BBBB1111", "2535400000000002");
        let stranger = player("2222", "2535400000000003");
        assert_eq!(permissions.of(&admin), Permission::Operator);
        assert_eq!(permissions.of(&guest), Permission::Visitor);
        assert_eq!(
            permissions.of(&stranger),
            Permission::Visitor,
            "the default"
        );

        // Opping a visitor changes their entry rather than adding one.
        assert!(permissions.op(&guest, "Guest").unwrap());
        assert_eq!(permissions.of(&guest), Permission::Operator);
        // Deopping removes the entry: back to the default.
        assert!(permissions.deop(&admin));
        assert_eq!(permissions.of(&admin), Permission::Visitor);
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!([{ "permission": "operator", "pfid": "aaaa0000bbbb1111" }])
        );

        // Without a PlayFab ID there is nothing to write down.
        let offline = PlayerIds::default();
        assert!(matches!(
            permissions.op(&offline, "Offline"),
            Err(PermissionsError::NoPlayFabId(_))
        ));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn levels_have_vanilla_names_and_numbers() {
        assert_eq!(
            Permission::from_name("OPERATOR"),
            Some(Permission::Operator)
        );
        assert_eq!(Permission::from_name("admin"), None);
        assert_eq!(Permission::Visitor.id(), 0);
        assert_eq!(Permission::Member.id(), 1);
        assert_eq!(Permission::Operator.id(), 2);
        assert!(!Permission::Visitor.may_build());
        assert!(Permission::Member.may_build());
    }
}
