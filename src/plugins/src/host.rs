//! The plugin host: an engine thread plus a watcher that reloads changed plugins.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::luau::{Limits, LuauEngine};
use crate::manifest::{MANIFEST_FILE, PluginSource};
use crate::{Action, CommandCall, CommandReply, Event, Output, PluginCommand, tracing_output};

/// Quiet period after the last change in the plugin directory before plugins
/// are reloaded; editors often save in several steps.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// Plugin host settings.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Directory holding one folder per plugin; created if missing.
    pub directory: PathBuf,
    /// Reload plugins when their files are saved, added or removed.
    pub hot_reload: bool,
    /// Memory each plugin VM may allocate.
    pub memory_limit: usize,
    /// Longest a single call into a plugin may run before it is aborted.
    pub execution_limit: Duration,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("plugins"),
            hot_reload: true,
            memory_limit: 64 * 1024 * 1024,
            execution_limit: Duration::from_secs(1),
        }
    }
}

/// Errors starting a [`PluginHost`]. Individual plugins that fail to load are
/// logged instead, so one broken plugin never stops the server.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("failed to create plugin directory {}: {source}", .path.display())]
    Directory { path: PathBuf, source: io::Error },
    #[error("failed to watch {} for changes: {source}", .path.display())]
    Watch {
        path: PathBuf,
        source: notify::Error,
    },
    #[error("failed to start the plugin thread: {0}")]
    Thread(io::Error),
    #[error("the plugin thread stopped during startup")]
    Startup,
}

#[derive(Debug)]
enum Command {
    /// Something in the plugin directory changed.
    Changed,
    /// An event for the plugins; for a cancellable one, where to say whether
    /// a plugin cancelled it.
    Event(Event, Option<oneshot::Sender<bool>>),
    /// A plugin command someone ran, and where its reply goes.
    Execute(CommandCall, oneshot::Sender<CommandReply>),
    Shutdown,
}

/// Runs the plugins in a directory on a dedicated thread, reloading them as
/// their files change. Dropping the host unloads every plugin.
#[derive(Debug)]
pub struct PluginHost {
    commands: mpsc::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
    _watcher: Option<RecommendedWatcher>,
    loaded: Vec<String>,
}

/// Delivers game events to the plugins. Cheap to clone and usable from any
/// thread; events sent after the host has stopped are dropped.
#[derive(Debug, Clone)]
pub struct Dispatcher {
    commands: mpsc::Sender<Command>,
}

impl Dispatcher {
    /// A dispatcher with no plugins behind it, which drops every event; for
    /// running without a plugin host, e.g. in tests.
    pub fn disconnected() -> Self {
        let (commands, _) = mpsc::channel();
        Self { commands }
    }

    /// Queues `event` for every plugin listening for it.
    pub fn dispatch(&self, event: Event) {
        let _ = self.commands.send(Command::Event(event, None));
    }

    /// Delivers a cancellable `event` to every plugin listening for it and
    /// resolves to whether one of them cancelled it. Without plugins, or if
    /// they stop first, nothing cancels it.
    pub async fn dispatch_cancellable(&self, event: Event) -> bool {
        let (verdict, cancelled) = oneshot::channel();
        if self
            .commands
            .send(Command::Event(event, Some(verdict)))
            .is_err()
        {
            return false;
        }
        cancelled.await.unwrap_or(false)
    }

    /// Runs a plugin command and resolves to what it replied. `None` if the
    /// plugins stopped first.
    pub async fn run_command(&self, call: CommandCall) -> Option<CommandReply> {
        let (reply, replied) = oneshot::channel();
        self.commands.send(Command::Execute(call, reply)).ok()?;
        replied.await.ok()
    }
}

impl PluginHost {
    /// Loads every plugin in the configured directory, logging their output
    /// through `tracing`, and returns once the initial load has finished.
    /// What plugins ask of the server arrives on `actions`.
    pub fn start(
        config: PluginConfig,
        actions: tokio_mpsc::Sender<Action>,
    ) -> Result<Self, PluginError> {
        Self::with_output(config, tracing_output(), actions)
    }

    /// Like [`PluginHost::start`], sending plugin output to `output`.
    pub fn with_output(
        config: PluginConfig,
        output: Output,
        actions: tokio_mpsc::Sender<Action>,
    ) -> Result<Self, PluginError> {
        fs::create_dir_all(&config.directory).map_err(|source| PluginError::Directory {
            path: config.directory.clone(),
            source,
        })?;

        let (commands, receiver) = mpsc::channel();
        // Start watching before the initial scan so no save in between is missed.
        let watcher = if config.hot_reload {
            Some(watch(&config.directory, commands.clone())?)
        } else {
            None
        };

        let (ready, started) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("luau-plugins".into())
            .spawn(move || run(config, output, actions, receiver, ready))
            .map_err(PluginError::Thread)?;
        let loaded = started.recv().map_err(|_| PluginError::Startup)?;

        Ok(Self {
            commands,
            thread: Some(thread),
            _watcher: watcher,
            loaded,
        })
    }

    /// Names of the plugins that loaded successfully at startup.
    pub fn loaded(&self) -> &[String] {
        &self.loaded
    }

    /// A handle for sending game events to the plugins.
    pub fn dispatcher(&self) -> Dispatcher {
        Dispatcher {
            commands: self.commands.clone(),
        }
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The engine thread: loads everything once, then delivers events and
/// rescans the plugin directory after changes settle.
fn run(
    config: PluginConfig,
    output: Output,
    actions: tokio_mpsc::Sender<Action>,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<Vec<String>>,
) {
    let limits = Limits {
        memory: config.memory_limit,
        execution: config.execution_limit,
    };
    let mut plugins = Plugins {
        directory: config.directory,
        engine: LuauEngine::new(limits, output, actions.clone()),
        running: BTreeMap::new(),
        warned: HashSet::new(),
    };
    plugins.scan();
    // The server learns the plugins' commands before anyone can join.
    let mut announced = Vec::new();
    announce_commands(&plugins.engine, &actions, &mut announced);
    let _ = ready.send(plugins.engine.names());

    // When to rescan; each change pushes it back, events do not.
    let mut rescan_at: Option<Instant> = None;
    loop {
        let command = match rescan_at {
            None => commands.recv().ok(),
            Some(at) => match commands.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => {
                    plugins.scan();
                    rescan_at = None;
                    announce_commands(&plugins.engine, &actions, &mut announced);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => None,
            },
        };
        match command {
            Some(Command::Changed) => rescan_at = Some(Instant::now() + RELOAD_DEBOUNCE),
            Some(Command::Event(event, verdict)) => {
                let cancelled = plugins.engine.dispatch(&event);
                if let Some(verdict) = verdict {
                    let _ = verdict.send(cancelled);
                }
            }
            Some(Command::Execute(call, reply)) => {
                let _ = reply.send(plugins.engine.run_command(&call));
            }
            Some(Command::Shutdown) | None => break,
        }
        // Loading, unloading or a handler may have changed the commands.
        announce_commands(&plugins.engine, &actions, &mut announced);
    }
}

/// Tells the server the plugins' commands if they changed since `announced`.
/// If the server is not keeping up, it is told on a later try.
fn announce_commands(
    engine: &LuauEngine,
    actions: &tokio_mpsc::Sender<Action>,
    announced: &mut Vec<PluginCommand>,
) {
    let commands = engine.commands();
    if commands == *announced {
        return;
    }
    match actions.try_send(Action::SetCommands(commands.clone())) {
        Ok(()) => *announced = commands,
        Err(tokio_mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!("the server is not keeping up; plugin commands will be updated later");
        }
        // The server is shutting down.
        Err(tokio_mpsc::error::TrySendError::Closed(_)) => *announced = commands,
    }
}

/// The plugins found in the plugin directory and what is running of them.
struct Plugins {
    directory: PathBuf,
    engine: LuauEngine,
    /// What each plugin folder is running, by folder.
    running: BTreeMap<PathBuf, PluginSource>,
    /// Stray files already warned about, so each is mentioned once.
    warned: HashSet<PathBuf>,
}

impl Plugins {
    /// Brings the running plugins in line with the plugin folders on disk:
    /// loads new ones, reloads changed ones and unloads removed ones.
    fn scan(&mut self) {
        let folders = self.folders();
        let gone: Vec<PathBuf> = self
            .running
            .keys()
            .filter(|folder| !folders.contains(folder))
            .cloned()
            .collect();
        for folder in gone {
            self.unload(&folder);
        }
        for folder in folders {
            match PluginSource::read(&folder) {
                Ok(Some(plugin)) => self.load(&folder, plugin),
                // A folder without a manifest is not (or no longer) a plugin.
                Ok(None) => {
                    if self.running.contains_key(&folder) {
                        self.unload(&folder);
                    } else if self.warned.insert(folder.clone()) {
                        tracing::warn!(
                            folder = %folder.display(),
                            "ignoring a plugin folder without a {MANIFEST_FILE}"
                        );
                    }
                }
                Err(err) => match self.running.get(&folder) {
                    Some(running) => tracing::error!(
                        plugin = %running.manifest.name,
                        "failed to reload plugin, keeping the running version: {err}"
                    ),
                    None => tracing::error!(
                        folder = %folder.display(),
                        "failed to load plugin: {err}"
                    ),
                },
            }
        }
    }

    /// Runs `plugin` for `folder`, unless it is what already runs there.
    fn load(&mut self, folder: &Path, plugin: PluginSource) {
        let previous = self.running.get(folder);
        if previous == Some(&plugin) {
            return;
        }
        let name = plugin.manifest.name.clone();
        if let Some((other, _)) = self
            .running
            .iter()
            .find(|(other, running)| *other != folder && running.manifest.name == name)
        {
            tracing::error!(
                plugin = %name,
                folder = %folder.display(),
                other = %other.display(),
                "not loading a plugin with the same name as another"
            );
            return;
        }
        let renamed_from = previous
            .map(|running| running.manifest.name.clone())
            .filter(|previous| *previous != name);
        let chunk_name = plugin.main_path.display().to_string();
        match self.engine.load(&name, &chunk_name, &plugin.source) {
            Ok(_) => {
                if let Some(old) = renamed_from {
                    self.engine.unload(&old);
                }
                let manifest = &plugin.manifest;
                tracing::info!(
                    plugin = %name,
                    version = %manifest.version,
                    author = %manifest.author,
                    "{} plugin: {}",
                    if previous.is_some() { "reloaded" } else { "loaded" },
                    manifest.description
                );
                self.running.insert(folder.to_owned(), plugin);
            }
            Err(err) if previous.is_some() => tracing::error!(
                plugin = %name,
                "failed to reload plugin, keeping the running version: {err}"
            ),
            Err(err) => tracing::error!(plugin = %name, "failed to load plugin: {err}"),
        }
    }

    fn unload(&mut self, folder: &Path) {
        if let Some(plugin) = self.running.remove(folder) {
            self.engine.unload(&plugin.manifest.name);
            tracing::info!(plugin = %plugin.manifest.name, "unloaded plugin");
        }
    }

    /// The folders directly inside the plugin directory, in name order. Loose
    /// scripts from before plugins had folders are pointed out once.
    fn folders(&mut self) -> Vec<PathBuf> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::error!(directory = %self.directory.display(), %err, "failed to list plugins");
                return Vec::new();
            }
        };
        let mut folders = Vec::new();
        for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
            if path.is_dir() {
                folders.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("luau"))
                && self.warned.insert(path.clone())
            {
                tracing::warn!(
                    file = %path.display(),
                    "ignoring a loose script: plugins live in their own folder with a {MANIFEST_FILE}"
                );
            }
        }
        folders.sort();
        folders
    }
}

fn watch(
    directory: &Path,
    commands: mpsc::Sender<Command>,
) -> Result<RecommendedWatcher, PluginError> {
    let watch_error = |source| PluginError::Watch {
        path: directory.to_owned(),
        source,
    };
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) => {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    let _ = commands.send(Command::Changed);
                }
            }
            Err(err) => tracing::warn!(%err, "plugin file watcher error"),
        })
        .map_err(watch_error)?;
    watcher
        .watch(directory, RecursiveMode::Recursive)
        .map_err(watch_error)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    /// A fresh, empty plugin directory for one test.
    fn plugin_directory(test: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("bedrockrs-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// Writes a plugin folder with a manifest naming `name` and `main.luau`.
    fn write_plugin(directory: &Path, folder: &str, name: &str, source: &str) {
        let folder = directory.join(folder);
        fs::create_dir_all(&folder).unwrap();
        let manifest = serde_json::json!({
            "name": name,
            "description": "A test plugin",
            "version": "1.0.0",
            "author": "Tester",
            "main": "main.luau",
        });
        fs::write(folder.join(MANIFEST_FILE), manifest.to_string()).unwrap();
        fs::write(folder.join("main.luau"), source).unwrap();
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn loads_plugin_folders_and_reloads_saved_scripts() {
        let directory = plugin_directory("plugins");
        write_plugin(&directory, "greet", "greeter", r#"print("v1")"#);
        write_plugin(&directory, "twin", "greeter", r#"print("twin")"#);
        fs::create_dir_all(directory.join("empty")).unwrap();
        fs::write(directory.join("loose.luau"), r#"print("loose")"#).unwrap();

        let messages = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&messages);
        let output: Output = Arc::new(move |plugin, _, message| {
            sink.lock().unwrap().push(format!("{plugin}: {message}"));
        });
        let config = PluginConfig {
            directory: directory.clone(),
            ..PluginConfig::default()
        };
        let (actions, _) = tokio_mpsc::channel(1);
        let host = PluginHost::with_output(config, output, actions).unwrap();
        // The folder without a manifest, the loose script and the second
        // plugin claiming the same name are all skipped.
        assert_eq!(host.loaded(), ["greeter"]);
        assert_eq!(*messages.lock().unwrap(), ["greeter: v1"]);

        fs::write(directory.join("greet").join("main.luau"), r#"print("v2")"#).unwrap();
        wait_until("the plugin was not reloaded", || {
            messages.lock().unwrap().contains(&"greeter: v2".to_owned())
        });

        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn events_reach_plugins_and_their_actions_reach_the_server() {
        let directory = plugin_directory("events");
        write_plugin(
            &directory,
            "welcome",
            "welcome",
            r#"server.on("player_join", function(player) server.broadcast("hi " .. player.name) end)"#,
        );

        let config = PluginConfig {
            directory: directory.clone(),
            hot_reload: false,
            ..PluginConfig::default()
        };
        let (actions, mut received) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config, Arc::new(|_, _, _| {}), actions).unwrap();
        host.dispatcher().dispatch(Event::PlayerJoin(crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        }));

        let mut action = None;
        wait_until("the plugin did not broadcast", || {
            action = received.try_recv().ok();
            action.is_some()
        });
        assert_eq!(action, Some(Action::Broadcast("hi Steve".into())));

        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn plugins_can_cancel_chat() {
        let directory = plugin_directory("cancel");
        write_plugin(
            &directory,
            "filter",
            "filter",
            r#"
                server.on("player_chat", function(event)
                    if event.message:find("badword") then event.cancel() end
                end)
            "#,
        );
        let config = PluginConfig {
            directory: directory.clone(),
            hot_reload: false,
            ..PluginConfig::default()
        };
        let (actions, _) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config, Arc::new(|_, _, _| {}), actions).unwrap();
        let chat = |message: &str| Event::PlayerChat {
            player: crate::Player {
                name: "Steve".into(),
                uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
            },
            message: message.into(),
        };
        let dispatcher = host.dispatcher();
        assert!(dispatcher.dispatch_cancellable(chat("a badword")).await);
        assert!(!dispatcher.dispatch_cancellable(chat("hello")).await);

        drop(host);
        assert!(
            !dispatcher.dispatch_cancellable(chat("a badword")).await,
            "with the plugins gone, nothing cancels"
        );
        fs::remove_dir_all(&directory).unwrap();
    }
}
