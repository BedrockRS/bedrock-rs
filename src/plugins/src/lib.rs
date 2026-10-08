//! The BedrockRS plugin engine: Luau and JavaScript plugins side by side,
//! with one API and one lifecycle.
//!
//! Each plugin lives in its own folder in `plugins/`, described by a
//! `plugin.json` [`Manifest`] whose `main` decides what runs it. Both engines
//! are built into the server, so a plugin runs from its source with nothing
//! to install or compile:
//!
//! - a `.luau` script runs in a sandboxed Luau VM (`luau`);
//! - a `.js` or `.mjs` module runs in its own QuickJS runtime (`javascript`).
//!
//! Either way the plugin is a [`Plugin`] with exactly three lifecycle
//! functions, `on_load`, `on_enable` and `on_disable` (see [`plugin`]), and
//! uses the same API, imported from `@bedrock-rs/core`:
//!
//! - `Logger`: `trace`, `debug`, `info`, `warn`, `error`;
//! - `Server`: `on(event, handler)` for game [`Event`]s, some cancellable;
//!   `broadcast`, `getPlayer`, `getPlayers`, `registerCommand` (only in
//!   `on_load`; see [`definition`]) and `getWorld()`;
//! - `Player`: `sendMessage`, `kick`, `setGameMode`, `setHealth`, `damage`;
//! - `World`, from `Server.getWorld()`: the scheduler, `run`, `runTimeout`,
//!   `runInterval`, `clearRun` and `waitTicks` (see [`scheduler`]), in place
//!   of a per-tick hook.
//!
//! Each engine binds the API as native functions; the checks and rules
//! behind them live once, in `state`, for both.
//! Plugins run on one thread, talk to the game only through messages, and
//! reload when their files change.

mod api;
pub mod command;
pub mod definition;
mod host;
mod javascript;
mod javascript_modules;
mod luau;
mod luau_require;
mod manager;
mod manifest;
mod output;
pub mod plugin;
pub mod scheduler;
mod state;
pub mod wire;

pub use api::{Action, BlockChange, DAMAGE_CAUSES, Damage, Event, Player, Position};
pub use command::{
    ArgKind, ArgSpec, ArgValue, CommandCall, CommandNode, CommandReply, CommandSender, CommandSpec,
    GAME_MODE_VALUES, Permission, PluginCommand, ReplyLine,
};
pub use host::{Dispatcher, HostError, PluginConfig, PluginHost};
pub use manifest::{Engine, MANIFEST_FILE, Manifest, ManifestError, PluginSource};
pub use output::{Output, PLUGIN_FIELD, PLUGIN_TARGET, tracing_output};
pub use plugin::{Plugin, PluginError};
pub use state::{Phase, TOP_LEVEL};
