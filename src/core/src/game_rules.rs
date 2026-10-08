//! Vanilla game rules, saved in the world's `level.dat` as vanilla keeps
//! them, and changed with `/gamerule`.
//!
//! Only the rules the server acts on are here; vanilla's others follow as
//! what they govern is built. They stay in `level.dat` as they were.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bedrockrs_protocol::packets::{GameRule, GameRuleValue};
use serde::Deserialize;

use crate::storage::LevelFile;

/// Where worlds kept their game rules before `level.dat`, inside their
/// folder. Read once, into a `level.dat` made for a world that had one.
pub const OLD_GAME_RULES_FILE: &str = "game_rules.json";

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

/// The value of every rule. (`Deserialize` reads an old `game_rules.json`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
    /// Every rule with its vanilla name, as `level.dat` holds them.
    pub fn named(&self) -> Vec<(&'static str, bool)> {
        Rule::ALL
            .into_iter()
            .map(|rule| (rule.name(), self.get(rule)))
            .collect()
    }

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

/// A world's game rules, saved to its `level.dat` whenever one changes.
#[derive(Debug, Default)]
pub struct GameRules {
    /// `None` keeps them in memory only, as in tests.
    level: Option<Arc<LevelFile>>,
    values: Mutex<Values>,
}

impl GameRules {
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The rules in `level`, the `level.dat` of the world in `world`: the
    /// defaults for any it does not have. A `level.dat` made as the world
    /// opened takes the rules of the world's old `game_rules.json`, if any.
    pub fn of_level(level: Arc<LevelFile>, world: &Path) -> Self {
        let mut values = Values::default();
        if level.created() {
            let old = world.join(OLD_GAME_RULES_FILE);
            if let Ok(text) = fs::read_to_string(&old) {
                match serde_json::from_str::<Values>(&text) {
                    Ok(saved) => {
                        let moved = level.update(|level| {
                            for (rule, value) in saved.named() {
                                level.set_game_rule(rule, value);
                            }
                        });
                        match moved {
                            Ok(()) => tracing::info!(
                                "Moved the game rules from {} into level.dat; {OLD_GAME_RULES_FILE} is no longer read",
                                old.display()
                            ),
                            Err(err) => tracing::error!("Couldn't save the game rules: {err}"),
                        }
                    }
                    Err(err) => tracing::warn!("Ignored {}: {err}", old.display()),
                }
            }
        }
        let saved = level.get();
        for rule in Rule::ALL {
            if let Some(value) = saved.game_rule(rule.name()) {
                *values.slot(rule) = value;
            }
        }
        Self {
            level: Some(level),
            values: Mutex::new(values),
        }
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
        if let Some(level) = &self.level
            && let Err(err) = level.update(|level| level.set_game_rule(rule.name(), value))
        {
            tracing::error!("Couldn't save the game rules: {err}");
        }
        Some(*values)
    }

    fn lock(&self) -> MutexGuard<'_, Values> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use bedrockrs_protocol::types::BlockPos;

    use super::*;
    use crate::storage::tests::temporary_world;
    use crate::storage::{LevelDat, NewWorld};

    fn level(world: &Path) -> Arc<LevelFile> {
        let file = LevelFile::open(world, || {
            LevelDat::new_world(&NewWorld {
                name: "Rules",
                spawn: BlockPos::default(),
                game_type: 1,
                game_rules: &Values::default().named(),
            })
        })
        .unwrap();
        Arc::new(file)
    }

    #[test]
    fn rules_are_saved_in_level_dat() {
        let world = temporary_world("game-rules");
        fs::create_dir_all(&world).unwrap();

        let rules = GameRules::of_level(level(&world), &world);
        assert_eq!(rules.values(), Values::default());
        assert!(rules.set(Rule::KeepInventory, true).is_some());
        assert!(rules.set(Rule::KeepInventory, true).is_none(), "unchanged");
        drop(rules);
        let rules = GameRules::of_level(level(&world), &world);
        assert!(rules.values().keepinventory);
        // Where vanilla keeps it.
        let saved = LevelFile::open(&world, || unreachable!()).unwrap().get();
        assert_eq!(saved.game_rule("keepinventory"), Some(true));
        fs::remove_dir_all(&world).unwrap();
    }

    #[test]
    fn an_old_rules_file_moves_into_a_new_level_dat() {
        let world = temporary_world("game-rules-move");
        fs::create_dir_all(&world).unwrap();
        fs::write(world.join(OLD_GAME_RULES_FILE), r#"{"falldamage": false}"#).unwrap();
        let values = GameRules::of_level(level(&world), &world).values();
        assert!(!values.falldamage);
        assert!(values.naturalregeneration, "the rest keep their defaults");

        // Once level.dat exists, the old file is not read again.
        fs::write(
            world.join(OLD_GAME_RULES_FILE),
            r#"{"pvp": false, "keepinventory": true}"#,
        )
        .unwrap();
        assert!(
            !GameRules::of_level(level(&world), &world)
                .values()
                .keepinventory
        );
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
