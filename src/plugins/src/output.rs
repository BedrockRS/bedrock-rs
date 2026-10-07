//! Where plugin output (`print` and `log.*`) goes.

use std::sync::Arc;

use tracing::Level;

/// Receives each line a plugin prints or logs: `(plugin, level, message)`.
pub type Output = Arc<dyn Fn(&str, Level, &str) + Send + Sync>;

/// The `tracing` target of plugin output. `tracing` targets are fixed at
/// compile time, so the plugin's name goes in the [`PLUGIN_FIELD`] field; the
/// server's console shows it in place of the target.
pub const PLUGIN_TARGET: &str = "plugin";
/// The field of plugin output holding the plugin's name from its manifest.
pub const PLUGIN_FIELD: &str = "plugin";

/// Sends plugin output to `tracing` under [`PLUGIN_TARGET`].
pub fn tracing_output() -> Output {
    Arc::new(|plugin, level, message| match level {
        Level::ERROR => tracing::error!(target: PLUGIN_TARGET, plugin, "{message}"),
        Level::WARN => tracing::warn!(target: PLUGIN_TARGET, plugin, "{message}"),
        Level::INFO => tracing::info!(target: PLUGIN_TARGET, plugin, "{message}"),
        Level::DEBUG => tracing::debug!(target: PLUGIN_TARGET, plugin, "{message}"),
        _ => tracing::trace!(target: PLUGIN_TARGET, plugin, "{message}"),
    })
}
