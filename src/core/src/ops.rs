//! Operators: players who may run operator commands, such as `/gamemode`.
//!
//! They are listed by UUID in `ops.json`, next to the configuration, with
//! their name when they were made operator for whoever reads the file. The
//! console makes the first operator with `op <player>`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where the server keeps its operators, relative to where it runs.
pub const OPS_FILE: &str = "ops.json";

/// Why the operator list could not be read.
#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error("failed to read {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid {}: {source}", .path.display())]
    Invalid {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    uuid: Uuid,
    name: String,
}

/// The operators, saved to their file whenever they change.
#[derive(Debug, Default)]
pub struct Operators {
    /// `None` keeps them in memory only, as in tests.
    path: Option<PathBuf>,
    by_uuid: Mutex<BTreeMap<Uuid, String>>,
}

impl Operators {
    /// Operators kept in memory only.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The operators saved at `path`; none if the file does not exist yet.
    pub fn open(path: &Path) -> Result<Self, OpsError> {
        let entries: Vec<Entry> = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| OpsError::Invalid {
                path: path.to_owned(),
                source,
            })?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(OpsError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Ok(Self {
            path: Some(path.to_owned()),
            by_uuid: Mutex::new(
                entries
                    .into_iter()
                    .map(|entry| (entry.uuid, entry.name))
                    .collect(),
            ),
        })
    }

    pub fn contains(&self, uuid: Uuid) -> bool {
        self.by_uuid().contains_key(&uuid)
    }

    /// Makes a player an operator. Returns whether they were not one already.
    pub fn add(&self, uuid: Uuid, name: &str) -> bool {
        let mut by_uuid = self.by_uuid();
        if by_uuid.contains_key(&uuid) {
            return false;
        }
        by_uuid.insert(uuid, name.to_owned());
        self.save(&by_uuid);
        true
    }

    /// Takes away a player's operator status. Returns whether they had it.
    pub fn remove(&self, uuid: Uuid) -> bool {
        let mut by_uuid = self.by_uuid();
        if by_uuid.remove(&uuid).is_none() {
            return false;
        }
        self.save(&by_uuid);
        true
    }

    /// Writes the list, logging a failure: the change still holds until the
    /// server stops.
    fn save(&self, by_uuid: &BTreeMap<Uuid, String>) {
        let Some(path) = &self.path else {
            return;
        };
        let entries: Vec<Entry> = by_uuid
            .iter()
            .map(|(uuid, name)| Entry {
                uuid: *uuid,
                name: name.clone(),
            })
            .collect();
        let text = serde_json::to_string_pretty(&entries).expect("operators serialize");
        if let Err(err) = fs::write(path, text + "\n") {
            tracing::error!(path = %path.display(), %err, "failed to save the operators");
        }
    }

    fn by_uuid(&self) -> MutexGuard<'_, BTreeMap<Uuid, String>> {
        self.by_uuid.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_are_saved_and_read_back() {
        let directory = std::env::temp_dir().join(format!("bedrockrs-ops-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(OPS_FILE);

        let ops = Operators::open(&path).unwrap();
        let steve = Uuid::new_v4();
        assert!(!ops.contains(steve));
        assert!(ops.add(steve, "Steve"));
        assert!(!ops.add(steve, "Steve"), "already an operator");

        let reopened = Operators::open(&path).unwrap();
        assert!(reopened.contains(steve));
        assert!(reopened.remove(steve));
        assert!(!reopened.remove(steve));
        assert!(!Operators::open(&path).unwrap().contains(steve));

        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            Operators::open(&path),
            Err(OpsError::Invalid { .. })
        ));
        fs::remove_dir_all(&directory).unwrap();
    }
}
