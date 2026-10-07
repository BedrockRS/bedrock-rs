//! Luau plugins, each running in its own sandboxed VM.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Function, Lua, MultiValue, Table, Value, VmState};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::Level;

use crate::luau_commands::{self, Registered};
use crate::{
    Action, CommandCall, CommandReply, DAMAGE_CAUSES, Event, GAME_MODE_VALUES, Output, Player,
    PluginCommand,
};

/// Registry key of each VM's table of event handlers: event name → list of functions.
const HANDLERS: &str = "bedrockrs.handlers";

/// What a kicked player is shown when the plugin gives no reason.
const DEFAULT_KICK_REASON: &str = "You were kicked from the server.";

/// Resource limits applied to every plugin VM.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub memory: usize,
    /// Longest a single call into a plugin may run.
    pub execution: Duration,
}

/// The players plugins know are online, by UUID: those whose `player_join`
/// plugins heard and whose `player_quit` they have not. Shared by every VM.
#[derive(Clone, Default)]
struct Roster(Rc<RefCell<BTreeMap<String, Player>>>);

/// Runs Luau plugins by name. Lives on one thread, since Luau VMs are not `Send`.
pub(crate) struct LuauEngine {
    limits: Limits,
    output: Output,
    actions: mpsc::Sender<Action>,
    plugins: BTreeMap<String, Plugin>,
    roster: Roster,
}

struct Plugin {
    lua: Lua,
    /// The slash commands the plugin registered.
    commands: Registered,
    /// When the running call must stop; checked by the VM's interrupt callback.
    deadline: Rc<Cell<Option<Instant>>>,
}

impl LuauEngine {
    pub fn new(limits: Limits, output: Output, actions: mpsc::Sender<Action>) -> Self {
        Self {
            limits,
            output,
            actions,
            plugins: BTreeMap::new(),
            roster: Roster::default(),
        }
    }

    /// Runs `source` as plugin `name` in a fresh VM. The new VM replaces a running
    /// plugin of the same name only if the script succeeds. Returns whether it did.
    pub fn load(&mut self, name: &str, chunk_name: &str, source: &str) -> mlua::Result<bool> {
        let plugin = self.create_plugin(name)?;
        plugin.run(self.limits.execution, || {
            plugin
                .lua
                .load(source)
                .set_name(format!("@{chunk_name}"))
                .exec()
        })?;
        Ok(self.plugins.insert(name.to_owned(), plugin).is_some())
    }

    /// Stops plugin `name`, dropping its VM. Returns whether it was running.
    pub fn unload(&mut self, name: &str) -> bool {
        self.plugins.remove(name).is_some()
    }

    pub fn names(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Every command the running plugins registered, plugin by plugin in
    /// name order.
    pub fn commands(&self) -> Vec<PluginCommand> {
        self.plugins
            .iter()
            .flat_map(|(name, plugin)| {
                plugin
                    .commands
                    .borrow()
                    .iter()
                    .map(|spec| PluginCommand {
                        plugin: name.clone(),
                        spec: spec.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Runs the handler of a plugin command within the execution limit.
    pub fn run_command(&self, call: &CommandCall) -> CommandReply {
        let Some(plugin) = self.plugins.get(&call.plugin) else {
            return CommandReply::error(format!("/{} is no longer available.", call.command));
        };
        plugin
            .run(self.limits.execution, || luau_commands::run(&plugin.lua, call))
            .unwrap_or_else(|err| {
                tracing::error!(plugin = %call.plugin, command = %call.command, "command failed: {err}");
                CommandReply::error("An error occurred while running this command.")
            })
    }

    /// Calls every handler for `event`, plugin by plugin in name order. A
    /// handler that fails is logged and skipped. Returns whether a handler
    /// cancelled the event; later handlers still run and can check.
    ///
    /// A joining player can be looked up from `player_join` on, and a leaving
    /// one until the `player_quit` handlers have run.
    pub fn dispatch(&self, event: &Event) -> bool {
        if let Event::PlayerJoin(player) = event {
            self.roster
                .0
                .borrow_mut()
                .insert(player.uuid.clone(), player.clone());
        }
        let cancelled = Rc::new(Cell::new(false));
        for (name, plugin) in &self.plugins {
            plugin.dispatch(name, event, &cancelled, self.limits.execution);
        }
        if let Event::PlayerQuit(player) = event {
            self.roster.0.borrow_mut().remove(&player.uuid);
        }
        cancelled.get()
    }

    fn create_plugin(&self, name: &str) -> mlua::Result<Plugin> {
        let lua = Lua::new();
        lua.set_memory_limit(self.limits.memory)?;
        // Globals become read-only once sandboxed, so install ours first.
        install_output(&lua, name, &self.output)?;
        let commands = Registered::default();
        install_server(&lua, &self.actions, &self.roster, &commands)?;
        lua.sandbox(true)?;

        let deadline = Rc::new(Cell::new(None::<Instant>));
        let expiry = Rc::clone(&deadline);
        lua.set_interrupt(move |_| match expiry.get() {
            Some(deadline) if Instant::now() >= deadline => Err(mlua::Error::runtime(
                "plugin exceeded its execution time limit",
            )),
            _ => Ok(VmState::Continue),
        });
        Ok(Plugin {
            lua,
            commands,
            deadline,
        })
    }
}

impl Plugin {
    fn run<T>(&self, limit: Duration, call: impl FnOnce() -> mlua::Result<T>) -> mlua::Result<T> {
        self.deadline.set(Some(Instant::now() + limit));
        let result = call();
        self.deadline.set(None);
        result
    }

    /// Calls this plugin's handlers for `event` in the order they were
    /// registered, each within the execution limit.
    fn dispatch(&self, plugin: &str, event: &Event, cancelled: &Rc<Cell<bool>>, limit: Duration) {
        let handlers = match self.handlers(event.name()) {
            Ok(handlers) => handlers,
            Err(err) => {
                tracing::error!(
                    plugin,
                    event = event.name(),
                    "failed to look up handlers: {err}"
                );
                return;
            }
        };
        if handlers.is_empty() {
            return;
        }
        let payload = match event_payload(&self.lua, event, cancelled) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::error!(
                    plugin,
                    event = event.name(),
                    "failed to build the event: {err}"
                );
                return;
            }
        };
        for handler in handlers {
            if let Err(err) = self.run(limit, || handler.call::<()>(&payload)) {
                tracing::error!(plugin, event = event.name(), "event handler failed: {err}");
            }
        }
    }

    /// A snapshot of the handlers for `event`, so handlers may register more
    /// while being called.
    fn handlers(&self, event: &str) -> mlua::Result<Vec<Function>> {
        let handlers: Table = self.lua.named_registry_value(HANDLERS)?;
        match handlers.raw_get::<Option<Table>>(event)? {
            Some(list) => list.sequence_values().collect(),
            None => Ok(Vec::new()),
        }
    }
}

/// The value handlers receive: a read-only table describing the event.
///
/// - `player_join`, `player_quit`: `{ player, cancel(), is_cancelled() }`;
///   cancelling stops vanilla's join or quit message.
/// - `player_chat`: `{ player, message, cancel(), is_cancelled() }`.
/// - `block_break`, `block_place`: `{ player, position = { x, y, z }, block }`.
/// - `player_damage`: `{ player, cause, amount, health, cancel(), is_cancelled() }`.
/// - `player_death`: `{ player, cause, message }`.
/// - `player_respawn`: the player.
fn event_payload(lua: &Lua, event: &Event, cancelled: &Rc<Cell<bool>>) -> mlua::Result<Table> {
    let payload = match event {
        Event::PlayerJoin(player) | Event::PlayerQuit(player) => {
            let table = lua.create_table()?;
            table.raw_set("player", player_table(lua, player)?)?;
            table
        }
        Event::PlayerRespawn(player) => player_table(lua, player)?,
        Event::PlayerDamage(damage) => {
            let table = lua.create_table()?;
            table.raw_set("player", player_table(lua, &damage.player)?)?;
            table.raw_set("cause", damage.cause.as_str())?;
            table.raw_set("amount", damage.amount)?;
            table.raw_set("health", damage.health)?;
            table
        }
        Event::PlayerDeath {
            player,
            cause,
            message,
        } => {
            let table = lua.create_table()?;
            table.raw_set("player", player_table(lua, player)?)?;
            table.raw_set("cause", cause.as_str())?;
            table.raw_set("message", message.as_str())?;
            table
        }
        Event::PlayerChat { player, message } => {
            let table = lua.create_table()?;
            table.raw_set("player", player_table(lua, player)?)?;
            table.raw_set("message", message.as_str())?;
            table
        }
        Event::BlockBreak(change) | Event::BlockPlace(change) => {
            let position = lua.create_table()?;
            position.raw_set("x", change.position.x)?;
            position.raw_set("y", change.position.y)?;
            position.raw_set("z", change.position.z)?;
            position.set_readonly(true);
            let table = lua.create_table()?;
            table.raw_set("player", player_table(lua, &change.player)?)?;
            table.raw_set("position", position)?;
            table.raw_set("block", change.block.as_str())?;
            table
        }
    };
    if event.is_cancellable() {
        let cancel = Rc::clone(cancelled);
        payload.raw_set(
            "cancel",
            lua.create_function(move |_, ()| {
                cancel.set(true);
                Ok(())
            })?,
        )?;
        let cancel = Rc::clone(cancelled);
        payload.raw_set(
            "is_cancelled",
            lua.create_function(move |_, ()| Ok(cancel.get()))?,
        )?;
    }
    payload.set_readonly(true);
    Ok(payload)
}

/// A player as handlers see them: `{ name, uuid, send_message(message),
/// set_game_mode(mode), set_health(health), damage(amount, cause?),
/// kick(reason?) }`. The methods work with `.` and `:` alike, and keep working
/// after the event; acting on a player who has left does nothing.
pub(crate) fn player_table(lua: &Lua, player: &Player) -> mlua::Result<Table> {
    let actions = lua
        .app_data_ref::<mpsc::Sender<Action>>()
        .ok_or_else(|| mlua::Error::runtime("the plugin API is not installed"))?
        .clone();
    let table = lua.create_table()?;
    table.raw_set("name", player.name.as_str())?;
    table.raw_set("uuid", player.uuid.as_str())?;

    let (queue, uuid) = (actions.clone(), player.uuid.clone());
    let send_message = lua.create_function(move |_, args: MultiValue| {
        let message = match method_args(args).next() {
            Some(Value::String(message)) => message.to_str()?.to_owned(),
            _ => {
                return Err(mlua::Error::runtime(
                    "send_message expects a message string",
                ));
            }
        };
        if message.is_empty() {
            return Err(mlua::Error::runtime("cannot send an empty message"));
        }
        request(
            &queue,
            Action::SendMessage {
                player: uuid.clone(),
                message,
            },
        )
    })?;
    table.raw_set("send_message", send_message)?;

    let (queue, uuid) = (actions.clone(), player.uuid.clone());
    let set_game_mode = lua.create_function(move |_, args: MultiValue| {
        let mode = match method_args(args).next() {
            Some(Value::String(mode)) => mode.to_str()?.to_ascii_lowercase(),
            _ => {
                return Err(mlua::Error::runtime(
                    "set_game_mode expects a game mode name",
                ));
            }
        };
        if !GAME_MODE_VALUES.contains(&mode.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "unknown game mode {mode:?}; expected survival, creative, adventure or spectator"
            )));
        }
        request(
            &queue,
            Action::SetGameMode {
                player: uuid.clone(),
                mode,
            },
        )
    })?;
    table.raw_set("set_game_mode", set_game_mode)?;

    let (queue, uuid) = (actions.clone(), player.uuid.clone());
    let set_health = lua.create_function(move |_, args: MultiValue| {
        let health = match method_args(args).next() {
            Some(Value::Integer(health)) => health as f32,
            Some(Value::Number(health)) if health.is_finite() => health as f32,
            _ => return Err(mlua::Error::runtime("set_health expects a number")),
        };
        request(
            &queue,
            Action::SetHealth {
                player: uuid.clone(),
                health,
            },
        )
    })?;
    table.raw_set("set_health", set_health)?;

    let (queue, uuid) = (actions.clone(), player.uuid.clone());
    let damage = lua.create_function(move |_, args: MultiValue| {
        let mut args = method_args(args);
        let amount = match args.next() {
            Some(Value::Integer(amount)) if amount > 0 => amount as f32,
            Some(Value::Number(amount)) if amount.is_finite() && amount > 0.0 => amount as f32,
            _ => return Err(mlua::Error::runtime("damage expects an amount above 0")),
        };
        let cause = match args.next() {
            None | Some(Value::Nil) => "none".to_owned(),
            Some(Value::String(cause)) => cause.to_str()?.to_owned(),
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "damage expects a cause string, got a {}",
                    other.type_name()
                )));
            }
        };
        if !DAMAGE_CAUSES.contains(&cause.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "unknown damage cause {cause:?}; expected a vanilla cause such as fall, void or entityAttack"
            )));
        }
        request(
            &queue,
            Action::Damage {
                player: uuid.clone(),
                amount,
                cause,
            },
        )
    })?;
    table.raw_set("damage", damage)?;

    let uuid = player.uuid.clone();
    let kick = lua.create_function(move |_, args: MultiValue| {
        let reason = match method_args(args).next() {
            Some(Value::String(reason)) if !reason.as_bytes().is_empty() => {
                reason.to_str()?.to_owned()
            }
            None | Some(Value::Nil | Value::String(_)) => DEFAULT_KICK_REASON.to_owned(),
            Some(other) => {
                return Err(mlua::Error::runtime(format!(
                    "kick expects a reason string, got a {}",
                    other.type_name()
                )));
            }
        };
        request(
            &actions,
            Action::Kick {
                player: uuid.clone(),
                reason,
            },
        )
    })?;
    table.raw_set("kick", kick)?;

    table.set_readonly(true);
    Ok(table)
}

/// A method's arguments without `self`, so `player:kick()` works like
/// `player.kick()`. A player method never takes a table otherwise.
pub(crate) fn method_args(args: MultiValue) -> impl Iterator<Item = Value> {
    let mut args = args.into_iter().peekable();
    if matches!(args.peek(), Some(Value::Table(_))) {
        args.next();
    }
    args
}

/// Queues `action` for the server.
pub(crate) fn request(actions: &mpsc::Sender<Action>, action: Action) -> mlua::Result<()> {
    match actions.try_send(action) {
        // A closed channel means the server is shutting down.
        Ok(()) | Err(TrySendError::Closed(_)) => Ok(()),
        Err(TrySendError::Full(_)) => Err(mlua::Error::runtime(
            "the server is not keeping up with plugin actions",
        )),
    }
}

/// Adds the `server` table:
/// - `server.on(event, handler)` registers an event handler;
/// - `server.broadcast(message)` sends a chat message to everyone;
/// - `server.player(uuid)` is the online player with that UUID, or `nil`;
/// - `server.command(definition)` adds a slash command (see [`luau_commands`]).
///
/// Actions on one player are methods of player tables.
fn install_server(
    lua: &Lua,
    actions: &mpsc::Sender<Action>,
    roster: &Roster,
    commands: &Registered,
) -> mlua::Result<()> {
    lua.set_named_registry_value(HANDLERS, lua.create_table()?)?;
    // Player tables built for events queue their actions here.
    lua.set_app_data(actions.clone());
    let server = lua.create_table()?;

    let on = lua.create_function(|lua, (event, handler): (String, Function)| {
        if !Event::NAMES.contains(&event.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "unknown event {event:?}; expected one of: {}",
                Event::NAMES.join(", ")
            )));
        }
        let handlers: Table = lua.named_registry_value(HANDLERS)?;
        let list = match handlers.raw_get::<Option<Table>>(event.as_str())? {
            Some(list) => list,
            None => {
                let list = lua.create_table()?;
                handlers.raw_set(event, &list)?;
                list
            }
        };
        list.raw_push(handler)
    })?;
    server.set("on", on)?;

    let queue = actions.clone();
    let broadcast = lua.create_function(move |_, message: String| {
        if message.is_empty() {
            return Err(mlua::Error::runtime("cannot broadcast an empty message"));
        }
        request(&queue, Action::Broadcast(message))
    })?;
    server.set("broadcast", broadcast)?;

    let roster = roster.clone();
    let player = lua.create_function(move |lua, uuid: String| {
        // UUIDs are written in lower case; accept any case.
        let found = roster.0.borrow().get(&uuid.to_ascii_lowercase()).cloned();
        found.map(|player| player_table(lua, &player)).transpose()
    })?;
    server.set("player", player)?;
    luau_commands::install(lua, &server, commands)?;

    lua.globals().set("server", server)
}

/// Replaces `print` and adds a `log` table (`log.trace` … `log.error`) that
/// forward to `output`, formatting arguments like Luau's `print`.
fn install_output(lua: &Lua, plugin: &str, output: &Output) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set("print", output_function(lua, plugin, output, Level::INFO)?)?;

    let log = lua.create_table()?;
    for (name, level) in [
        ("trace", Level::TRACE),
        ("debug", Level::DEBUG),
        ("info", Level::INFO),
        ("warn", Level::WARN),
        ("error", Level::ERROR),
    ] {
        log.set(name, output_function(lua, plugin, output, level)?)?;
    }
    globals.set("log", log)
}

fn output_function(
    lua: &Lua,
    plugin: &str,
    output: &Output,
    level: Level,
) -> mlua::Result<Function> {
    let plugin = plugin.to_owned();
    let output = Arc::clone(output);
    lua.create_function(move |_, args: MultiValue| {
        let message = args
            .iter()
            .map(Value::to_string)
            .collect::<mlua::Result<Vec<_>>>()?
            .join("\t");
        output(&plugin, level, &message);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    type Lines = Arc<Mutex<Vec<(String, Level, String)>>>;

    const DEFAULT_LIMITS: Limits = Limits {
        memory: 16 * 1024 * 1024,
        execution: Duration::from_millis(250),
    };

    fn engine_with_actions(limits: Limits) -> (LuauEngine, Lines, mpsc::Receiver<Action>) {
        let lines = Lines::default();
        let sink = Arc::clone(&lines);
        let output: Output = Arc::new(move |plugin, level, message| {
            sink.lock()
                .unwrap()
                .push((plugin.to_owned(), level, message.to_owned()));
        });
        let (actions, received) = mpsc::channel(16);
        (LuauEngine::new(limits, output, actions), lines, received)
    }

    fn engine(limits: Limits) -> (LuauEngine, Lines) {
        let (engine, lines, _) = engine_with_actions(limits);
        (engine, lines)
    }

    fn default_engine() -> (LuauEngine, Lines) {
        engine(DEFAULT_LIMITS)
    }

    fn steve_joins() -> Event {
        Event::PlayerJoin(crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        })
    }

    fn messages(lines: &Lines) -> Vec<String> {
        lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, m)| m.clone())
            .collect()
    }

    #[test]
    fn print_formats_arguments_like_luau() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "hello",
                "hello.luau",
                r#"
                    print("hi", 42, 1.5, true, nil)
                    print(setmetatable({}, { __tostring = function() return "custom" end }))
                "#,
            )
            .unwrap();
        assert_eq!(
            *lines.lock().unwrap(),
            vec![
                ("hello".into(), Level::INFO, "hi\t42\t1.5\ttrue\tnil".into()),
                ("hello".into(), Level::INFO, "custom".into()),
            ]
        );
    }

    #[test]
    fn log_functions_use_their_levels() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "levels",
                "levels.luau",
                r#"log.warn("careful") log.error("broken", 1)"#,
            )
            .unwrap();
        let levels: Vec<_> = lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, l, m)| (*l, m.clone()))
            .collect();
        assert_eq!(
            levels,
            vec![
                (Level::WARN, "careful".into()),
                (Level::ERROR, "broken\t1".into())
            ]
        );
    }

    #[test]
    fn luau_syntax_and_type_annotations_are_supported() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "typed",
                "typed.luau",
                "local count: number = 2\ncount += 1\nprint(`count is {count}`)",
            )
            .unwrap();
        assert_eq!(messages(&lines), vec!["count is 3"]);
    }

    #[test]
    fn sandbox_makes_libraries_read_only() {
        let (mut engine, _) = default_engine();
        let err = engine
            .load("vandal", "vandal.luau", "string.upper = nil")
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
    }

    #[test]
    fn plugins_do_not_share_globals() {
        let (mut engine, lines) = default_engine();
        engine.load("a", "a.luau", r#"shared = "from a""#).unwrap();
        engine.load("b", "b.luau", "print(shared)").unwrap();
        assert_eq!(messages(&lines), vec!["nil"]);
    }

    #[test]
    fn runaway_scripts_are_stopped() {
        let (mut engine, _) = default_engine();
        let started = Instant::now();
        let err = engine
            .load("spin", "spin.luau", "while true do end")
            .unwrap_err();
        assert!(err.to_string().contains("execution time limit"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(engine.names().is_empty());
    }

    #[test]
    fn memory_limit_is_enforced() {
        let (mut engine, _) = engine(Limits {
            memory: 4 * 1024 * 1024,
            execution: Duration::from_secs(5),
        });
        let err = engine
            .load(
                "hog",
                "hog.luau",
                "local t = {} for i = 1, 1e7 do t[i] = string.rep('x', 64) .. i end",
            )
            .unwrap_err();
        assert!(matches!(err, mlua::Error::MemoryError(_)), "{err}");
    }

    #[test]
    fn failed_reload_keeps_the_running_version() {
        let (mut engine, _) = default_engine();
        assert!(!engine.load("hello", "hello.luau", "print('v1')").unwrap());
        assert!(engine.load("hello", "hello.luau", "print(").is_err());
        assert_eq!(engine.names(), vec!["hello"]);
        assert!(engine.load("hello", "hello.luau", "print('v2')").unwrap());
        assert!(engine.unload("hello"));
        assert!(!engine.unload("hello"));
    }

    #[test]
    fn join_handlers_get_the_player_and_can_broadcast() {
        let (mut engine, lines, mut actions) = engine_with_actions(DEFAULT_LIMITS);
        engine
            .load(
                "welcome",
                "welcome.luau",
                r#"
                    server.on("player_join", function(event) local player = event.player
                        print(player.name, player.uuid)
                        server.broadcast(`Welcome, {player.name}!`)
                    end)
                "#,
            )
            .unwrap();
        assert!(
            actions.try_recv().is_err(),
            "nothing happens until someone joins"
        );

        engine.dispatch(&steve_joins());
        assert_eq!(
            messages(&lines),
            ["Steve\t174319cc-f69f-30d8-a279-6ace57f2011e"]
        );
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::Broadcast("Welcome, Steve!".into())
        );
    }

    #[test]
    fn unknown_events_are_rejected() {
        let (mut engine, _) = default_engine();
        let err = engine
            .load(
                "typo",
                "typo.luau",
                r#"server.on("player_joined", function() end)"#,
            )
            .unwrap_err();
        assert!(err.to_string().contains("unknown event"), "{err}");
    }

    #[test]
    fn a_failing_handler_does_not_stop_the_others() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "a",
                "a.luau",
                r#"
                    server.on("player_join", function(event) local player = event.player player.name = "Alex" end)
                    server.on("player_join", function(event) local player = event.player print("a saw", player.name) end)
                "#,
            )
            .unwrap();
        engine
            .load(
                "b",
                "b.luau",
                r#"server.on("player_join", function(event) local player = event.player print("b saw", player.name) end)"#,
            )
            .unwrap();
        engine.dispatch(&steve_joins());
        assert_eq!(messages(&lines), ["a saw\tSteve", "b saw\tSteve"]);
    }

    #[test]
    fn runaway_handlers_are_stopped() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "spin",
                "spin.luau",
                r#"
                    server.on("player_join", function() while true do end end)
                    server.on("player_join", function() print("still here") end)
                "#,
            )
            .unwrap();
        let started = Instant::now();
        engine.dispatch(&steve_joins());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(messages(&lines), ["still here"]);
    }

    #[test]
    fn reloading_replaces_the_handlers() {
        let (mut engine, lines) = default_engine();
        let source = |version: &str| {
            format!(r#"server.on("player_join", function() print("{version}") end)"#)
        };
        engine.load("hello", "hello.luau", &source("v1")).unwrap();
        engine.load("hello", "hello.luau", &source("v2")).unwrap();
        engine.dispatch(&steve_joins());
        assert_eq!(messages(&lines), ["v2"]);
    }

    #[test]
    fn broadcast_rejects_empty_messages_and_the_api_is_read_only() {
        let (mut engine, _) = default_engine();
        let err = engine
            .load("empty", "empty.luau", r#"server.broadcast("")"#)
            .unwrap_err();
        assert!(err.to_string().contains("empty message"), "{err}");
        let err = engine
            .load("vandal", "vandal.luau", "server.broadcast = nil")
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
    }

    fn steve() -> crate::Player {
        crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        }
    }

    #[test]
    fn chat_can_be_cancelled_and_later_handlers_see_it() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "filter",
                "filter.luau",
                r#"
                    server.on("player_chat", function(event)
                        if event.message:find("spam") then event.cancel() end
                    end)
                "#,
            )
            .unwrap();
        engine
            .load(
                "logger",
                "logger.luau",
                r#"
                    server.on("player_chat", function(event)
                        print(event.player.name, event.message, event.is_cancelled())
                    end)
                "#,
            )
            .unwrap();
        let chat = |message: &str| Event::PlayerChat {
            player: steve(),
            message: message.into(),
        };
        assert!(engine.dispatch(&chat("buy spam")));
        assert!(!engine.dispatch(&chat("hello")));
        assert_eq!(
            messages(&lines),
            ["Steve\tbuy spam\ttrue", "Steve\thello\tfalse"]
        );
    }

    #[test]
    fn quit_and_block_events_describe_what_happened() {
        let (mut engine, lines) = default_engine();
        engine
            .load(
                "watch",
                "watch.luau",
                r#"
                    server.on("player_quit", function(event) local player = event.player print("quit", player.name) end)
                    local function changed(event)
                        local at = event.position
                        print(event.player.name, event.block, at.x, at.y, at.z, event.cancel)
                    end
                    server.on("block_break", changed)
                    server.on("block_place", changed)
                "#,
            )
            .unwrap();
        let change = |block: &str| crate::BlockChange {
            player: steve(),
            position: crate::Position {
                x: 1,
                y: -60,
                z: -2,
            },
            block: block.into(),
        };
        assert!(!engine.dispatch(&Event::BlockBreak(change("minecraft:grass_block"))));
        engine.dispatch(&Event::BlockPlace(change("minecraft:stone")));
        engine.dispatch(&Event::PlayerQuit(steve()));
        assert_eq!(
            messages(&lines),
            [
                "Steve\tminecraft:grass_block\t1\t-60\t-2\tnil",
                "Steve\tminecraft:stone\t1\t-60\t-2\tnil",
                "quit\tSteve",
            ]
        );
    }

    #[test]
    fn players_can_be_messaged_and_kicked_through_their_methods() {
        let (mut engine, _, mut actions) = engine_with_actions(DEFAULT_LIMITS);
        engine
            .load(
                "moderator",
                "moderator.luau",
                r#"
                    server.on("player_join", function(event) local player = event.player
                        player.send_message("Only you can see this")
                        player:send_message("And this")
                    end)
                    server.on("player_chat", function(event)
                        event.player.kick("Come back later")
                        event.player:kick()
                    end)
                "#,
            )
            .unwrap();
        engine.dispatch(&steve_joins());
        engine.dispatch(&Event::PlayerChat {
            player: steve(),
            message: "hi".into(),
        });
        let uuid = steve().uuid;
        let received: Vec<_> = std::iter::from_fn(|| actions.try_recv().ok()).collect();
        assert_eq!(
            received,
            [
                Action::SendMessage {
                    player: uuid.clone(),
                    message: "Only you can see this".into()
                },
                Action::SendMessage {
                    player: uuid.clone(),
                    message: "And this".into()
                },
                Action::Kick {
                    player: uuid.clone(),
                    reason: "Come back later".into()
                },
                Action::Kick {
                    player: uuid,
                    reason: DEFAULT_KICK_REASON.into()
                },
            ]
        );
    }

    #[test]
    fn player_methods_check_their_arguments_and_players_are_read_only() {
        let (mut engine, lines, mut actions) = engine_with_actions(DEFAULT_LIMITS);
        engine
            .load(
                "careless",
                "careless.luau",
                r#"
                    server.on("player_join", function(event) local player = event.player
                        for _, attempt in {
                            function() player.send_message() end,
                            function() player.send_message("") end,
                            function() player.send_message(42) end,
                            function() player.kick(false) end,
                            function() player.name = "Alex" end,
                            function() player.kick = nil end,
                        } do
                            print(pcall(attempt))
                        end
                    end)
                "#,
            )
            .unwrap();
        engine.dispatch(&steve_joins());
        let results = messages(&lines);
        assert_eq!(results.len(), 6);
        assert!(
            results.iter().all(|line| line.starts_with("false")),
            "{results:?}"
        );
        assert!(actions.try_recv().is_err(), "nothing was sent");
    }

    #[test]
    fn online_players_can_be_looked_up_by_uuid() {
        let (mut engine, lines, mut actions) = engine_with_actions(DEFAULT_LIMITS);
        engine
            .load(
                "lookup",
                "lookup.luau",
                r#"
                    local uuid = "174319CC-F69F-30D8-A279-6ACE57F2011E"
                    local function show(when)
                        local player = server.player(uuid)
                        print(when, player and player.name)
                    end
                    show("before")
                    server.on("player_join", function() show("join") end)
                    server.on("player_quit", function() show("quit") end)
                    server.on("block_break", function()
                        show("later")
                        local player = server.player(uuid)
                        if player then player.send_message("found you") end
                    end)
                "#,
            )
            .unwrap();
        let block_break = || {
            Event::BlockBreak(crate::BlockChange {
                player: steve(),
                position: crate::Position { x: 0, y: 0, z: 0 },
                block: "minecraft:dirt".into(),
            })
        };
        engine.dispatch(&steve_joins());
        engine.dispatch(&block_break());
        engine.dispatch(&Event::PlayerQuit(steve()));
        engine.dispatch(&block_break());
        assert_eq!(
            messages(&lines),
            [
                "before\tnil",
                "join\tSteve",
                "later\tSteve",
                "quit\tSteve",
                "later\tnil"
            ]
        );
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::SendMessage {
                player: steve().uuid,
                message: "found you".into()
            }
        );
        assert!(actions.try_recv().is_err());

        // A plugin loaded later sees who is already online.
        engine.dispatch(&steve_joins());
        engine
            .load(
                "late",
                "late.luau",
                r#"print(server.player("174319cc-f69f-30d8-a279-6ace57f2011e").name)"#,
            )
            .unwrap();
        assert_eq!(messages(&lines).last().unwrap(), "Steve");
    }

    #[test]
    fn damage_can_be_cancelled_and_health_changed() {
        let (mut engine, lines, mut actions) = engine_with_actions(DEFAULT_LIMITS);
        engine
            .load(
                "medic",
                "medic.luau",
                r#"
                    server.on("player_damage", function(event)
                        print(`{event.cause} {event.amount} of {event.health}`)
                        if event.cause == "fall" then event.cancel() end
                    end)
                    server.on("player_respawn", function(player)
                        player:set_health(10)
                        player.damage(2.5, "magic")
                        player.damage(1)
                    end)
                    server.on("player_death", function(event)
                        print(`{event.cause}: {event.message}`)
                        event.player.damage(1, "laser")
                    end)
                "#,
            )
            .unwrap();
        let steve = crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        };
        let damage = |cause: &str| {
            Event::PlayerDamage(crate::Damage {
                player: steve.clone(),
                cause: cause.into(),
                amount: 4.0,
                health: 20.0,
            })
        };
        assert!(engine.dispatch(&damage("fall")));
        assert!(!engine.dispatch(&damage("void")));

        engine.dispatch(&Event::PlayerRespawn(steve.clone()));
        let uuid = steve.uuid.clone();
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::SetHealth {
                player: uuid.clone(),
                health: 10.0
            }
        );
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::Damage {
                player: uuid.clone(),
                amount: 2.5,
                cause: "magic".into()
            }
        );
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::Damage {
                player: uuid,
                amount: 1.0,
                cause: "none".into()
            },
            "damage without a cause"
        );

        // Unknown causes are errors, so nothing is sent.
        engine.dispatch(&Event::PlayerDeath {
            player: steve.clone(),
            cause: "void".into(),
            message: "Steve fell out of the world".into(),
        });
        assert!(actions.try_recv().is_err());
        assert_eq!(
            messages(&lines),
            [
                "fall 4 of 20",
                "void 4 of 20",
                "void: Steve fell out of the world"
            ]
        );
    }

    #[test]
    fn joins_and_quits_can_replace_the_vanilla_message() {
        let (mut engine, _, mut actions) = engine_with_actions(DEFAULT_LIMITS);
        engine
            .load(
                "greeter",
                "greeter.luau",
                r#"
                    server.on("player_join", function(event)
                        event.cancel()
                        server.broadcast(`+ {event.player.name}`)
                    end)
                    server.on("player_quit", function(event)
                        if event.is_cancelled() then error("not cancelled yet") end
                    end)
                "#,
            )
            .unwrap();
        assert!(
            engine.dispatch(&steve_joins()),
            "the vanilla message is cancelled"
        );
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::Broadcast("+ Steve".into())
        );
        let quit = Event::PlayerQuit(crate::Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        });
        assert!(!engine.dispatch(&quit), "the vanilla message stays");
    }
}
