//! The server's settings: `server.properties`, the file vanilla's Bedrock
//! Dedicated Server reads, created with the defaults the first time the
//! server starts. A vanilla `server.properties` works as it is: properties
//! BedrockRS does not use yet are ignored.
//!
//! The format is Java's `.properties`: `key=value` lines (`:` or a space
//! also separate them), `#` or `!` comments, a backslash at the end of a
//! line continuing it, and `\t`, `\n`, `\uXXXX` and the like as escapes. A
//! key given twice takes its last value.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::game_mode::GameMode;
use crate::world::DEFAULT_NAME;

/// Where the server looks for its settings, relative to where it runs.
pub const PROPERTIES_FILE: &str = "server.properties";

/// Where earlier versions kept their settings; no longer read.
pub const OLD_CONFIG_FILE: &str = "bedrockrs.toml";

/// The folder worlds are in, relative to where the server runs: the world
/// is `worlds/<level-name>/`, as in vanilla.
pub const WORLDS_DIR: &str = "worlds";

/// What a new `server.properties` holds: every property, at its default,
/// with what it does, in vanilla's style.
pub const DEFAULT_PROPERTIES: &str = r#"# BedrockRS server properties, read as the server starts: restart it after editing.
# This is vanilla's format (Bedrock Dedicated Server), so a vanilla server.properties
# works here too; properties BedrockRS does not use yet are ignored.

level-name=Bedrock level
# The world folder in worlds/ to load, created if it does not exist. Copy a vanilla
# world folder (or an unzipped .mcworld) into worlds/ and name it here to play it.
# The name players see comes from the world's own levelname.txt.

level-type=FLAT
# How new chunks are generated.
# Allowed values: "FLAT" (vanilla's default superflat layers). "DEFAULT" and
# "LEGACY" are not supported yet.

gamemode=creative
# The game mode of players joining for the first time. Operators change anyone's
# with /gamemode.
# Allowed values: "survival", "creative", "adventure" or "spectator" (or 0, 1, 2).

log-level=info
# BedrockRS only. How much the console shows.
# Allowed values: "error", "warn", "info", "debug" or "trace". "info" shows what
# happens on the server; "debug" adds routine activity and details for finding
# problems, such as when reporting a bug.

log-chat=true
# BedrockRS only. Show player chat, plugin broadcasts, private plugin messages and
# chat that plugins cancelled in the console.
# Allowed values: "true" or "false".
"#;

/// The server's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProperties {
    /// `level-name`: the world folder, in [`WORLDS_DIR`].
    pub level_name: String,
    /// `level-type`: how new chunks are generated.
    pub level_type: LevelType,
    /// `gamemode`: the game mode of players joining for the first time.
    pub default_game_mode: GameMode,
    /// `log-level` and `log-chat`.
    pub logs: Logs,
    /// Properties in the file that the server does not use, in file order.
    pub ignored: Vec<String>,
}

impl Default for ServerProperties {
    fn default() -> Self {
        Self {
            level_name: DEFAULT_NAME.to_owned(),
            level_type: LevelType::Flat,
            default_game_mode: GameMode::Creative,
            logs: Logs::default(),
            ignored: Vec::new(),
        }
    }
}

/// How new chunks are generated: only vanilla's superflat world so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelType {
    Flat,
}

/// What the console shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Logs {
    pub level: LogLevel,
    pub chat: bool,
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            chat: true,
        }
    }
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

impl Logs {
    /// The `tracing` filter these settings stand for, in `RUST_LOG` syntax.
    ///
    /// BedrockRS's own crates (whose targets all start with `bedrockrs`) and
    /// plugins log at the level; the libraries underneath stay a step
    /// quieter, so debugging the server is not buried in theirs. Chat has its
    /// own `chat` target, at info.
    pub fn filter(&self) -> String {
        let ours = self.level;
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

/// Why the settings could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to create {}: {source}", .path.display())]
    Create { path: PathBuf, source: io::Error },
    #[error("invalid {}: {source}", .path.display())]
    Invalid {
        path: PathBuf,
        source: PropertyError,
    },
}

/// A property with a value the server cannot use.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct PropertyError {
    pub line: usize,
    pub message: String,
}

/// Loaded settings, and whether their file was just created.
#[derive(Debug)]
pub struct Loaded {
    pub properties: ServerProperties,
    pub created: bool,
}

impl ServerProperties {
    pub fn parse(text: &str) -> Result<Self, PropertyError> {
        let mut properties = Self::default();
        for Property { line, key, value } in parse_properties(text) {
            let invalid = |message: String| PropertyError { line, message };
            let value = value.trim();
            match key.as_str() {
                "level-name" => properties.level_name = level_name(value).map_err(invalid)?,
                "level-type" => properties.level_type = level_type(value).map_err(invalid)?,
                "gamemode" => {
                    properties.default_game_mode = game_mode(value).map_err(invalid)?;
                }
                "log-level" => properties.logs.level = log_level(value).map_err(invalid)?,
                "log-chat" => properties.logs.chat = boolean(&key, value).map_err(invalid)?,
                _ => {
                    if !properties.ignored.contains(&key) {
                        properties.ignored.push(key);
                    }
                }
            }
        }
        Ok(properties)
    }

    /// Reads the settings at `path`, first writing the defaults there if
    /// there is no file.
    pub fn load_or_create(path: &Path) -> Result<Loaded, ConfigError> {
        let (text, created) = match fs::read_to_string(path) {
            Ok(text) => (text, false),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::write(path, DEFAULT_PROPERTIES).map_err(|source| ConfigError::Create {
                    path: path.to_owned(),
                    source,
                })?;
                (DEFAULT_PROPERTIES.to_owned(), true)
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let properties = Self::parse(&text).map_err(|source| ConfigError::Invalid {
            path: path.to_owned(),
            source,
        })?;
        Ok(Loaded {
            properties,
            created,
        })
    }

    /// The folder of the world to load: `worlds/<level-name>`.
    pub fn world_directory(&self) -> PathBuf {
        Path::new(WORLDS_DIR).join(&self.level_name)
    }
}

/// A world folder's name: one folder inside `worlds/`, never a path out of it.
fn level_name(value: &str) -> Result<String, String> {
    const FORBIDDEN: &[char] = &['/', '\\', '<', '>', ':', '"', '|', '?', '*'];
    if value.is_empty() {
        return Err("level-name is empty".into());
    }
    if value == "." || value == ".." || value.ends_with('.') {
        return Err(format!("level-name {value:?} is not a folder name"));
    }
    if let Some(bad) = value
        .chars()
        .find(|c| FORBIDDEN.contains(c) || c.is_control())
    {
        return Err(format!(
            "level-name {value:?} has {bad:?}, which folder names cannot"
        ));
    }
    Ok(value.to_owned())
}

fn level_type(value: &str) -> Result<LevelType, String> {
    match value.to_ascii_uppercase().as_str() {
        "FLAT" => Ok(LevelType::Flat),
        "DEFAULT" | "LEGACY" => Err(format!(
            "level-type={value} is not supported yet: BedrockRS only generates FLAT worlds"
        )),
        _ => Err(format!(
            "unknown level-type {value:?}; expected FLAT, DEFAULT or LEGACY"
        )),
    }
}

fn game_mode(value: &str) -> Result<GameMode, String> {
    let by_number = value.parse().ok().and_then(GameMode::from_number);
    by_number
        .or_else(|| GameMode::from_name(value))
        .ok_or_else(|| {
            format!(
                "unknown gamemode {value:?}; expected survival, creative, adventure or spectator"
            )
        })
}

fn log_level(value: &str) -> Result<LogLevel, String> {
    LogLevel::ALL
        .into_iter()
        .find(|level| level.name().eq_ignore_ascii_case(value))
        .ok_or_else(|| {
            format!("unknown log-level {value:?}; expected error, warn, info, debug or trace")
        })
}

fn boolean(key: &str, value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key} is {value:?}; expected true or false")),
    }
}

/// One `key=value` from a properties file, with the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Property {
    line: usize,
    key: String,
    value: String,
}

/// The properties in `text`, in order, as Java reads `.properties` files.
fn parse_properties(text: &str) -> Vec<Property> {
    let mut properties = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((index, first)) = lines.next() {
        let first = first.trim_start_matches('\u{feff}').trim_start();
        if first.is_empty() || first.starts_with('#') || first.starts_with('!') {
            continue;
        }
        // A line ending in an odd number of backslashes goes on to the next.
        let mut logical = String::new();
        let mut part = first;
        loop {
            let trailing = part.len() - part.trim_end_matches('\\').len();
            if trailing % 2 == 0 {
                logical.push_str(part);
                break;
            }
            logical.push_str(&part[..part.len() - 1]);
            match lines.next() {
                Some((_, next)) => part = next.trim_start(),
                None => break,
            }
        }
        let (key, value) = split_property(&logical);
        properties.push(Property {
            line: index + 1,
            key: unescape(key),
            value: unescape(value),
        });
    }
    properties
}

/// A logical line's key and value: the key ends at the first unescaped
/// `=`, `:` or whitespace, which with whitespace around it separates them.
fn split_property(line: &str) -> (&str, &str) {
    let mut escaped = false;
    let mut end = line.len();
    for (at, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '=' || c == ':' || c.is_whitespace() {
            end = at;
            break;
        }
    }
    let key = &line[..end];
    let mut rest = line[end..].trim_start();
    if let Some(after) = rest.strip_prefix(['=', ':']) {
        rest = after.trim_start();
    }
    (key, rest)
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push_str(&hex),
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ServerProperties {
        ServerProperties::parse(text).unwrap()
    }

    #[test]
    fn the_default_file_holds_the_defaults() {
        assert_eq!(parse(DEFAULT_PROPERTIES), ServerProperties::default());
        assert_eq!(
            ServerProperties::default().world_directory(),
            Path::new("worlds").join("Bedrock level")
        );
    }

    #[test]
    fn missing_properties_keep_their_defaults() {
        assert_eq!(parse(""), ServerProperties::default());
        let properties = parse("log-chat=false");
        assert!(!properties.logs.chat);
        assert_eq!(properties.logs.level, LogLevel::Info);
    }

    #[test]
    fn a_vanilla_file_works_and_its_other_properties_are_ignored() {
        let vanilla = "\
server-name=Dedicated Server
# Used as the server name
gamemode=survival
level-name=My World
level-seed=
level-type=FLAT
server-port=19132
gamemode=adventure
";
        let properties = parse(vanilla);
        assert_eq!(properties.level_name, "My World");
        // The last of a property given twice counts.
        assert_eq!(properties.default_game_mode, GameMode::Adventure);
        assert_eq!(
            properties.ignored,
            ["server-name", "level-seed", "server-port"]
        );
        assert_eq!(
            properties.world_directory(),
            Path::new("worlds").join("My World")
        );
    }

    #[test]
    fn only_flat_worlds_are_supported() {
        assert_eq!(parse("level-type=flat").level_type, LevelType::Flat);
        let err = ServerProperties::parse("\n\nlevel-type=DEFAULT").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("only generates FLAT"), "{err}");
        let err = ServerProperties::parse("level-type=AMPLIFIED").unwrap_err();
        assert!(err.message.contains("unknown level-type"), "{err}");
    }

    #[test]
    fn level_names_stay_inside_the_worlds_folder() {
        // A backslash is an escape in properties files: `a\\b` holds one.
        for bad in ["", "..", "../escape", "a/b", r"a\\b", "C:", "trailing."] {
            let text = format!("level-name={bad}");
            assert!(ServerProperties::parse(&text).is_err(), "{bad:?}");
        }
        assert_eq!(parse("level-name = Spaced Out ").level_name, "Spaced Out");
    }

    #[test]
    fn game_modes_and_log_settings_are_read() {
        assert_eq!(parse("gamemode=1").default_game_mode, GameMode::Creative);
        assert_eq!(
            parse("gamemode=Spectator").default_game_mode,
            GameMode::Spectator
        );
        assert!(ServerProperties::parse("gamemode=hardcore").is_err());
        let debug = parse("log-level=DEBUG\nlog-chat=false");
        assert_eq!(
            debug.logs.filter(),
            "info,bedrockrs=debug,plugin=debug,chat=off"
        );
        assert_eq!(
            Logs::default().filter(),
            "warn,bedrockrs=info,plugin=info,chat=info"
        );
        let quiet = parse("log-level=warn");
        assert_eq!(
            quiet.logs.filter(),
            "warn,bedrockrs=warn,plugin=warn,chat=off",
            "chat is info, so it goes quiet too"
        );
        assert!(ServerProperties::parse("log-level=loud").is_err());
        assert!(ServerProperties::parse("log-chat=yes").is_err());
    }

    #[test]
    fn the_properties_format_is_read_as_java_reads_it() {
        let text = "\
# comment
! also a comment
   indented = value with spaces
colon:separated
space separated
long = first \\
       second
escaped\\=key = tab\\there \\u00e9
empty=
";
        let read: Vec<(String, String)> = parse_properties(text)
            .into_iter()
            .map(|property| (property.key, property.value))
            .collect();
        let expected = [
            ("indented", "value with spaces"),
            ("colon", "separated"),
            ("space", "separated"),
            ("long", "first second"),
            ("escaped=key", "tab\there é"),
            ("empty", ""),
        ];
        assert_eq!(
            read,
            expected
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_missing_file_is_created_with_the_defaults() {
        let directory =
            std::env::temp_dir().join(format!("bedrockrs-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(PROPERTIES_FILE);

        let loaded = ServerProperties::load_or_create(&path).unwrap();
        assert!(loaded.created);
        assert_eq!(loaded.properties, ServerProperties::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_PROPERTIES);

        fs::write(&path, "log-chat=false\n").unwrap();
        let loaded = ServerProperties::load_or_create(&path).unwrap();
        assert!(!loaded.created);
        assert!(!loaded.properties.logs.chat);

        fs::write(&path, "level-type=LEGACY\n").unwrap();
        assert!(matches!(
            ServerProperties::load_or_create(&path),
            Err(ConfigError::Invalid { .. })
        ));
        fs::remove_dir_all(&directory).unwrap();
    }
}
