//! Polyglot plugin engine for BedrockRS.
//!
//! Loads plugins from their own folders in the `plugins/` directory, each
//! described by a `plugin.json` [`Manifest`], and reloads them when they
//! change, with no build step. Each scripting engine runs on its
//! own thread with one isolated VM per plugin and talks to the game loop only
//! through messages.
//!
//! Engines are selected with cargo features:
//! - `luau` (default): Luau via `mlua`
//! - `js`: JavaScript/TypeScript via `deno_core` (TypeScript is transpiled on load)
//! - `python`: Python via RustPython
//!
//! Only the Luau engine exists so far; [`PluginHost`] runs Luau plugins.
//! Plugins listen for game [`Event`]s with `server.on(name, handler)`, may
//! cancel some (`player_chat`), and ask the server for [`Action`]s such as
//! `server.broadcast(message)`, or, on a player from an event,
//! `player.send_message(message)` and `player.kick(reason)`. Plugins add
//! slash commands, with subcommands and typed arguments, with
//! `server.command(definition)`; see [`command`] for how they are described.

mod api;
pub mod command;
#[cfg(feature = "luau")]
mod host;
#[cfg(feature = "luau")]
mod luau;
#[cfg(feature = "luau")]
mod luau_commands;
mod manifest;
mod output;

pub use api::{Action, BlockChange, Event, Player, Position};
pub use command::{
    ArgKind, ArgSpec, ArgValue, CommandCall, CommandNode, CommandReply, CommandSender, CommandSpec,
    GAME_MODE_VALUES, Permission, PluginCommand, ReplyLine,
};
#[cfg(feature = "luau")]
pub use host::{Dispatcher, PluginConfig, PluginError, PluginHost};
pub use manifest::{MANIFEST_FILE, Manifest, ManifestError, PluginSource};
pub use output::{Output, PLUGIN_FIELD, PLUGIN_TARGET, tracing_output};
