//! [`LuauPlugin`]: a plugin written in Luau, each in its own sandboxed VM.
//!
//! The script imports the API as JavaScript does, and returns its lifecycle
//! functions:
//!
//! ```lua
//! local Core = require("@bedrock-rs/core")
//! local Logger, Server = Core.Logger, Core.Server
//!
//! local plugin = {}
//! function plugin.on_load() Server.registerCommand({ ... }) end
//! function plugin.on_enable()
//!     Server.getWorld():runInterval(function() Logger.info("tick tock") end, 20 * 60)
//! end
//! function plugin.on_disable() end
//! return plugin
//! ```
//!
//! The API is the JavaScript one with Luau's method calls (`world:runTimeout`
//! where JavaScript writes `world.runTimeout`): the same names, arguments,
//! rules and errors, since every call goes through [`crate::state`].
//!
//! Every call into the script (a lifecycle function, a handler, a task) runs
//! as a coroutine, so `world:waitTicks(n)` can pause it until `n` ticks have
//! passed, as `await world.waitTicks(n)` does in JavaScript. Each resumption
//! is held to the execution time limit.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use mlua::thread::ThreadStatus;
use mlua::{Function, IntoLuaMulti, Lua, MultiValue, Table, Thread, Value, VmState};
use serde_json::Value as Json;
use tracing::Level;

use crate::definition::parse_command;
use crate::luau_require::PluginRequirer;
use crate::manifest::PluginSource;
use crate::plugin::{HANDLER_FAILED, Limits, Plugin, PluginError};
use crate::state::{self, Phase, State};
use crate::{CommandCall, CommandReply, CommandSender, Event, wire};

/// Registry keys of each VM's tables.
const HANDLERS: &str = "bedrockrs.handlers"; // event name → list of functions
const COMMANDS: &str = "bedrockrs.commands"; // command → path → function
const TASKS: &str = "bedrockrs.tasks"; // task id → function, or a waiting thread
const PLAYER: &str = "bedrockrs.player"; // the metatable of player tables
pub(crate) const CORE: &str = "bedrockrs.core"; // what `require("@bedrock-rs/core")` returns

/// A Luau plugin.
pub(crate) struct LuauPlugin {
    name: String,
    state: State,
    lua: Lua,
    /// The table the script returned: its lifecycle functions.
    exports: Option<Table>,
    /// When the running call must stop; checked by the VM's interrupt callback.
    deadline: Rc<Cell<Option<Instant>>>,
    limits: Limits,
}

impl LuauPlugin {
    /// Runs `source`, the script `main` in the plugin's `folder`, in a fresh
    /// VM: its top level, up to the table of lifecycle functions it returns.
    pub fn new(
        state: State,
        folder: &Path,
        main: &Path,
        source: &str,
        limits: Limits,
    ) -> Result<Self, PluginError> {
        let name = state::lock(&state).name.clone();
        let lua = Lua::new();
        let deadline = Rc::new(Cell::new(None::<Instant>));
        let plugin = Self {
            name,
            state,
            lua,
            exports: None,
            deadline,
            limits,
        };
        plugin.install(folder).map_err(script_error)?;
        let exports = plugin
            .within_limit(|| {
                plugin
                    .lua
                    .load(source)
                    .set_name(PluginRequirer::chunk_name(folder, main))
                    .call::<Value>(())
            })
            .map_err(script_error)?;
        let exports = match exports {
            Value::Table(table) => Some(table),
            Value::Nil => None,
            other => {
                return Err(PluginError::Script(format!(
                    "a plugin script returns a table of its lifecycle functions, or nothing; \
                     this one returned a {}",
                    other.type_name()
                )));
            }
        };
        Ok(Self { exports, ..plugin })
    }

    /// Sets up the VM: limits, `require`, the API, then the sandbox.
    fn install(&self, folder: &Path) -> mlua::Result<()> {
        let lua = &self.lua;
        lua.set_memory_limit(self.limits.memory)?;
        // Globals become read-only once sandboxed, so install ours first.
        // mlua's own `require` reads anywhere on disk; the plugin's stays in
        // its folder, and knows `@bedrock-rs/core`.
        let require = lua.create_require_function(PluginRequirer::new(folder))?;
        lua.globals().raw_set("require", require)?;
        for key in [HANDLERS, COMMANDS, TASKS] {
            lua.set_named_registry_value(key, lua.create_table()?)?;
        }
        let core = self.core()?;
        // `print` is `Logger.info`.
        let logger: Table = core.raw_get("Logger")?;
        lua.globals()
            .raw_set("print", logger.raw_get::<Function>("info")?)?;
        lua.set_named_registry_value(CORE, core)?;
        lua.sandbox(true)?;

        let expiry = Rc::clone(&self.deadline);
        lua.set_interrupt(move |_| match expiry.get() {
            Some(deadline) if Instant::now() >= deadline => Err(mlua::Error::runtime(
                "plugin exceeded its execution time limit",
            )),
            _ => Ok(VmState::Continue),
        });
        Ok(())
    }

    /// The `@bedrock-rs/core` module: `{ Logger, Server, Player, World }`.
    fn core(&self) -> mlua::Result<Table> {
        let lua = &self.lua;
        let core = lua.create_table()?;
        core.raw_set("Logger", self.logger()?)?;
        let player = self.player_class()?;
        lua.set_named_registry_value(PLAYER, lua.create_table_from([("__index", &player)])?)?;
        let world = self.world()?;
        core.raw_set("Server", self.server(&world)?)?;
        core.raw_set("Player", player)?;
        core.raw_set("World", world)?;
        core.set_readonly(true);
        Ok(core)
    }

    /// `Logger.trace/debug/info/warn/error(...)`: the arguments, as
    /// `tostring` shows them, separated by spaces.
    fn logger(&self) -> mlua::Result<Table> {
        let logger = self.lua.create_table()?;
        for (name, level) in [
            ("trace", Level::TRACE),
            ("debug", Level::DEBUG),
            ("info", Level::INFO),
            ("warn", Level::WARN),
            ("error", Level::ERROR),
        ] {
            let state = State::clone(&self.state);
            let log = self.lua.create_function(move |_, args: MultiValue| {
                let message = args
                    .iter()
                    .map(Value::to_string)
                    .collect::<mlua::Result<Vec<_>>>()?
                    .join(" ");
                state::lock(&state)
                    .log(level, &message)
                    .map_err(mlua::Error::runtime)
            })?;
            logger.raw_set(name, log)?;
        }
        logger.set_readonly(true);
        Ok(logger)
    }

    /// `Server`: `on`, `broadcast`, `getPlayer`, `getPlayers`,
    /// `registerCommand` and `getWorld`.
    fn server(&self, world: &Table) -> mlua::Result<Table> {
        let lua = &self.lua;
        let server = lua.create_table()?;

        let state = State::clone(&self.state);
        let on = lua.create_function(move |lua, (event, handler): (String, Function)| {
            state::lock(&state)
                .subscribe(&event)
                .map_err(mlua::Error::runtime)?;
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
        server.raw_set("on", on)?;

        let state = State::clone(&self.state);
        let broadcast = lua.create_function(move |_, message: String| {
            state::lock(&state)
                .broadcast(&message)
                .map_err(mlua::Error::runtime)
        })?;
        server.raw_set("broadcast", broadcast)?;

        let state = State::clone(&self.state);
        let get_player = lua.create_function(move |lua, uuid: String| {
            let player = state::lock(&state)
                .player(&uuid)
                .map_err(mlua::Error::runtime)?;
            player
                .map(|player| json_to_lua(lua, &wire::player(&player)))
                .transpose()
        })?;
        server.raw_set("getPlayer", get_player)?;

        let state = State::clone(&self.state);
        let get_players = lua.create_function(move |lua, ()| {
            let players = state::lock(&state)
                .players()
                .map_err(mlua::Error::runtime)?;
            let list: Vec<Json> = players.iter().map(wire::player).collect();
            json_to_lua(lua, &Json::Array(list))
        })?;
        server.raw_set("getPlayers", get_players)?;

        let state = State::clone(&self.state);
        let register = lua.create_function(move |lua, definition: Value| {
            let mut handlers = Vec::new();
            let json = definition_to_json(&definition, &mut Vec::new(), &mut handlers, false)
                .map_err(|err| mlua::Error::runtime(format!("invalid command: {err}")))?;
            let spec = parse_command(&json)
                .map_err(|err| mlua::Error::runtime(format!("invalid command: {err}")))?;
            let name = spec.name.clone();
            state::lock(&state)
                .register_command(spec)
                .map_err(mlua::Error::runtime)?;
            let paths = lua.create_table()?;
            for (path, handler) in handlers {
                paths.raw_set(path, handler)?;
            }
            lua.named_registry_value::<Table>(COMMANDS)?
                .raw_set(name, paths)
        })?;
        server.raw_set("registerCommand", register)?;

        let (state, world) = (State::clone(&self.state), world.clone());
        let get_world = lua.create_function(move |_, ()| {
            // Usable from on_load on, like the rest.
            if state::lock(&state).phase == Phase::TopLevel {
                return Err(mlua::Error::runtime(state::TOP_LEVEL));
            }
            Ok(world.clone())
        })?;
        server.raw_set("getWorld", get_world)?;

        server.set_readonly(true);
        Ok(server)
    }

    /// `Player`: the methods of player tables, which are `{ name, uuid }`.
    fn player_class(&self) -> mlua::Result<Table> {
        let lua = &self.lua;
        let player = lua.create_table()?;
        let method = |name: &'static str,
                      act: fn(&state::PluginState, &str, &[Value]) -> Result<(), String>|
         -> mlua::Result<Function> {
            let state = State::clone(&self.state);
            lua.create_function(move |_, args: MultiValue| {
                let mut args = args.into_iter();
                let uuid = match args.next() {
                    Some(Value::Table(this)) => this.raw_get::<String>("uuid").ok(),
                    _ => None,
                }
                .ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "{name} is a method: call it as player:{name}(...)"
                    ))
                })?;
                let rest: Vec<Value> = args.collect();
                act(&state::lock(&state), &uuid, &rest).map_err(mlua::Error::runtime)
            })
        };
        player.raw_set(
            "sendMessage",
            method("sendMessage", |state, uuid, args| {
                state.send_message(
                    uuid,
                    &text_arg(args.first(), "sendMessage expects a message string")?,
                )
            })?,
        )?;
        player.raw_set(
            "kick",
            method("kick", |state, uuid, args| {
                let reason = optional_text(args.first(), "kick expects a reason string")?;
                state.kick(uuid, reason.as_deref())
            })?,
        )?;
        player.raw_set(
            "setGameMode",
            method("setGameMode", |state, uuid, args| {
                state.set_game_mode(
                    uuid,
                    &text_arg(args.first(), "setGameMode expects a game mode name")?,
                )
            })?,
        )?;
        player.raw_set(
            "setHealth",
            method("setHealth", |state, uuid, args| {
                state.set_health(
                    uuid,
                    number_arg(args.first(), "setHealth expects a number")?,
                )
            })?,
        )?;
        player.raw_set(
            "damage",
            method("damage", |state, uuid, args| {
                let amount = number_arg(args.first(), "damage expects an amount above 0")?;
                let cause = optional_text(args.get(1), "damage expects a cause string")?;
                state.damage(uuid, amount, cause.as_deref())
            })?,
        )?;
        player.set_readonly(true);
        Ok(player)
    }

    /// The world, `Server.getWorld()`: its scheduler. `run(callback)`,
    /// `runTimeout(callback, ticks = 1)`, `runInterval(callback, ticks = 1)`,
    /// `clearRun(id)` and `waitTicks(ticks = 1)`, as methods.
    fn world(&self) -> mlua::Result<Table> {
        let lua = &self.lua;
        let world = lua.create_table()?;

        let schedule = |repeats: bool, default: f64| -> mlua::Result<Function> {
            let state = State::clone(&self.state);
            lua.create_function(move |lua, args: MultiValue| {
                let mut args = without_self(args);
                let callback = match args.next() {
                    Some(Value::Function(callback)) => callback,
                    _ => return Err(mlua::Error::runtime("expected a callback function")),
                };
                let ticks = match args.next() {
                    None | Some(Value::Nil) => default,
                    Some(value) => number_arg(Some(&value), "ticks must be a number")
                        .map_err(mlua::Error::runtime)?,
                };
                let id = state::lock(&state)
                    .schedule(ticks, repeats.then_some(ticks))
                    .map_err(mlua::Error::runtime)?;
                lua.named_registry_value::<Table>(TASKS)?
                    .raw_set(id, callback)?;
                Ok(id)
            })
        };
        world.raw_set("run", schedule(false, 0.0)?)?;
        world.raw_set("runTimeout", schedule(false, 1.0)?)?;
        world.raw_set("runInterval", schedule(true, 1.0)?)?;

        let state = State::clone(&self.state);
        let clear = lua.create_function(move |lua, args: MultiValue| {
            let id = number_arg(
                without_self(args).next().as_ref(),
                "clearRun expects a task id",
            )
            .map_err(mlua::Error::runtime)?;
            state::lock(&state)
                .clear_run(id)
                .map_err(mlua::Error::runtime)?;
            lua.named_registry_value::<Table>(TASKS)?
                .raw_set(id, Value::Nil)
        })?;
        world.raw_set("clearRun", clear)?;

        // `waitTicks` yields the running coroutine; a task resumes it.
        let state = State::clone(&self.state);
        let wait = lua.create_function(move |lua, (thread, ticks): (Thread, Option<f64>)| {
            let id = state::lock(&state)
                .schedule(ticks.unwrap_or(1.0), None)
                .map_err(mlua::Error::runtime)?;
            lua.named_registry_value::<Table>(TASKS)?
                .raw_set(id, thread)
        })?;
        let wait_ticks: Function = lua
            .load(
                r#"
                local wait = ...
                return function(...)
                    local args = { ... }
                    -- Called as world:waitTicks(n) or world.waitTicks(n).
                    local ticks = if type(args[1]) == "table" then args[2] else args[1]
                    wait(coroutine.running(), ticks)
                    return coroutine.yield()
                end
            "#,
            )
            .set_name("=bedrockrs")
            .call(wait)?;
        world.raw_set("waitTicks", wait_ticks)?;
        world.set_readonly(true);
        Ok(world)
    }

    fn within_limit<T>(&self, call: impl FnOnce() -> mlua::Result<T>) -> mlua::Result<T> {
        self.deadline
            .set(Some(Instant::now() + self.limits.execution));
        let result = call();
        self.deadline.set(None);
        result
    }

    /// Resumes `thread` within the time limit: the values it returned, or
    /// `None` if it is waiting (`waitTicks`).
    fn resume(&self, thread: &Thread, args: impl IntoLuaMulti) -> mlua::Result<Option<MultiValue>> {
        let values = self.within_limit(|| thread.resume::<MultiValue>(args))?;
        Ok(match thread.status() {
            ThreadStatus::Resumable => None,
            _ => Some(values),
        })
    }

    /// Calls `function` as a coroutine; see [`LuauPlugin::resume`].
    fn call(
        &self,
        function: Function,
        args: impl IntoLuaMulti,
    ) -> mlua::Result<Option<MultiValue>> {
        let thread = self.lua.create_thread(function)?;
        self.resume(&thread, args)
    }

    /// Calls the lifecycle function `name`, if the plugin has one.
    fn lifecycle(&mut self, name: &str) -> Result<(), PluginError> {
        let Some(exports) = &self.exports else {
            return Ok(());
        };
        let function = match exports.raw_get::<Value>(name).map_err(script_error)? {
            Value::Nil => return Ok(()),
            Value::Function(function) => function,
            other => {
                return Err(PluginError::Script(format!(
                    "{name} must be a function, not a {}",
                    other.type_name()
                )));
            }
        };
        self.call(function, ()).map_err(script_error)?;
        Ok(())
    }

    fn report(&self, what: &str, error: &mlua::Error) {
        state::lock(&self.state).report_failure(what, &error.to_string());
    }
}

impl Plugin for LuauPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_load(&mut self) -> Result<(), PluginError> {
        self.lifecycle("on_load")
    }

    fn on_enable(&mut self) -> Result<(), PluginError> {
        self.lifecycle("on_enable")
    }

    fn on_disable(&mut self) -> Result<(), PluginError> {
        self.lifecycle("on_disable")
    }

    fn dispatch(&mut self, event: &Event) -> bool {
        let handlers = self
            .lua
            .named_registry_value::<Table>(HANDLERS)
            .and_then(|handlers| handlers.raw_get::<Option<Table>>(event.name()));
        let handlers: Vec<Function> = match handlers {
            Ok(Some(list)) => list.sequence_values().filter_map(Result::ok).collect(),
            Ok(None) => return false,
            Err(err) => {
                self.report(&format!("{} handlers", event.name()), &err);
                return false;
            }
        };
        let cancelled = Rc::new(Cell::new(false));
        let payload = match event_payload(&self.lua, event, &cancelled) {
            Ok(payload) => payload,
            Err(err) => {
                self.report(&format!("{} event", event.name()), &err);
                return false;
            }
        };
        for handler in handlers {
            if let Err(err) = self.call(handler, &payload) {
                self.report(&format!("{} handler", event.name()), &err);
            }
        }
        cancelled.get()
    }

    fn run_command(&mut self, call: &CommandCall) -> CommandReply {
        let handler = self
            .lua
            .named_registry_value::<Table>(COMMANDS)
            .and_then(|commands| commands.raw_get::<Option<Table>>(call.command.as_str()))
            .and_then(|paths| {
                paths
                    .map(|paths| paths.raw_get::<Option<Function>>(call.path.join(" ")))
                    .transpose()
            })
            .map(Option::flatten);
        let handler = match handler {
            Ok(Some(handler)) => handler,
            Ok(None) => {
                return CommandReply::error(format!("/{} is no longer available", call.command));
            }
            Err(err) => {
                self.report(&format!("/{}", call.command), &err);
                return CommandReply::error(HANDLER_FAILED);
            }
        };
        let reply = Rc::new(RefCell::new(CommandReply::default()));
        let finished = Rc::new(Cell::new(false));
        let result = command_context(&self.lua, &self.state, call, &reply, &finished)
            .and_then(|ctx| self.call(handler, ctx));
        finished.set(true);
        let mut reply = reply.take();
        match result {
            Ok(Some(values)) => {
                if let Some(Value::String(text)) = values.into_iter().next()
                    && let Ok(text) = text.to_str()
                {
                    reply.push_ok(text.to_owned());
                }
            }
            // Waiting: what it replies from now on is a late reply.
            Ok(None) => {}
            Err(err) => {
                self.report(&format!("/{} command", call.command), &err);
                reply.push_error(HANDLER_FAILED);
            }
        }
        reply
    }

    fn run_tasks(&mut self, ids: &[u32]) {
        let Ok(tasks) = self.lua.named_registry_value::<Table>(TASKS) else {
            return;
        };
        for id in ids {
            let Ok(task) = tasks.raw_get::<Value>(*id) else {
                continue;
            };
            // A one-off is done once it runs.
            if !state::lock(&self.state).tasks.contains(*id) {
                let _ = tasks.raw_set(*id, Value::Nil);
            }
            let result = match task {
                Value::Function(callback) => self.call(callback, ()),
                Value::Thread(thread) if thread.status() == ThreadStatus::Resumable => {
                    self.resume(&thread, ())
                }
                _ => continue,
            };
            if let Err(err) = result {
                self.report("scheduled task", &err);
            }
        }
    }
}

/// Compiles every Luau file of a plugin without running any of it: what a
/// reload checks before it stops the version that runs.
pub(crate) fn check(
    folder: &Path,
    source: &PluginSource,
    limits: Limits,
) -> Result<(), PluginError> {
    let lua = Lua::new();
    lua.set_memory_limit(limits.memory).map_err(script_error)?;
    for (path, contents) in &source.files {
        let relative = path.strip_prefix(folder).unwrap_or(path);
        lua.load(contents.as_slice())
            .set_name(PluginRequirer::chunk_name(folder, relative))
            .into_function()
            .map_err(script_error)?;
    }
    Ok(())
}

fn script_error(error: mlua::Error) -> PluginError {
    PluginError::Script(error.to_string())
}

/// The arguments of a function that may be called as a method, without the
/// table it was called on.
fn without_self(args: MultiValue) -> impl Iterator<Item = Value> {
    let mut args = args.into_iter().peekable();
    if matches!(args.peek(), Some(Value::Table(_))) {
        args.next();
    }
    args
}

fn text_arg(value: Option<&Value>, message: &str) -> Result<String, String> {
    match value {
        Some(Value::String(text)) => text
            .to_str()
            .map(|text| text.to_owned())
            .map_err(|_| message.to_owned()),
        _ => Err(message.to_owned()),
    }
}

fn optional_text(value: Option<&Value>, message: &str) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Nil) => Ok(None),
        some => text_arg(some, message).map(Some),
    }
}

fn number_arg(value: Option<&Value>, message: &str) -> Result<f64, String> {
    match value {
        Some(Value::Integer(int)) => Ok(*int as f64),
        Some(Value::Number(number)) => Ok(*number),
        _ => Err(message.to_owned()),
    }
}

/// JSON from the server as Luau values: read-only tables, with players (an
/// object with a `name` and a `uuid`) as player tables.
fn json_to_lua(lua: &Lua, json: &Json) -> mlua::Result<Value> {
    Ok(match json {
        Json::Null => Value::Nil,
        Json::Bool(value) => Value::Boolean(*value),
        Json::Number(number) => match number.as_i64() {
            Some(int) => Value::Integer(int),
            None => Value::Number(number.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(text) => Value::String(lua.create_string(text)?),
        Json::Array(items) => {
            let table = lua.create_table()?;
            for item in items {
                table.raw_push(json_to_lua(lua, item)?)?;
            }
            table.set_readonly(true);
            Value::Table(table)
        }
        Json::Object(map) => {
            let table = lua.create_table()?;
            for (key, value) in map {
                table.raw_set(key.as_str(), json_to_lua(lua, value)?)?;
            }
            if wire::is_player(json) {
                table.set_metatable(Some(lua.named_registry_value::<Table>(PLAYER)?))?;
            }
            table.set_readonly(true);
            Value::Table(table)
        }
    })
}

/// A command definition table as JSON for [`parse_command`], with each
/// `run` function taken out into `handlers` under its subcommand path.
fn definition_to_json(
    value: &Value,
    path: &mut Vec<String>,
    handlers: &mut Vec<(String, Function)>,
    in_subcommands: bool,
) -> Result<Json, String> {
    Ok(match value {
        Value::Nil => Json::Null,
        Value::Boolean(value) => Json::Bool(*value),
        Value::Integer(int) => Json::from(*int),
        Value::Number(number) => serde_json::Number::from_f64(*number)
            .map(Json::Number)
            .ok_or("numbers must be finite")?,
        Value::String(text) => {
            Json::String(text.to_str().map_err(|err| err.to_string())?.to_owned())
        }
        Value::Table(table) => {
            let length = table.raw_len();
            let is_list = length > 0
                && table
                    .pairs::<Value, Value>()
                    .all(|pair| matches!(pair, Ok((Value::Integer(key), _)) if key >= 1 && key as usize <= length));
            if is_list || (length == 0 && table.pairs::<Value, Value>().next().is_none()) {
                let mut items = Vec::new();
                for item in table.sequence_values::<Value>() {
                    let item = item.map_err(|err| err.to_string())?;
                    items.push(definition_to_json(&item, path, handlers, false)?);
                }
                Json::Array(items)
            } else {
                let mut map = serde_json::Map::new();
                for pair in table.pairs::<Value, Value>() {
                    let (key, value) = pair.map_err(|err| err.to_string())?;
                    let Value::String(key) = key else {
                        return Err("tables must have string keys or be lists".into());
                    };
                    let key = key.to_str().map_err(|err| err.to_string())?.to_owned();
                    let json = if in_subcommands {
                        // Each entry is a subcommand node, named by its key.
                        path.push(key.clone());
                        let node = definition_to_json(&value, path, handlers, false);
                        path.pop();
                        node?
                    } else if key == "run" {
                        match value {
                            Value::Function(handler) => {
                                handlers.push((path.join(" "), handler));
                                Json::Bool(true)
                            }
                            other => definition_to_json(&other, path, handlers, false)?,
                        }
                    } else {
                        definition_to_json(&value, path, handlers, key == "subcommands")?
                    };
                    map.insert(key, json);
                }
                Json::Object(map)
            }
        }
        Value::Function(_) => return Err("functions are only allowed as run handlers".into()),
        other => {
            return Err(format!(
                "a {} cannot be part of a command",
                other.type_name()
            ));
        }
    })
}

/// The event handlers receive (see [`wire::event`]), with `cancel()` and
/// `isCancelled()` for cancellable events.
fn event_payload(lua: &Lua, event: &Event, cancelled: &Rc<Cell<bool>>) -> mlua::Result<Table> {
    let json = wire::event(event);
    let data = json.get("data").cloned().unwrap_or(Json::Null);
    let payload = lua.create_table()?;
    if let Json::Object(map) = &data {
        for (key, value) in map {
            payload.raw_set(key.as_str(), json_to_lua(lua, value)?)?;
        }
    }
    if event.is_cancellable() {
        let cancel = Rc::clone(cancelled);
        payload.raw_set(
            "cancel",
            lua.create_function(move |_, _: MultiValue| {
                cancel.set(true);
                Ok(())
            })?,
        )?;
        let cancel = Rc::clone(cancelled);
        payload.raw_set(
            "isCancelled",
            lua.create_function(move |_, _: MultiValue| Ok(cancel.get()))?,
        )?;
    }
    payload.set_readonly(true);
    Ok(payload)
}

/// The `ctx` command handlers receive: `sender` (the player, or `nil` for the
/// console), `console`, `command`, `path` (the subcommands typed), `args` by
/// name, and `reply(message)` / `error(message)` to answer. A string the
/// handler returns is replied too. Replies after the handler returned (or
/// once it waits) reach a player as chat and the console as a log line.
fn command_context(
    lua: &Lua,
    state: &State,
    call: &CommandCall,
    reply: &Rc<RefCell<CommandReply>>,
    finished: &Rc<Cell<bool>>,
) -> mlua::Result<Table> {
    let json = wire::command_call(call);
    let ctx = lua.create_table()?;
    ctx.raw_set("sender", json_to_lua(lua, &json["sender"])?)?;
    ctx.raw_set("console", call.sender == CommandSender::Console)?;
    ctx.raw_set("command", call.command.as_str())?;
    ctx.raw_set("path", json_to_lua(lua, &json["path"])?)?;
    ctx.raw_set("args", json_to_lua(lua, &json["args"])?)?;
    for (name, success) in [("reply", true), ("error", false)] {
        let (reply, finished, state) = (Rc::clone(reply), Rc::clone(finished), State::clone(state));
        let sender = call.sender.clone();
        let respond = lua.create_function(move |_, args: MultiValue| {
            let text = match without_self(args).next() {
                Some(Value::String(text)) if !text.as_bytes().is_empty() => {
                    text.to_str()?.to_owned()
                }
                _ => return Err(mlua::Error::runtime("expected a message string")),
            };
            if !finished.get() {
                let mut reply = reply.borrow_mut();
                if success {
                    reply.push_ok(text);
                } else {
                    reply.push_error(text);
                }
                return Ok(());
            }
            state::lock(&state)
                .late_reply(&sender, &text, success)
                .map_err(mlua::Error::runtime)
        })?;
        ctx.raw_set(name, respond)?;
    }
    ctx.set_readonly(true);
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;
    use crate::state::{PluginState, Shared};
    use crate::{Action, ArgValue, Player};

    type Lines = Arc<Mutex<Vec<(Level, String)>>>;

    struct Setup {
        state: State,
        lines: Lines,
        actions: mpsc::Receiver<Action>,
    }

    fn setup() -> Setup {
        let lines: Lines = Arc::default();
        let sink = Arc::clone(&lines);
        let (actions, received) = mpsc::channel(16);
        let shared = Shared::new(
            actions,
            Arc::new(move |_, level, message: &str| {
                sink.lock().unwrap().push((level, message.to_owned()))
            }),
        );
        let state = PluginState::new("test", shared);
        state::lock(&state).release_output();
        Setup {
            state,
            lines,
            actions: received,
        }
    }

    const LIMITS: Limits = Limits {
        memory: 16 * 1024 * 1024,
        execution: Duration::from_millis(250),
    };

    fn load(setup: &Setup, source: &str) -> Result<LuauPlugin, PluginError> {
        LuauPlugin::new(
            State::clone(&setup.state),
            Path::new("."),
            Path::new("main.luau"),
            source,
            LIMITS,
        )
    }

    /// Loads `source` and runs it up to enabled, as the manager would.
    fn enabled(setup: &Setup, source: &str) -> LuauPlugin {
        let mut plugin = load(setup, source).unwrap();
        state::lock(&setup.state).phase = Phase::Loading;
        plugin.on_load().unwrap();
        state::lock(&setup.state).phase = Phase::Enabled;
        plugin.on_enable().unwrap();
        plugin
    }

    fn messages(setup: &Setup) -> Vec<String> {
        setup
            .lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, line)| line.clone())
            .collect()
    }

    fn steve() -> Player {
        Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        }
    }

    /// Runs whatever is due at each tick up to `until`.
    fn tick(setup: &Setup, plugin: &mut LuauPlugin, until: u64) {
        let clock = Arc::clone(&state::lock(&setup.state).shared().clock);
        for now in clock.load(std::sync::atomic::Ordering::Relaxed) + 1..=until {
            clock.store(now, std::sync::atomic::Ordering::Relaxed);
            let due = state::lock(&setup.state).tasks.take_due(now);
            plugin.run_tasks(&due);
        }
    }

    #[test]
    fn lifecycle_functions_run_in_order_with_the_api() {
        let setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                local Core = require("@bedrock-rs/core")
                local Logger = Core.Logger
                local greeting = "hello"
                return {
                    on_load = function() Logger.info(greeting, "load") end,
                    on_enable = function() print("enable") end,
                    on_disable = function() Logger.warn("disable", 1) end,
                }
            "#,
        );
        plugin.on_disable().unwrap();
        assert_eq!(messages(&setup), ["hello load", "enable", "disable 1"]);
    }

    #[test]
    fn the_top_level_cannot_use_the_api() {
        let setup = setup();
        let err = load(
            &setup,
            r#"require("@bedrock-rs/core").Logger.info("too early")"#,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("not available at the top level"), "{err}");
        let err = load(&setup, r#"print("too early")"#)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("not available at the top level"), "{err}");
        // A script need not return anything.
        assert!(load(&setup, "local x = 1").is_ok());
        let err = load(&setup, "return 5").err().unwrap().to_string();
        assert!(err.contains("returns a table"), "{err}");
    }

    #[test]
    fn commands_register_in_on_load_only_and_run() {
        let setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                local Server = require("@bedrock-rs/core").Server
                return {
                    on_load = function()
                        Server.registerCommand({
                            name = "warp",
                            args = { { name = "name", type = "string" } },
                            run = function(ctx) ctx.reply(`to {ctx.args.name}`) end,
                            subcommands = {
                                set = { run = function(ctx) return `set by {ctx.sender.name}` end },
                                broken = { run = function() error("oops") end },
                            },
                        })
                    end,
                }
            "#,
        );
        let commands = state::lock(&setup.state).commands.clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "warp");

        let call = |path: &[&str], args: Vec<(&str, ArgValue)>| CommandCall {
            plugin: "test".into(),
            command: "warp".into(),
            path: path.iter().map(|name| (*name).to_owned()).collect(),
            args: args
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
            sender: CommandSender::Player(steve()),
        };
        let reply =
            plugin.run_command(&call(&[], vec![("name", ArgValue::String("spawn".into()))]));
        assert_eq!(reply, CommandReply::ok("to spawn"));
        assert_eq!(
            plugin.run_command(&call(&["set"], Vec::new())),
            CommandReply::ok("set by Steve")
        );
        assert_eq!(
            plugin.run_command(&call(&["broken"], Vec::new())),
            CommandReply::error(HANDLER_FAILED)
        );
    }

    #[test]
    fn on_enable_cannot_register_commands() {
        let setup = setup();
        let mut plugin = load(
            &setup,
            r#"
                local Server = require("@bedrock-rs/core").Server
                return { on_enable = function() Server.registerCommand({ name = "x", run = print }) end }
            "#,
        )
        .unwrap();
        state::lock(&setup.state).phase = Phase::Enabled;
        let err = plugin.on_enable().unwrap_err().to_string();
        assert!(err.contains("only be registered in on_load"), "{err}");
    }

    #[test]
    fn events_reach_handlers_which_can_cancel_and_act() {
        let mut setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                local Server = require("@bedrock-rs/core").Server
                return {
                    on_load = function()
                        Server.on("player_chat", function(event)
                            if event.message:find("badword") then event.cancel() end
                        end)
                        Server.on("player_join", function(event)
                            event.player:sendMessage(`hi {event.player.name}`)
                        end)
                    end,
                }
            "#,
        );
        let chat = |message: &str| Event::PlayerChat {
            player: steve(),
            message: message.into(),
        };
        assert!(plugin.dispatch(&chat("a badword")));
        assert!(!plugin.dispatch(&chat("hello")));
        assert!(!plugin.dispatch(&Event::PlayerJoin(steve())));
        assert_eq!(
            setup.actions.try_recv().unwrap(),
            Action::SendMessage {
                player: steve().uuid,
                message: "hi Steve".into()
            }
        );
    }

    #[test]
    fn the_scheduler_runs_timeouts_intervals_and_waits() {
        let setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                local Core = require("@bedrock-rs/core")
                local Logger, Server = Core.Logger, Core.Server
                local interval
                return {
                    on_enable = function()
                        local world = Server.getWorld()
                        world:run(function() Logger.info("next tick") end)
                        world:runTimeout(function() Logger.info("after 5") end, 5)
                        interval = world:runInterval(function() Logger.info("every 3") end, 3)
                        Logger.info("waiting")
                        world:waitTicks(4)
                        Logger.info("waited 4")
                        world:clearRun(interval)
                    end,
                }
            "#,
        );
        assert_eq!(messages(&setup), ["waiting"], "on_enable is waiting");
        tick(&setup, &mut plugin, 10);
        assert_eq!(
            messages(&setup),
            ["waiting", "next tick", "every 3", "waited 4", "after 5"]
        );
    }

    #[test]
    fn failures_in_handlers_and_tasks_are_logged_not_fatal() {
        let setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                local Core = require("@bedrock-rs/core")
                local Logger, Server = Core.Logger, Core.Server
                return {
                    on_load = function()
                        Server.on("player_join", function() error("first") end)
                        Server.on("player_join", function() Logger.info("second") end)
                    end,
                    on_enable = function()
                        Server.getWorld():run(function() error("task") end)
                    end,
                }
            "#,
        );
        plugin.dispatch(&Event::PlayerJoin(steve()));
        tick(&setup, &mut plugin, 2);
        assert_eq!(messages(&setup), ["second"]);
    }

    #[test]
    fn runaway_scripts_are_stopped() {
        let setup = setup();
        let err = load(&setup, "while true do end").err().unwrap().to_string();
        assert!(err.contains("execution time limit"), "{err}");
        let mut plugin = load(
            &setup,
            "return { on_load = function() while true do end end }",
        )
        .unwrap();
        state::lock(&setup.state).phase = Phase::Loading;
        let err = plugin.on_load().unwrap_err().to_string();
        assert!(err.contains("execution time limit"), "{err}");
    }

    #[test]
    fn the_api_is_read_only_and_players_have_methods() {
        let setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                local Core = require("@bedrock-rs/core")
                local Server = Core.Server
                return {
                    on_load = function()
                        Server.on("player_join", function(event)
                            local ok = pcall(function() Server.broadcast = nil end)
                            assert(not ok, "Server is read-only")
                            local ok2, err = pcall(function() event.player.sendMessage("x") end)
                            assert(not ok2 and tostring(err):find("is a method"), tostring(err))
                            Core.Player.kick(event.player)
                        end)
                    end,
                }
            "#,
        );
        plugin.dispatch(&Event::PlayerJoin(steve()));
        assert!(messages(&setup).is_empty(), "{:?}", messages(&setup));
    }
}
