//! [`PluginManager`]: every plugin, whatever runs it, and its lifecycle.
//!
//! The manager reads the plugin folders, makes each plugin with the engine
//! its `main` asks for (a [`LuauPlugin`] or a [`JavaScriptPlugin`]), and
//! drives them all the same way, as [`Box<dyn Plugin>`](Plugin). Nothing here
//! knows which language a plugin is in, and each plugin has its own VM or
//! runtime, so the two kinds sit side by side in one list:
//!
//! - **Loading**: the script's top level runs, then `on_load`. If either
//!   fails, the plugin is not loaded.
//! - **Enabling**: `on_enable`, right after. A plugin whose `on_enable` fails
//!   is disabled again and unloaded.
//! - **Disabling**: `on_disable`, before a plugin is unloaded or reloaded,
//!   and when the server stops. Its tasks stop after it.
//! - **Reloading**, when a plugin's files change: the running version is
//!   disabled, then the new one loaded and enabled, so the two never run at
//!   once (what the old one saves in `on_disable`, the new one can load in
//!   `on_load`). First, though, the new files must compile: a save that does
//!   not leaves the running version as it is. A new version that compiles
//!   but fails to load leaves the plugin unloaded until its files change.
//!
//! Events, commands and due tasks only reach enabled plugins. Each tick the
//! manager asks each plugin's task table what is due and has the plugin run
//! just those; plugins have no per-tick hook.
//!
//! A plugin that fails to load is remembered with what failed, so a rescan
//! (after another plugin changed, say) does not run it and log its error
//! again: it is tried again once its own files change.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use crate::javascript::{self, JavaScriptPlugin};
use crate::luau::{self, LuauPlugin};
use crate::manifest::{Engine, MANIFEST_FILE, PluginSource};
use crate::plugin::{Limits, Plugin, PluginError};
use crate::state::{self, Phase, PluginState, Shared, State};
use crate::{Action, CommandCall, CommandReply, Event, Output, PluginCommand};

/// A plugin the manager runs, with what it was made from.
struct Loaded {
    folder: PathBuf,
    source: PluginSource,
    state: State,
    plugin: Box<dyn Plugin>,
}

/// What last failed in a plugin folder.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The plugin, made from this source, failed to load or enable.
    Source(PluginSource),
    /// The folder could not be read as a plugin, for this reason.
    Read(String),
}

/// Every plugin in the plugin directory.
pub(crate) struct PluginManager {
    directory: PathBuf,
    limits: Limits,
    shared: Shared,
    /// The running plugins, in name order.
    plugins: Vec<Loaded>,
    /// Folders whose plugin failed, and what failed, until it changes.
    failed: HashMap<PathBuf, Failure>,
    /// Stray files already warned about, so each is mentioned once.
    warned: HashSet<PathBuf>,
}

impl PluginManager {
    pub fn new(
        directory: PathBuf,
        limits: Limits,
        output: Output,
        actions: mpsc::Sender<Action>,
    ) -> Self {
        Self {
            directory,
            limits,
            shared: Shared::new(actions, output),
            plugins: Vec::new(),
            failed: HashMap::new(),
            warned: HashSet::new(),
        }
    }

    /// The names of the running plugins.
    pub fn names(&self) -> Vec<String> {
        self.plugins
            .iter()
            .map(|loaded| loaded.plugin.name().to_owned())
            .collect()
    }

    /// Every command the running plugins registered, plugin by plugin in
    /// name order.
    pub fn commands(&self) -> Vec<PluginCommand> {
        self.plugins
            .iter()
            .flat_map(|loaded| {
                let state = state::lock(&loaded.state);
                state
                    .commands
                    .iter()
                    .map(|spec| PluginCommand {
                        plugin: state.name.clone(),
                        spec: spec.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Brings the running plugins in line with the plugin folders on disk:
    /// loads new ones, reloads changed ones and unloads removed ones.
    pub fn scan(&mut self) {
        let folders = self.folders();
        let gone: Vec<PathBuf> = self
            .plugins
            .iter()
            .filter(|loaded| !folders.contains(&loaded.folder))
            .map(|loaded| loaded.folder.clone())
            .collect();
        for folder in gone {
            self.unload(&folder);
        }
        self.failed.retain(|folder, _| folders.contains(folder));
        for folder in folders {
            match PluginSource::read(&folder) {
                Ok(Some(source)) => self.load(&folder, source),
                // A folder without a manifest is not (or no longer) a plugin.
                Ok(None) => {
                    self.failed.remove(&folder);
                    if self.position(&folder).is_some() {
                        self.unload(&folder);
                    } else if self.warned.insert(folder.clone()) {
                        tracing::warn!(
                            "Ignored the folder {}: it has no {MANIFEST_FILE}",
                            folder.display()
                        );
                    }
                }
                Err(err) => {
                    // Said once, until the reason changes.
                    let failure = Failure::Read(err.to_string());
                    if self.failed.get(&folder) == Some(&failure) {
                        continue;
                    }
                    match self.position(&folder) {
                        Some(index) => tracing::error!(
                            "Couldn't reload plugin {}, so it keeps running its previous version: {err}",
                            self.plugins[index].plugin.name()
                        ),
                        None => tracing::error!(
                            "Couldn't load the plugin in {}: {err}",
                            folder.display()
                        ),
                    }
                    self.failed.insert(folder, failure);
                }
            }
        }
    }

    /// Delivers `event` to every enabled plugin. Returns whether one
    /// cancelled it; later plugins still see it.
    ///
    /// A joining player can be looked up from `player_join` on, and a leaving
    /// one until the `player_quit` handlers have run.
    pub fn dispatch(&mut self, event: &Event) -> bool {
        if let Event::PlayerJoin(player) = event {
            self.shared
                .roster()
                .insert(player.uuid.clone(), player.clone());
        }
        let mut cancelled = false;
        for loaded in &mut self.plugins {
            if state::lock(&loaded.state).phase == Phase::Enabled {
                cancelled |= loaded.plugin.dispatch(event);
            }
        }
        if let Event::PlayerQuit(player) = event {
            self.shared.roster().remove(&player.uuid);
        }
        cancelled
    }

    /// Runs a plugin command.
    pub fn run_command(&mut self, call: &CommandCall) -> CommandReply {
        let loaded = self.plugins.iter_mut().find(|loaded| {
            let state = state::lock(&loaded.state);
            state.name == call.plugin && state.phase == Phase::Enabled
        });
        match loaded {
            Some(loaded) => loaded.plugin.run_command(call),
            None => CommandReply::error(format!("/{} is no longer available", call.command)),
        }
    }

    /// The server's tick `tick` began: runs the tasks that are due.
    pub fn tick(&mut self, tick: u64) {
        self.shared.clock.store(tick, Ordering::Relaxed);
        for loaded in &mut self.plugins {
            let due = {
                let mut state = state::lock(&loaded.state);
                if state.phase != Phase::Enabled || !state.tasks.any_due(tick) {
                    continue;
                }
                state.tasks.take_due(tick)
            };
            loaded.plugin.run_tasks(&due);
        }
    }

    /// Disables every plugin, as the server stops.
    pub fn shutdown(&mut self) {
        for loaded in &mut self.plugins {
            disable(loaded);
        }
        self.plugins.clear();
    }

    fn position(&self, folder: &Path) -> Option<usize> {
        self.plugins
            .iter()
            .position(|loaded| loaded.folder == folder)
    }

    /// Runs `source` for `folder`, unless it is what already runs there.
    fn load(&mut self, folder: &Path, source: PluginSource) {
        let previous = self.position(folder);
        if previous.is_some_and(|index| self.plugins[index].source == source) {
            // Back to what runs, so whatever failed in between is moot.
            self.failed.remove(folder);
            return;
        }
        if matches!(self.failed.get(folder), Some(Failure::Source(failed)) if *failed == source) {
            return;
        }
        let name = source.manifest.name.clone();
        if let Some(other) = self
            .plugins
            .iter()
            .find(|loaded| loaded.folder != folder && loaded.plugin.name() == name)
        {
            tracing::error!(
                "Didn't load the plugin in {}: plugin {name} is already loaded from {}",
                folder.display(),
                other.folder.display()
            );
            return;
        }
        let reloading = previous.is_some();
        if let Some(index) = previous {
            // A save that doesn't even compile leaves the running version
            // be. Otherwise it stops before the new one loads.
            if let Err(err) = self.check(folder, &source) {
                tracing::error!(
                    "Couldn't reload plugin {name}, so it keeps running its previous version: {err}"
                );
                self.failed
                    .insert(folder.to_owned(), Failure::Source(source));
                return;
            }
            let mut old = self.plugins.remove(index);
            disable(&mut old);
        }
        let state = PluginState::new(&name, self.shared.clone());
        let plugin = match self.start(folder, &source, &state) {
            Ok(plugin) => plugin,
            Err(err) => {
                if reloading {
                    tracing::error!(
                        "Couldn't reload plugin {name}, so it is unloaded until its files change: {err}"
                    );
                } else {
                    tracing::error!("Couldn't load plugin {name}: {err}");
                }
                self.failed
                    .insert(folder.to_owned(), Failure::Source(source));
                return;
            }
        };
        self.failed.remove(folder);
        let manifest = &source.manifest;
        tracing::info!(
            "{} plugin: {name} ({}, {})",
            if reloading { "Reloaded" } else { "Loaded" },
            manifest.version,
            manifest.author
        );
        // What it printed while loading comes after the line saying so.
        release_output(&state);

        let mut loaded = Loaded {
            folder: folder.to_owned(),
            source,
            state,
            plugin,
        };
        state::lock(&loaded.state).phase = Phase::Enabled;
        if let Err(err) = loaded.plugin.on_enable() {
            tracing::error!("Plugin {name} failed to enable, so it was unloaded: {err}");
            disable(&mut loaded);
            self.failed
                .insert(loaded.folder, Failure::Source(loaded.source));
            return;
        }
        let at = self
            .plugins
            .partition_point(|other| other.plugin.name() < name.as_str());
        self.plugins.insert(at, loaded);
    }

    /// Whether the plugin's files compile, without running them.
    fn check(&self, folder: &Path, source: &PluginSource) -> Result<(), PluginError> {
        match source.engine {
            Engine::Luau => luau::check(folder, source, self.limits),
            Engine::JavaScript => javascript::check(folder, source, self.limits),
        }
    }

    /// Makes the plugin and runs it up to loaded: its top level, then
    /// `on_load`.
    fn start(
        &self,
        folder: &Path,
        source: &PluginSource,
        state: &State,
    ) -> Result<Box<dyn Plugin>, PluginError> {
        let main = Path::new(&source.manifest.main);
        let script = std::str::from_utf8(&source.source)
            .map_err(|_| PluginError::Script(format!("{} is not UTF-8 text", main.display())))?;
        // The one place the engines differ: what makes the plugin.
        let mut plugin: Box<dyn Plugin> = match source.engine {
            Engine::Luau => Box::new(LuauPlugin::new(
                State::clone(state),
                folder,
                main,
                script,
                self.limits,
            )?),
            Engine::JavaScript => Box::new(JavaScriptPlugin::new(
                State::clone(state),
                folder,
                main,
                script,
                self.limits,
            )?),
        };
        state::lock(state).phase = Phase::Loading;
        plugin.on_load()?;
        state::lock(state).phase = Phase::Loaded;
        Ok(plugin)
    }

    fn unload(&mut self, folder: &Path) {
        if let Some(index) = self.position(folder) {
            let mut loaded = self.plugins.remove(index);
            disable(&mut loaded);
            tracing::info!("Unloaded plugin: {}", loaded.plugin.name());
        }
    }

    /// The folders directly inside the plugin directory, in name order.
    /// Loose scripts from before plugins had folders are pointed out once.
    fn folders(&mut self) -> Vec<PathBuf> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::error!(
                    "Couldn't list the plugins in {}: {err}",
                    self.directory.display()
                );
                return Vec::new();
            }
        };
        let mut folders = Vec::new();
        for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
            let hidden = path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'));
            if path.is_dir() && !hidden {
                folders.push(path);
            } else if path.is_file()
                && path.extension().is_some_and(|extension| {
                    ["luau", "js", "mjs"]
                        .iter()
                        .any(|script| extension.eq_ignore_ascii_case(script))
                })
                && self.warned.insert(path.clone())
            {
                tracing::warn!(
                    "Ignored {}: plugins live in their own folder with a {MANIFEST_FILE}",
                    path.display()
                );
            }
        }
        folders.sort();
        folders
    }
}

/// Runs the plugin's `on_disable` and stops everything it scheduled.
fn disable(loaded: &mut Loaded) {
    let enabled = {
        let mut state = state::lock(&loaded.state);
        let enabled = matches!(state.phase, Phase::Enabled | Phase::Loaded);
        state.phase = Phase::Disabling;
        enabled
    };
    if enabled && let Err(err) = loaded.plugin.on_disable() {
        tracing::error!("Plugin {} failed to disable: {err}", loaded.plugin.name());
    }
    let mut state = state::lock(&loaded.state);
    state.phase = Phase::Disabled;
    state.tasks.clear_all();
}

fn release_output(state: &State) {
    let (name, output, lines) = {
        let mut state = state::lock(state);
        let lines = state.release_output();
        (state.name.clone(), Output::clone(state.output()), lines)
    };
    for (level, message) in lines {
        output(&name, level, &message);
    }
}
