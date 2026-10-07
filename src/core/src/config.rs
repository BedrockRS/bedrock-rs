//! The server's configuration file, `bedrockrs.toml`, created with the default
//! settings the first time the server starts.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Where the server looks for its configuration, relative to where it runs.
pub const CONFIG_FILE: &str = "bedrockrs.toml";

/// What a new configuration file holds: every setting, at its default, with
/// what it does.
pub const DEFAULT_CONFIG: &str = r#"# BedrockRS configuration.
# Settings left out keep their default. Restart the server after editing.

[logs]
# Show player chat, plugin broadcasts, private plugin messages and chat that
# plugins cancelled in the console.
chat = true
# Show routine internal activity: connections, logins, chunk streaming and
# saves, refused actions, and the libraries the server is built on.
system_noise = false
"#;

/// The server's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub logs: Logs,
}

/// What the console shows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logs {
    pub chat: bool,
    pub system_noise: bool,
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            chat: true,
            system_noise: false,
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
    /// The `tracing` filter these settings stand for, in `RUST_LOG` syntax.
    ///
    /// BedrockRS's own crates (whose targets all start with `bedrockrs`) log at
    /// info; routine activity is logged at debug, which `system_noise` shows,
    /// along with info from the libraries underneath. Chat has its own
    /// `chat` target.
    pub fn filter(&self) -> String {
        let (libraries, ours) = if self.system_noise {
            ("info", "debug")
        } else {
            ("warn", "info")
        };
        let chat = if self.chat { "info" } else { "off" };
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
        let config = Config::parse("[logs]\nsystem_noise = true").unwrap();
        assert!(config.logs.chat);
        assert!(config.logs.system_noise);
    }

    #[test]
    fn typos_are_errors() {
        let err = Config::parse("[logs]\nchats = false").unwrap_err();
        assert!(err.to_string().contains("chats"), "{err}");
        assert!(Config::parse("[logs]\nchat = \"yes\"").is_err());
    }

    #[test]
    fn log_settings_become_a_filter() {
        assert_eq!(
            Logs::default().filter(),
            "warn,bedrockrs=info,plugin=info,chat=info"
        );
        let quiet = Logs {
            chat: false,
            system_noise: true,
        };
        assert_eq!(quiet.filter(), "info,bedrockrs=debug,plugin=debug,chat=off");
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
