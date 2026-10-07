//! The server's configuration file, `bedrockrs.toml`, created with the default
//! settings the first time the server starts.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::game_mode::GameMode;

/// Where the server looks for its configuration, relative to where it runs.
pub const CONFIG_FILE: &str = "bedrockrs.toml";

/// What a new configuration file holds: every setting, at its default, with
/// what it does.
pub const DEFAULT_CONFIG: &str = r#"# BedrockRS configuration.
# Settings left out keep their default. Restart the server after editing.

[logs]
# How much the console shows: "error", "warn", "info", "debug" or "trace".
# "info" shows what happens on the server; "debug" adds routine activity and
# details for finding problems, such as when reporting a bug.
level = "info"
# Show player chat, plugin broadcasts, private plugin messages and chat that
# plugins cancelled in the console.
chat = true

[players]
# The game mode of players joining for the first time: survival, creative,
# adventure or spectator. Operators change anyone's with /gamemode.
default_game_mode = "creative"
"#;

/// The server's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub logs: Logs,
    pub players: Players,
}

/// Settings for players.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Players {
    #[serde(deserialize_with = "game_mode")]
    pub default_game_mode: GameMode,
}

impl Default for Players {
    fn default() -> Self {
        Self {
            default_game_mode: GameMode::Creative,
        }
    }
}

fn game_mode<'de, D: Deserializer<'de>>(deserializer: D) -> Result<GameMode, D::Error> {
    let name = String::deserialize(deserializer)?;
    GameMode::from_name(&name).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "unknown game mode {name:?}; expected survival, creative, adventure or spectator"
        ))
    })
}

/// What the console shows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logs {
    /// How much the console shows; see [`Logs::level`] when it is left out.
    #[serde(default, deserialize_with = "log_level")]
    pub level: Option<LogLevel>,
    pub chat: bool,
    /// Replaced by `level`; `true` still means `debug`.
    #[serde(default)]
    system_noise: Option<bool>,
}

/// How much the console shows, from least to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub const ALL: [Self; 5] = [
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

fn log_level<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<LogLevel>, D::Error> {
    let name = String::deserialize(deserializer)?;
    LogLevel::ALL
        .into_iter()
        .find(|level| level.name().eq_ignore_ascii_case(&name))
        .map(Some)
        .ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown log level {name:?}; expected error, warn, info, debug or trace"
            ))
        })
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            level: Some(LogLevel::Info),
            chat: true,
            system_noise: None,
        }
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to create {}: {source}", .path.display())]
    Create { path: PathBuf, source: io::Error },
    #[error("invalid {}: {source}", .path.display())]
    Invalid {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// A loaded configuration, and whether its file was just created.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub created: bool,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Reads the configuration at `path`, first writing the default one there
    /// if there is none.
    pub fn load_or_create(path: &Path) -> Result<Loaded, ConfigError> {
        let (text, created) = match fs::read_to_string(path) {
            Ok(text) => (text, false),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::write(path, DEFAULT_CONFIG).map_err(|source| ConfigError::Create {
                    path: path.to_owned(),
                    source,
                })?;
                (DEFAULT_CONFIG.to_owned(), true)
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let config = Self::parse(&text).map_err(|source| ConfigError::Invalid {
            path: path.to_owned(),
            source,
        })?;
        Ok(Loaded { config, created })
    }
}

impl Logs {
    /// How much the console shows: `level`, or without it `info` (`debug`
    /// for a file from before `level`, with `system_noise = true`).
    pub fn level(&self) -> LogLevel {
        match (self.level, self.system_noise) {
            (Some(level), _) => level,
            (None, Some(true)) => LogLevel::Debug,
            (None, _) => LogLevel::Info,
        }
    }

    /// Whether the file still uses `system_noise`, which `level` replaced.
    pub fn uses_system_noise(&self) -> bool {
        self.system_noise.is_some()
    }

    /// The `tracing` filter these settings stand for, in `RUST_LOG` syntax.
    ///
    /// BedrockRS's own crates (whose targets all start with `bedrockrs`) and
    /// plugins log at the level; the libraries underneath stay a step
    /// quieter, so debugging the server is not buried in theirs. Chat has its
    /// own `chat` target, at info.
    pub fn filter(&self) -> String {
        let ours = self.level();
        let libraries = match ours {
            LogLevel::Error => LogLevel::Error,
            LogLevel::Warn | LogLevel::Info => LogLevel::Warn,
            LogLevel::Debug => LogLevel::Info,
            LogLevel::Trace => LogLevel::Debug,
        };
        let chat = if self.chat && ours >= LogLevel::Info {
            "info"
        } else {
            "off"
        };
        let (ours, libraries) = (ours.name(), libraries.name());
        format!("{libraries},bedrockrs={ours},plugin={ours},chat={chat}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_file_holds_the_defaults() {
        assert_eq!(Config::parse(DEFAULT_CONFIG).unwrap(), Config::default());
    }

    #[test]
    fn missing_settings_keep_their_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let config = Config::parse("[logs]\nchat = false").unwrap();
        assert!(!config.logs.chat);
        assert_eq!(config.logs.level(), LogLevel::Info);
    }

    #[test]
    fn typos_are_errors() {
        let err = Config::parse("[logs]\nchats = false").unwrap_err();
        assert!(err.to_string().contains("chats"), "{err}");
        assert!(Config::parse("[logs]\nchat = \"yes\"").is_err());
    }

    #[test]
    fn game_modes_are_read_by_name() {
        let config = Config::parse("[players]\ndefault_game_mode = \"Survival\"").unwrap();
        assert_eq!(config.players.default_game_mode, GameMode::Survival);
        let err = Config::parse("[players]\ndefault_game_mode = \"hardcore\"").unwrap_err();
        assert!(err.to_string().contains("unknown game mode"), "{err}");
    }

    #[test]
    fn log_settings_become_a_filter() {
        assert_eq!(
            Logs::default().filter(),
            "warn,bedrockrs=info,plugin=info,chat=info"
        );
        let debug = Config::parse("[logs]\nlevel = \"DEBUG\"\nchat = false").unwrap();
        assert_eq!(
            debug.logs.filter(),
            "info,bedrockrs=debug,plugin=debug,chat=off"
        );
        let quiet = Config::parse("[logs]\nlevel = \"warn\"").unwrap();
        assert_eq!(
            quiet.logs.filter(),
            "warn,bedrockrs=warn,plugin=warn,chat=off",
            "chat is info, so it goes quiet too"
        );
        let err = Config::parse("[logs]\nlevel = \"loud\"").unwrap_err();
        assert!(err.to_string().contains("unknown log level"), "{err}");
    }

    #[test]
    fn system_noise_still_means_debug() {
        let old = Config::parse("[logs]\nsystem_noise = true").unwrap();
        assert_eq!(old.logs.level(), LogLevel::Debug);
        assert!(old.logs.uses_system_noise());
        let off = Config::parse("[logs]\nsystem_noise = false").unwrap();
        assert_eq!(off.logs.level(), LogLevel::Info);
        // An explicit level wins.
        let both = Config::parse("[logs]\nsystem_noise = true\nlevel = \"warn\"").unwrap();
        assert_eq!(both.logs.level(), LogLevel::Warn);
        assert!(!Config::default().logs.uses_system_noise());
    }

    #[test]
    fn a_missing_file_is_created_with_the_defaults() {
        let directory =
            std::env::temp_dir().join(format!("bedrockrs-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(CONFIG_FILE);

        let loaded = Config::load_or_create(&path).unwrap();
        assert!(loaded.created);
        assert_eq!(loaded.config, Config::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);

        fs::write(&path, "[logs]\nchat = false\n").unwrap();
        let loaded = Config::load_or_create(&path).unwrap();
        assert!(!loaded.created);
        assert!(!loaded.config.logs.chat);

        fs::write(&path, "[logs\n").unwrap();
        assert!(matches!(
            Config::load_or_create(&path),
            Err(ConfigError::Invalid { .. })
        ));
        fs::remove_dir_all(&directory).unwrap();
    }
}
