//! The plugin host: the thread every plugin runs on, plus a watcher that
//! reloads changed plugins.
//!
//! Luau VMs and JavaScript runtimes cannot leave the thread that made them,
//! so every plugin, in either language, lives on one thread, driven by a
//! [`PluginManager`], and talks to the game only through messages: events,
//! commands and ticks in, actions out.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::manager::PluginManager;
use crate::plugin::Limits;
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
pub enum HostError {
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
    /// The server began a tick: scheduled tasks may be due.
    Tick(u64),
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

    /// Tells the plugins the server began tick `tick`, so the tasks they
    /// scheduled for it run.
    pub fn tick(&self, tick: u64) {
        let _ = self.commands.send(Command::Tick(tick));
    }
}

impl PluginHost {
    /// Loads every plugin in the configured directory, logging their output
    /// through `tracing`, and returns once the initial load has finished.
    /// What plugins ask of the server arrives on `actions`.
    pub fn start(
        config: PluginConfig,
        actions: tokio_mpsc::Sender<Action>,
    ) -> Result<Self, HostError> {
        Self::with_output(config, tracing_output(), actions)
    }

    /// Like [`PluginHost::start`], sending plugin output to `output`.
    pub fn with_output(
        config: PluginConfig,
        output: Output,
        actions: tokio_mpsc::Sender<Action>,
    ) -> Result<Self, HostError> {
        fs::create_dir_all(&config.directory).map_err(|source| HostError::Directory {
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
            .name("plugins".into())
            .spawn(move || run(config, output, actions, receiver, ready))
            .map_err(HostError::Thread)?;
        let loaded = started.recv().map_err(|_| HostError::Startup)?;

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

/// The plugin thread: loads everything once, then delivers events, commands
/// and ticks, and rescans the plugin directory after changes settle.
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
    let mut plugins = PluginManager::new(config.directory, limits, output, actions.clone());
    plugins.scan();
    // The server learns the plugins' commands before anyone can join.
    let mut announced = Vec::new();
    announce_commands(&plugins, &actions, &mut announced);
    let _ = ready.send(plugins.names());

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
                    announce_commands(&plugins, &actions, &mut announced);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => None,
            },
        };
        match command {
            Some(Command::Changed) => rescan_at = Some(Instant::now() + RELOAD_DEBOUNCE),
            Some(Command::Event(event, verdict)) => {
                let cancelled = plugins.dispatch(&event);
                if let Some(verdict) = verdict {
                    let _ = verdict.send(cancelled);
                }
            }
            Some(Command::Execute(call, reply)) => {
                let _ = reply.send(plugins.run_command(&call));
            }
            Some(Command::Tick(tick)) => plugins.tick(tick),
            Some(Command::Shutdown) | None => break,
        }
        // Loading, unloading or a handler may have changed the commands.
        announce_commands(&plugins, &actions, &mut announced);
    }
    plugins.shutdown();
}

/// Tells the server the plugins' commands if they changed since `announced`.
/// If the server is not keeping up, it is told on a later try.
fn announce_commands(
    plugins: &PluginManager,
    actions: &tokio_mpsc::Sender<Action>,
    announced: &mut Vec<PluginCommand>,
) {
    let commands = plugins.commands();
    if commands == *announced {
        return;
    }
    match actions.try_send(Action::SetCommands(commands.clone())) {
        Ok(()) => *announced = commands,
        Err(tokio_mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!("The server isn't keeping up; plugin commands will be updated later");
        }
        // The server is shutting down.
        Err(tokio_mpsc::error::TrySendError::Closed(_)) => *announced = commands,
    }
}

fn watch(
    directory: &Path,
    commands: mpsc::Sender<Command>,
) -> Result<RecommendedWatcher, HostError> {
    let watch_error = |source| HostError::Watch {
        path: directory.to_owned(),
        source,
    };
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) => {
                if is_change(&event) {
                    let _ = commands.send(Command::Changed);
                }
            }
            Err(err) => tracing::warn!("Watching the plugins for changes failed: {err}"),
        })
        .map_err(watch_error)?;
    watcher
        .watch(directory, RecursiveMode::Recursive)
        .map_err(watch_error)?;
    Ok(watcher)
}

/// Whether a watcher event can mean a plugin changed: a file or folder made,
/// removed or renamed, or a file modified.
///
/// A folder reported as modified is not a change in itself, since changes to
/// the files in it are reported for those files. Windows reports one when a
/// folder is merely listed soon after files were added to it (NTFS updates a
/// folder's metadata lazily), so counting it would rescan the plugins right
/// after the startup scan read a freshly copied plugin.
fn is_change(event: &notify::Event) -> bool {
    match event.kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
            true
        }
        EventKind::Modify(_) => {
            event.paths.is_empty() || event.paths.iter().any(|path| !path.is_dir())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;
    use crate::manifest::MANIFEST_FILE;

    /// A fresh, empty plugin directory for one test.
    fn plugin_directory(test: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("bedrockrs-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// Writes a plugin folder with a manifest naming `name` and `main`.
    fn write_plugin_file(directory: &Path, folder: &str, name: &str, main: &str, source: &str) {
        let folder = directory.join(folder);
        fs::create_dir_all(&folder).unwrap();
        let manifest = serde_json::json!({
            "name": name,
            "description": "A test plugin",
            "version": "1.0.0",
            "author": "Tester",
            "main": main,
        });
        fs::write(folder.join(MANIFEST_FILE), manifest.to_string()).unwrap();
        fs::write(folder.join(main), source).unwrap();
    }

    fn write_plugin(directory: &Path, folder: &str, name: &str, source: &str) {
        write_plugin_file(directory, folder, name, "main.luau", source);
    }

    /// A script whose `on_load` prints `message`.
    fn printing(message: &str) -> String {
        format!(r#"return {{ on_load = function() print("{message}") end }}"#)
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn recording() -> (Output, Arc<Mutex<Vec<String>>>) {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&messages);
        let output: Output = Arc::new(move |plugin, _, message| {
            sink.lock().unwrap().push(format!("{plugin}: {message}"));
        });
        (output, messages)
    }

    fn config(directory: &Path, hot_reload: bool) -> PluginConfig {
        PluginConfig {
            directory: directory.to_owned(),
            hot_reload,
            ..PluginConfig::default()
        }
    }

    fn steve() -> crate::Player {
        crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        }
    }

    #[test]
    fn loads_plugin_folders_and_reloads_saved_scripts() {
        let directory = plugin_directory("plugins");
        write_plugin(&directory, "greet", "greeter", &printing("v1"));
        write_plugin(&directory, "twin", "greeter", &printing("twin"));
        fs::create_dir_all(directory.join("empty")).unwrap();
        fs::write(directory.join("loose.luau"), r#"print("loose")"#).unwrap();

        let (output, messages) = recording();
        let (actions, _) = tokio_mpsc::channel(1);
        let host = PluginHost::with_output(config(&directory, true), output, actions).unwrap();
        // The folder without a manifest, the loose script and the second
        // plugin claiming the same name are all skipped.
        assert_eq!(host.loaded(), ["greeter"]);
        assert_eq!(*messages.lock().unwrap(), ["greeter: v1"]);

        fs::write(directory.join("greet").join("main.luau"), printing("v2")).unwrap();
        wait_until("the plugin was not reloaded", || {
            messages.lock().unwrap().contains(&"greeter: v2".to_owned())
        });

        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn saving_a_required_module_reloads_the_plugin() {
        let directory = plugin_directory("modules");
        write_plugin(
            &directory,
            "split",
            "split",
            r#"
                local words = require("./lib/words")
                return { on_load = function() print(words.greeting) end }
            "#,
        );
        let words = directory.join("split").join("lib").join("words.luau");
        fs::create_dir_all(words.parent().unwrap()).unwrap();
        fs::write(&words, r#"return { greeting = "v1" }"#).unwrap();

        let (output, messages) = recording();
        let (actions, _) = tokio_mpsc::channel(1);
        let host = PluginHost::with_output(config(&directory, true), output, actions).unwrap();
        assert_eq!(*messages.lock().unwrap(), ["split: v1"]);

        // Only the module changes; the entry script stays the same.
        fs::write(&words, r#"return { greeting = "v2" }"#).unwrap();
        wait_until("the plugin was not reloaded", || {
            messages.lock().unwrap().contains(&"split: v2".to_owned())
        });

        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn the_lifecycle_runs_in_order_through_reloads_and_shutdown() {
        let directory = plugin_directory("lifecycle");
        let script = |version: &str| {
            format!(
                r#"
                    return {{
                        on_load = function() print("load {version}") end,
                        on_enable = function() print("enable {version}") end,
                        on_disable = function() print("disable {version}") end,
                    }}
                "#
            )
        };
        write_plugin(&directory, "life", "life", &script("1"));
        let (output, messages) = recording();
        let (actions, _) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config(&directory, true), output, actions).unwrap();
        fs::write(directory.join("life").join("main.luau"), script("2")).unwrap();
        wait_until("the plugin was not reloaded", || {
            messages
                .lock()
                .unwrap()
                .contains(&"life: enable 2".to_owned())
        });
        drop(host);
        assert_eq!(
            *messages.lock().unwrap(),
            [
                "life: load 1",
                "life: enable 1",
                // The old version stops before the new one loads and starts.
                "life: disable 1",
                "life: load 2",
                "life: enable 2",
                "life: disable 2",
            ]
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    /// A save that doesn't compile leaves the running version alone; one
    /// that compiles but fails to load leaves the plugin unloaded, until a
    /// working version is saved.
    #[test]
    fn a_reload_stops_the_running_version_only_if_the_new_one_compiles() {
        let directory = plugin_directory("reload-check");
        let script = |version: &str, on_load: &str| {
            format!(
                r#"
                    local Server = require("@bedrock-rs/core").Server
                    return {{
                        on_load = function()
                            {on_load}
                            Server.on("player_join", function() print("{version} sees a join") end)
                        end,
                        on_disable = function() print("disable {version}") end,
                    }}
                "#
            )
        };
        let main = directory.join("life").join("main.luau");
        write_plugin(&directory, "life", "life", &script("v1", ""));
        let (output, messages) = recording();
        let (actions, _) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config(&directory, true), output, actions).unwrap();
        let dispatcher = host.dispatcher();
        let lines = || messages.lock().unwrap().clone();
        let joined = |expected: usize| {
            dispatcher.dispatch(Event::PlayerJoin(steve()));
            wait_until("the join was not handled", || lines().len() == expected);
        };
        joined(1);

        // A syntax error: v1 keeps running.
        fs::write(&main, "return { on_load = function( }").unwrap();
        thread::sleep(RELOAD_DEBOUNCE * 5);
        joined(2);
        assert_eq!(lines(), ["life: v1 sees a join", "life: v1 sees a join"]);

        // It compiles but fails in on_load: v1 stops, and nothing replaces it.
        fs::write(&main, script("v2", r#"error("no")"#)).unwrap();
        wait_until("v1 was not disabled", || {
            lines().contains(&"life: disable v1".to_owned())
        });
        dispatcher.dispatch(Event::PlayerJoin(steve()));
        thread::sleep(RELOAD_DEBOUNCE * 3);
        assert_eq!(lines().len(), 3, "{:?}", lines());

        // Fixed, it loads.
        fs::write(&main, script("v3", "")).unwrap();
        thread::sleep(RELOAD_DEBOUNCE * 5);
        joined(4);
        assert_eq!(lines().last().unwrap(), "life: v3 sees a join");
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
            r#"
                local Server = require("@bedrock-rs/core").Server
                return {
                    on_load = function()
                        Server.on("player_join", function(event)
                            Server.broadcast("hi " .. event.player.name)
                        end)
                    end,
                }
            "#,
        );

        let (actions, mut received) = tokio_mpsc::channel(4);
        let host =
            PluginHost::with_output(config(&directory, false), Arc::new(|_, _, _| {}), actions)
                .unwrap();
        host.dispatcher().dispatch(Event::PlayerJoin(steve()));

        let mut action = None;
        wait_until("the plugin did not broadcast", || {
            action = received.try_recv().ok();
            action.is_some()
        });
        assert_eq!(action, Some(Action::Broadcast("hi Steve".into())));

        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn scheduled_tasks_run_on_the_servers_ticks() {
        let directory = plugin_directory("ticks");
        write_plugin(
            &directory,
            "timer",
            "timer",
            r#"
                local Core = require("@bedrock-rs/core")
                return {
                    on_enable = function()
                        Core.Server.getWorld():runInterval(function()
                            Core.Server.broadcast("tock")
                        end, 2)
                    end,
                }
            "#,
        );
        let (actions, mut received) = tokio_mpsc::channel(16);
        let host =
            PluginHost::with_output(config(&directory, false), Arc::new(|_, _, _| {}), actions)
                .unwrap();
        let dispatcher = host.dispatcher();
        for tick in 1..=6 {
            dispatcher.tick(tick);
        }
        let mut tocks = 0;
        wait_until("the interval did not run three times", || {
            while let Ok(action) = received.try_recv() {
                if action == Action::Broadcast("tock".into()) {
                    tocks += 1;
                }
            }
            tocks == 3
        });
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
                local Server = require("@bedrock-rs/core").Server
                return {
                    on_load = function()
                        Server.on("player_chat", function(event)
                            if event.message:find("badword") then event.cancel() end
                        end)
                    end,
                }
            "#,
        );
        let (actions, _) = tokio_mpsc::channel(4);
        let host =
            PluginHost::with_output(config(&directory, false), Arc::new(|_, _, _| {}), actions)
                .unwrap();
        let chat = |message: &str| Event::PlayerChat {
            player: steve(),
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

    /// What a plugin did in [`scenario`]: its log lines, the actions it
    /// asked for, whether it cancelled two chat messages, and a command's
    /// reply.
    #[derive(Debug, PartialEq)]
    struct Observed {
        lines: Vec<String>,
        actions: Vec<Action>,
        cancelled: (bool, bool),
        reply: Vec<(bool, String)>,
    }

    /// Runs the same story against a plugin: Steve joins, chats twice, runs
    /// `/count 5`, six ticks pass, the server stops.
    async fn scenario(test: &str, main: &str, source: &str) -> Observed {
        let directory = plugin_directory(test);
        write_plugin_file(&directory, "parity", "parity", main, source);
        let (output, messages) = recording();
        let (actions, mut received) = tokio_mpsc::channel(64);
        let host = PluginHost::with_output(config(&directory, false), output, actions).unwrap();
        assert_eq!(host.loaded(), ["parity"], "{:?}", messages.lock().unwrap());
        let dispatcher = host.dispatcher();
        dispatcher.dispatch(Event::PlayerJoin(steve()));
        let chat = |message: &str| Event::PlayerChat {
            player: steve(),
            message: message.into(),
        };
        let cancelled = (
            dispatcher.dispatch_cancellable(chat("bad")).await,
            dispatcher.dispatch_cancellable(chat("fine")).await,
        );
        let reply = dispatcher
            .run_command(CommandCall {
                plugin: "parity".into(),
                command: "count".into(),
                path: Vec::new(),
                args: vec![("n".into(), crate::ArgValue::Int(5))],
                sender: crate::CommandSender::Player(steve()),
            })
            .await
            .unwrap();
        for tick in 1..=6 {
            dispatcher.tick(tick);
        }
        drop(host);
        let mut seen = Vec::new();
        while let Ok(action) = received.try_recv() {
            if !matches!(action, Action::SetCommands(_)) {
                seen.push(action);
            }
        }
        let lines = messages.lock().unwrap().clone();
        fs::remove_dir_all(&directory).unwrap();
        Observed {
            lines,
            actions: seen,
            cancelled,
            reply: reply
                .lines
                .into_iter()
                .map(|line| (line.success, line.text))
                .collect(),
        }
    }

    const PARITY_LUAU: &str = r#"
        local Core = require("@bedrock-rs/core")
        local Logger, Server = Core.Logger, Core.Server
        local greeting = "hi"
        return {
            on_load = function()
                Logger.info("load")
                Server.registerCommand({
                    name = "count",
                    args = { { name = "n", type = "int" } },
                    run = function(ctx)
                        ctx.reply(`n={ctx.args.n} from {ctx.sender.name}`)
                        return "done"
                    end,
                })
                Server.on("player_join", function(event)
                    Logger.info(`{greeting} {event.player.name}`)
                    event.player:sendMessage("welcome")
                end)
                Server.on("player_chat", function(event)
                    if event.message == "bad" then event.cancel() end
                end)
            end,
            on_enable = function()
                local world = Server.getWorld()
                world:runInterval(function() Logger.info("every 2") end, 2)
                world:runTimeout(function() Logger.info("after 3") end, 3)
                world:run(function()
                    world:waitTicks(4)
                    Logger.info("waited 4")
                    Server.broadcast(`{#Server.getPlayers()} online`)
                end)
            end,
            on_disable = function() Logger.info("disable") end,
        }
    "#;

    const PARITY_JS: &str = r#"
        import { Logger, Server } from "@bedrock-rs/core";
        const greeting = "hi";
        export function on_load() {
            Logger.info("load");
            Server.registerCommand({
                name: "count",
                args: [{ name: "n", type: "int" }],
                run: (ctx) => {
                    ctx.reply(`n=${ctx.args.n} from ${ctx.sender.name}`);
                    return "done";
                },
            });
            Server.on("player_join", (event) => {
                Logger.info(`${greeting} ${event.player.name}`);
                event.player.sendMessage("welcome");
            });
            Server.on("player_chat", (event) => {
                if (event.message === "bad") event.cancel();
            });
        }
        export function on_enable() {
            const world = Server.getWorld();
            world.runInterval(() => Logger.info("every 2"), 2);
            world.runTimeout(() => Logger.info("after 3"), 3);
            world.run(async () => {
                await world.waitTicks(4);
                Logger.info("waited 4");
                Server.broadcast(`${Server.getPlayers().length} online`);
            });
        }
        export function on_disable() { Logger.info("disable"); }
    "#;

    #[tokio::test]
    async fn a_luau_plugin_behaves_as_expected_in_the_parity_story() {
        let observed = scenario("parity-luau", "main.luau", PARITY_LUAU).await;
        assert_eq!(
            observed.lines,
            [
                "parity: load",
                "parity: hi Steve",
                "parity: every 2",
                "parity: after 3",
                "parity: every 2",
                "parity: waited 4",
                "parity: every 2",
                "parity: disable",
            ]
        );
        assert_eq!(
            observed.actions,
            [
                Action::SendMessage {
                    player: steve().uuid,
                    message: "welcome".into()
                },
                Action::Broadcast("1 online".into()),
            ]
        );
        assert_eq!(observed.cancelled, (true, false));
        assert_eq!(
            observed.reply,
            [(true, "n=5 from Steve".into()), (true, "done".into())]
        );
    }

    /// The same story, the same plugin in JavaScript: exactly what the Luau
    /// one did.
    #[tokio::test]
    async fn a_javascript_plugin_behaves_exactly_like_its_luau_twin() {
        let luau = scenario("parity-luau-twin", "main.luau", PARITY_LUAU).await;
        let javascript = scenario("parity-js", "index.js", PARITY_JS).await;
        assert_eq!(javascript, luau);
    }

    /// Luau and JavaScript plugins load side by side, in name order, and
    /// both hear the same event.
    #[test]
    fn luau_and_javascript_plugins_run_side_by_side() {
        let directory = plugin_directory("side-by-side");
        write_plugin(
            &directory,
            "a",
            "alpha",
            r#"local Core = require("@bedrock-rs/core")
            return { on_load = function()
                Core.Server.on("player_join", function(event) print(`luau sees {event.player.name}`) end)
            end }"#,
        );
        write_plugin_file(
            &directory,
            "b",
            "beta",
            "index.js",
            r#"import { Server } from "@bedrock-rs/core";
            export function on_load() {
                Server.on("player_join", (event) => console.log(`js sees ${event.player.name}`));
            }"#,
        );
        let (output, messages) = recording();
        let (actions, _) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config(&directory, false), output, actions).unwrap();
        assert_eq!(host.loaded(), ["alpha", "beta"]);
        host.dispatcher().dispatch(Event::PlayerJoin(steve()));
        wait_until("both plugins heard the join", || {
            messages.lock().unwrap().len() == 2
        });
        assert_eq!(
            *messages.lock().unwrap(),
            ["alpha: luau sees Steve", "beta: js sees Steve"]
        );
        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    /// Saving a JavaScript module the plugin imports reloads it, once.
    #[test]
    fn saving_an_imported_javascript_module_reloads_the_plugin() {
        let directory = plugin_directory("js-reload");
        write_plugin_file(
            &directory,
            "js",
            "js",
            "index.js",
            r#"import { version } from "./version.js";
            import { Logger } from "@bedrock-rs/core";
            export function on_load() { Logger.info(version); }"#,
        );
        let version = directory.join("js").join("version.js");
        fs::write(&version, r#"export const version = "v1";"#).unwrap();
        let (output, messages) = recording();
        let (actions, _) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config(&directory, true), output, actions).unwrap();
        assert_eq!(*messages.lock().unwrap(), ["js: v1"]);
        fs::write(&version, r#"export const version = "v2";"#).unwrap();
        wait_until("the plugin was not reloaded", || {
            messages.lock().unwrap().contains(&"js: v2".to_owned())
        });
        thread::sleep(RELOAD_DEBOUNCE * 3);
        assert_eq!(*messages.lock().unwrap(), ["js: v1", "js: v2"]);
        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_javascript_plugin_with_a_syntax_error_is_skipped_not_fatal() {
        let directory = plugin_directory("js-broken");
        write_plugin_file(
            &directory,
            "broken",
            "broken",
            "index.js",
            "export function on_load( {",
        );
        write_plugin(&directory, "fine", "fine", &printing("still here"));
        let (output, messages) = recording();
        let (actions, _) = tokio_mpsc::channel(4);
        let host = PluginHost::with_output(config(&directory, false), output, actions).unwrap();
        // The JavaScript plugin is left out (its SyntaxError is logged), and
        // the other one runs.
        assert_eq!(host.loaded(), ["fine"]);
        assert_eq!(*messages.lock().unwrap(), ["fine: still here"]);
        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    /// A broken plugin is tried again when its own files change, not on
    /// every rescan: saving another plugin neither reruns it nor logs its
    /// error again.
    #[test]
    fn a_broken_plugin_is_retried_only_when_it_changes() {
        let directory = plugin_directory("broken-retry");
        // Each attempt says so through an action, which (unlike output)
        // reaches the server even though the load then fails.
        let broken = |attempt: u32| {
            format!(
                r#"local Server = require("@bedrock-rs/core").Server
                return {{ on_load = function()
                    Server.broadcast("attempt {attempt}")
                    error("broken")
                end }}"#
            )
        };
        write_plugin(&directory, "broken", "broken", &broken(1));
        write_plugin(&directory, "fine", "fine", &printing("v1"));
        let (output, messages) = recording();
        let (actions, mut received) = tokio_mpsc::channel(64);
        let host = PluginHost::with_output(config(&directory, true), output, actions).unwrap();
        assert_eq!(host.loaded(), ["fine"]);
        let mut attempts = Vec::new();
        let mut collect = |attempts: &mut Vec<String>| {
            while let Ok(action) = received.try_recv() {
                if let Action::Broadcast(message) = action {
                    attempts.push(message);
                }
            }
        };

        // Another plugin changing rescans everything, but the broken one is
        // as it was, so it is left alone.
        let fine = directory.join("fine").join("main.luau");
        fs::write(&fine, printing("v2")).unwrap();
        wait_until("the fine plugin was not reloaded", || {
            messages.lock().unwrap().contains(&"fine: v2".to_owned())
        });
        thread::sleep(RELOAD_DEBOUNCE * 3);
        collect(&mut attempts);
        assert_eq!(attempts, ["attempt 1"]);

        // Saving the broken plugin tries it again.
        let script = directory.join("broken").join("main.luau");
        fs::write(&script, broken(2)).unwrap();
        wait_until("the broken plugin was not retried", || {
            collect(&mut attempts);
            attempts.len() == 2
        });
        assert_eq!(attempts, ["attempt 1", "attempt 2"]);

        // Fixed, it loads.
        fs::write(&script, printing("fixed")).unwrap();
        wait_until("the fixed plugin did not load", || {
            messages
                .lock()
                .unwrap()
                .contains(&"broken: fixed".to_owned())
        });
        drop(host);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn only_real_changes_trigger_a_rescan() {
        use notify::event::{CreateKind, DataChange, ModifyKind, RenameMode};

        let directory = plugin_directory("watch-events");
        let folder = directory.join("plugin");
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("main.luau");
        fs::write(&file, "").unwrap();
        let event =
            |kind: EventKind, path: &Path| notify::Event::new(kind).add_path(path.to_owned());

        // What Windows reports when a fresh folder is merely listed.
        assert!(!is_change(&event(
            EventKind::Modify(ModifyKind::Any),
            &folder
        )));
        assert!(is_change(&event(EventKind::Modify(ModifyKind::Any), &file)));
        assert!(is_change(&event(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            &file
        )));
        // Folders still count when they appear, go or are renamed.
        assert!(is_change(&event(
            EventKind::Create(CreateKind::Folder),
            &folder
        )));
        assert!(is_change(&event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &folder
        )));
        // A path that is gone by now is a removed file, not a folder.
        assert!(is_change(&event(
            EventKind::Modify(ModifyKind::Any),
            &folder.join("gone.luau")
        )));
        assert!(!is_change(&event(
            EventKind::Access(notify::event::AccessKind::Any),
            &file
        )));
        fs::remove_dir_all(&directory).unwrap();
    }
}
