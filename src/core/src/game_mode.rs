//! Game modes: what a player may do, and what their actions cost.

use bedrockrs_protocol::packets::ability;

/// A player's game mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}

impl GameMode {
    pub const ALL: [Self; 4] = [
        Self::Survival,
        Self::Creative,
        Self::Adventure,
        Self::Spectator,
    ];

    /// The mode's number on the wire (StartGame, AddPlayer, SetPlayerGameType).
    pub const fn id(self) -> i32 {
        match self {
            Self::Survival => 0,
            Self::Creative => 1,
            Self::Adventure => 2,
            Self::Spectator => 6,
        }
    }

    /// The mode's name, as saved and shown.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Survival => "survival",
            Self::Creative => "creative",
            Self::Adventure => "adventure",
            Self::Spectator => "spectator",
        }
    }

    /// The mode a name stands for: its full name, or the short forms vanilla
    /// accepts (`s`, `c`, `a`), in any case. Spectator has no short form.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "survival" | "s" => Some(Self::Survival),
            "creative" | "c" => Some(Self::Creative),
            "adventure" | "a" => Some(Self::Adventure),
            "spectator" => Some(Self::Spectator),
            _ => None,
        }
    }

    /// Like [`GameMode::from_name`], with vanilla's `default` and `d` for
    /// `default`, the server's default game mode.
    pub fn resolve(name: &str, default: Self) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "default" | "d" => Some(default),
            _ => Self::from_name(name),
        }
    }

    /// The mode a number stands for in vanilla `/gamemode`: 0, 1 or 2.
    pub fn from_number(number: i64) -> Option<Self> {
        match number {
            0 => Some(Self::Survival),
            1 => Some(Self::Creative),
            2 => Some(Self::Adventure),
            _ => None,
        }
    }

    /// Whether the player may take any item from the creative inventory,
    /// and places blocks without using them up.
    pub const fn is_creative(self) -> bool {
        matches!(self, Self::Creative)
    }

    /// Whether blocks the player breaks drop their item. Creative players
    /// break blocks without collecting them, as in vanilla.
    pub const fn drops_broken_blocks(self) -> bool {
        matches!(self, Self::Survival | Self::Adventure)
    }

    /// Whether the player may break and place blocks. Adventure players need
    /// items that allow it, which do not exist yet; spectators never may.
    pub const fn may_build(self) -> bool {
        matches!(self, Self::Survival | Self::Creative)
    }

    /// Whether the first hit breaks a block. Elsewhere the client says when
    /// it finished breaking.
    pub const fn breaks_instantly(self) -> bool {
        matches!(self, Self::Creative)
    }

    /// Whether the player may fly at will.
    pub const fn may_fly(self) -> bool {
        matches!(self, Self::Creative | Self::Spectator)
    }

    /// Whether the player picks up items and is seen by other players.
    pub const fn is_present(self) -> bool {
        !matches!(self, Self::Spectator)
    }

    /// The abilities the mode grants, as vanilla grants them. `flying` keeps
    /// a player who may fly in the air; spectators always fly.
    pub const fn abilities(self, flying: bool) -> u32 {
        let interact = ability::DOORS_AND_SWITCHES
            | ability::OPEN_CONTAINERS
            | ability::ATTACK_PLAYERS
            | ability::ATTACK_MOBS;
        let granted = match self {
            Self::Survival => interact | ability::BUILD | ability::MINE,
            Self::Creative => {
                interact
                    | ability::BUILD
                    | ability::MINE
                    | ability::INVULNERABLE
                    | ability::MAY_FLY
                    | ability::INSTANT_BUILD
            }
            Self::Adventure => interact,
            Self::Spectator => {
                ability::INVULNERABLE | ability::MAY_FLY | ability::FLYING | ability::NO_CLIP
            }
        };
        if flying && self.may_fly() {
            granted | ability::FLYING
        } else {
            granted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_survival_and_adventure_drop_broken_blocks() {
        assert!(GameMode::Survival.drops_broken_blocks());
        assert!(GameMode::Adventure.drops_broken_blocks());
        assert!(!GameMode::Creative.drops_broken_blocks());
        assert!(!GameMode::Spectator.drops_broken_blocks());
    }

    #[test]
    fn modes_use_the_protocol_numbers() {
        assert_eq!(GameMode::ALL.map(GameMode::id), [0, 1, 2, 6]);
    }

    #[test]
    fn modes_are_found_by_any_of_their_names() {
        for mode in GameMode::ALL {
            assert_eq!(GameMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(GameMode::from_name("C"), Some(GameMode::Creative));
        assert_eq!(GameMode::from_name("hardcore"), None);
        // As in vanilla: no short form for spectator, numbers only as numbers.
        assert_eq!(GameMode::from_name("sp"), None);
        assert_eq!(GameMode::from_name("0"), None);
        assert_eq!(GameMode::from_number(0), Some(GameMode::Survival));
        assert_eq!(GameMode::from_number(2), Some(GameMode::Adventure));
        assert_eq!(GameMode::from_number(6), None);
        assert_eq!(
            GameMode::resolve("D", GameMode::Adventure),
            Some(GameMode::Adventure)
        );
        // Every name commands and plugins accept means a mode.
        for name in bedrockrs_plugins::GAME_MODE_VALUES {
            assert!(
                GameMode::resolve(name, GameMode::Survival).is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn flying_needs_a_mode_that_may_fly() {
        assert_ne!(GameMode::Creative.abilities(true) & ability::FLYING, 0);
        assert_eq!(GameMode::Creative.abilities(false) & ability::FLYING, 0);
        assert_eq!(GameMode::Survival.abilities(true) & ability::FLYING, 0);
        assert_ne!(GameMode::Spectator.abilities(false) & ability::FLYING, 0);
        assert_eq!(
            GameMode::Adventure.abilities(false) & (ability::BUILD | ability::MINE),
            0
        );
    }
}
