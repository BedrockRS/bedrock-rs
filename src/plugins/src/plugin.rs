//! The one shape every plugin has, whatever runs it.
//!
//! A plugin's script (Luau) or module (JavaScript) may define three
//! lifecycle functions, called by the `PluginManager` in this order:
//!
//! 1. **`on_load`**: when the plugin is loaded. The only time it may register
//!    commands, so the server knows them all before anyone can use them.
//! 2. **`on_enable`**: once loaded. Events, commands and scheduled tasks reach
//!    the plugin from now on.
//! 3. **`on_disable`**: before it is unloaded, reloaded, or the server stops:
//!    the time to save. Its scheduled tasks are cancelled after it.
//!
//! Code at the top level of the script runs before `on_load`, and may only
//! set things up: the API is not available there, in either language.
//!
//! There is deliberately no per-tick hook. Plugins that need time use the
//! scheduler (`Server.getWorld().runInterval(...)` and friends), so a plugin
//! that schedules nothing costs nothing per tick.

use std::time::Duration;

use crate::{CommandCall, CommandReply, Event};

/// What a failing command handler's player is told; the details go to the log.
pub(crate) const HANDLER_FAILED: &str = "An error occurred while running this command";

/// Resource limits applied to every plugin, in either engine.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub memory: usize,
    /// Longest a single call into a plugin may run.
    pub execution: Duration,
}

/// Why a plugin could not be loaded or a lifecycle function failed.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The script itself failed: a syntax error, a runtime error, a time or
    /// memory limit.
    #[error("{0}")]
    Script(String),
}

/// A plugin, as the plugin manager drives it.
///
/// The three lifecycle methods are the plugin's own. The rest are how the
/// manager hands it what the server sends (events, commands, and the tasks it
/// scheduled that are due); plugin code never sees them as such.
pub trait Plugin {
    /// The plugin's name, from its manifest.
    fn name(&self) -> &str;

    // ---- Lifecycle ------------------------------------------------------

    /// Calls the plugin's `on_load`, if it has one.
    fn on_load(&mut self) -> Result<(), PluginError>;
    /// Calls the plugin's `on_enable`, if it has one.
    fn on_enable(&mut self) -> Result<(), PluginError>;
    /// Calls the plugin's `on_disable`, if it has one.
    fn on_disable(&mut self) -> Result<(), PluginError>;

    // ---- What the server sends ------------------------------------------

    /// Calls the plugin's handlers for `event`. Returns whether one
    /// cancelled it. A handler that fails is logged and the others still run.
    fn dispatch(&mut self, event: &Event) -> bool;

    /// Runs the handler of one of the plugin's commands.
    fn run_command(&mut self, call: &CommandCall) -> CommandReply;

    /// Runs the scheduled tasks with these ids, which are due.
    fn run_tasks(&mut self, ids: &[u32]);
}
