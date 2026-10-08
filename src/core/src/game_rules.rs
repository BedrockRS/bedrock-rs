//! Vanilla game rules, saved with the world and changed with `/gamerule`.
//!
//! Only the rules the server acts on are here; vanilla's others follow as
//! what they govern is built.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use bedrockrs_protocol::packets::{GameRule, GameRuleValue};
use serde::{Deserialize, Serialize};

/// Where a world keeps its game rules, inside its directory.
pub const GAME_RULES_FILE: &str = "game_rules.json";

/// A true-or-false game rule, by its vanilla name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    DoTileDrops,
    FallDamage,
    KeepInventory,
    NaturalRegeneration,
    ShowCoordinates,
    ShowDeathMessages,
}

impl Rule {
    pub const ALL: [Self; 6] = [
        Self::DoTileDrops,
        Self::FallDamage,
        Self::KeepInventory,
        Self::NaturalRegeneration,
        Self::ShowCoordinates,
        Self::ShowDeathMessages,
    ];

    /// The rule's vanilla name, as `/gamerule` takes it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::DoTileDrops => "dotiledrops",
            Self::FallDamage => "falldamage",
            Self::KeepInventory => "keepinventory",
            Self::NaturalRegeneration => "naturalregeneration",
            Self::ShowCoordinates => "showcoordinates",
            Self::ShowDeathMessages => "showdeathmessages",
        }
    }

    /// The rule named `name`, in any case.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|rule| rule.name().eq_ignore_ascii_case(name))
    }
}

/// The value of every rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Values {
    /// Whether broken blocks drop their item, and blocks that lose their
    /// support pop off as one.
    pub dotiledrops: bool,
    pub falldamage: bool,
    pub keepinventory: bool,
    pub naturalregeneration: bool,
    /// Vanilla's default is off; the server has always shown coordinates.
    pub showcoordinates: bool,
    pub showdeathmessages: bool,
}

impl Default for Values {
    fn default() -> Self {
        Self {
            dotiledrops: true,
            falldamage: true,
            keepinventory: false,
            naturalregeneration: true,
            showcoordinates: true,
            showdeathmessages: true,
        }
    }
}

impl Values {
    pub fn get(&self, rule: Rule) -> bool {
        match rule {
            Rule::DoTileDrops => self.dotiledrops,
            Rule::FallDamage => self.falldamage,
            Rule::KeepInventory => self.keepinventory,
            Rule::NaturalRegeneration => self.naturalregeneration,
            Rule::ShowCoordinates => self.showcoordinates,
            Rule::ShowDeathMessages => self.showdeathmessages,
        }
    }

    fn slot(&mut self, rule: Rule) -> &mut bool {
        match rule {
            Rule::DoTileDrops => &mut self.dotiledrops,
            Rule::FallDamage => &mut self.falldamage,
            Rule::KeepInventory => &mut self.keepinventory,
            Rule::NaturalRegeneration => &mut self.naturalregeneration,
            Rule::ShowCoordinates => &mut self.showcoordinates,
            Rule::ShowDeathMessages => &mut self.showdeathmessages,
        }
    }

    /// The rules as clients are told them, in StartGame and GameRulesChanged.
    pub fn packet_rules(&self) -> Vec<GameRule> {
        Rule::ALL
            .into_iter()
            .map(|rule| GameRule {
                name: rule.name().into(),
                editable: false,
                value: GameRuleValue::Bool(self.get(rule)),
            })
            .collect()
    }
}

/// Why the game rules could not be read.
#[derive(Debug, thiserror::Error)]
pub enum GameRulesError {
    #[error("failed to read {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid {}: {source}", .path.display())]
    Invalid {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// A world's game rules, saved to its directory whenever one changes.
#[derive(Debug, Default)]
pub struct GameRules {
    /// `None` keeps them in memory only, as in tests.
    path: Option<PathBuf>,
    values: Mutex<Values>,
}

impl GameRules {
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The rules saved in the world directory `world`; the defaults if none
    /// are saved yet.
    pub fn open(world: &Path) -> Result<Self, GameRulesError> {
        let path = world.join(GAME_RULES_FILE);
        let values = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| GameRulesError::Invalid {
                path: path.clone(),
                source,
            })?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Values::default(),
            Err(source) => return Err(GameRulesError::Read { path, source }),
        };
        Ok(Self {
            path: Some(path),
            values: Mutex::new(values),
        })
    }

    pub fn values(&self) -> Values {
        *self.lock()
    }

    /// Sets a rule. Returns the rules as they are now if it changed.
    pub fn set(&self, rule: Rule, value: bool) -> Option<Values> {
        let mut values = self.lock();
        if values.get(rule) == value {
            return None;
        }
        *values.slot(rule) = value;
        if let Some(path) = &self.path {
            let text = serde_json::to_string_pretty(&*values).expect("game rules serialize");
            if let Err(err) = fs::write(path, text + "\n") {
                tracing::error!("Couldn't save the game rules to {}: {err}", path.display());
            }
        }
        Some(*values)
    }

    fn lock(&self) -> MutexGuard<'_, Values> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_saved_with_the_world() {
        let world =
            std::env::temp_dir().join(format!("bedrockrs-game-rules-{}", std::process::id()));
        let _ = fs::remove_dir_all(&world);
        fs::create_dir_all(&world).unwrap();

        let rules = GameRules::open(&world).unwrap();
        assert_eq!(rules.values(), Values::default());
        assert!(rules.set(Rule::KeepInventory, true).is_some());
        assert!(rules.set(Rule::KeepInventory, true).is_none(), "unchanged");
        assert!(GameRules::open(&world).unwrap().values().keepinventory);

        // Rules missing from the file keep their defaults.
        fs::write(world.join(GAME_RULES_FILE), r#"{"falldamage": false}"#).unwrap();
        let values = GameRules::open(&world).unwrap().values();
        assert!(!values.falldamage);
        assert!(values.naturalregeneration);
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn rules_go_by_their_vanilla_names() {
        assert_eq!(Rule::from_name("KeepInventory"), Some(Rule::KeepInventory));
        assert_eq!(Rule::from_name("dofiretick"), None);
        let rules = Values::default().packet_rules();
        assert_eq!(rules.len(), Rule::ALL.len());
        assert_eq!(rules[0].name, "dotiledrops");
        assert_eq!(rules[0].value, GameRuleValue::Bool(true));
        assert_eq!(rules[2].name, "keepinventory");
        assert_eq!(rules[2].value, GameRuleValue::Bool(false));
    }
}
