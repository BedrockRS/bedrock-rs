//! [`JavaScriptPlugin`]: a plugin written in JavaScript, run by the QuickJS
//! engine built into the server (through `rquickjs`), each in its own
//! runtime.
//!
//! The plugin's `main` is an ES module. It imports the API and exports its
//! lifecycle functions:
//!
//! ```js
//! import { Logger, Server } from "@bedrock-rs/core";
//!
//! export function on_load() { Server.registerCommand({ ... }); }
//! export function on_enable() {
//!     Server.getWorld().runInterval(() => Logger.info("tick tock"), 20 * 60);
//! }
//! export function on_disable() {}
//! ```
//!
//! The source runs as it is: nothing compiles it first and nothing needs
//! installing. `@bedrock-rs/core` is a native module
//! ([`crate::javascript_modules`]): each function in it is a Rust closure bound
//! straight into the engine, which checks its arguments and calls
//! [`crate::state`], as the Luau engine's functions do, so the same plugin
//! behaves the same in either language.
//!
//! `async` works through the scheduler: `world.waitTicks(n)` returns a Promise
//! that a task resolves `n` ticks later. After every call into the plugin the
//! engine runs the promise jobs it queued, within the same execution time
//! limit, so code after an `await` runs as soon as what it awaited is done.
//!
//! The functions a plugin hands the API (handlers, tasks) are held from Rust
//! in a [`Registry`]. They must all be let go before the runtime is freed,
//! which [`JavaScriptPlugin`]'s `Drop` sees to.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use rquickjs::function::{IntoArgs, Opt, Rest, This};
use rquickjs::promise::PromiseState;
use rquickjs::{
    Array, Coerced, Context, Ctx, Error, Exception, Function, Module, Object, Persistent, Promise,
    Runtime, Value,
};
use serde_json::Value as Json;
use tracing::Level;

use crate::definition::parse_command;
use crate::javascript_modules::PluginModules;
use crate::manifest::PluginSource;
use crate::plugin::{HANDLER_FAILED, Limits, Plugin, PluginError};
use crate::state::{self, Phase, PluginState, State};
use crate::{CommandCall, CommandReply, CommandSender, Event, Player, wire};

/// The most native stack a plugin's JavaScript may use, well within the
/// plugin thread's.
const STACK_SIZE: usize = 512 * 1024;

/// Why a call that ran past the execution limit was stopped, as in Luau.
const TIME_LIMIT: &str = "plugin exceeded its execution time limit";

/// The levels of `Logger`'s functions.
const LEVELS: [(&str, Level); 5] = [
    ("trace", Level::TRACE),
    ("debug", Level::DEBUG),
    ("info", Level::INFO),
    ("warn", Level::WARN),
    ("error", Level::ERROR),
];

/// What the plugin handed the API, and the API objects made for it, held
/// from Rust between calls.
#[derive(Default)]
struct Registry {
    /// `Player.prototype`, which every player object has.
    player: Option<Persistent<Object<'static>>>,
    /// What `Server.getWorld()` returns.
    world: Option<Persistent<Object<'static>>>,
    /// Event name → handlers, in the order added.
    handlers: BTreeMap<String, Vec<Persistent<Function<'static>>>>,
    /// (Command, subcommand path) → handler.
    commands: HashMap<(String, String), Persistent<Function<'static>>>,
    /// Task id → callback, or the resolve function of a `waitTicks` Promise.
    tasks: BTreeMap<u32, Persistent<Function<'static>>>,
}

type Shared = Rc<RefCell<Registry>>;

/// A JavaScript plugin.
pub(crate) struct JavaScriptPlugin {
    name: String,
    state: State,
    registry: Shared,
    /// The main module's exports: its lifecycle functions.
    exports: Option<Persistent<Object<'static>>>,
    /// When the running call must stop; checked by the interrupt handler.
    deadline: Rc<Cell<Option<Instant>>>,
    limits: Limits,
    /// The plugin's own context, in its own runtime, dropped last.
    context: Context,
}

impl JavaScriptPlugin {
    /// Runs `source`, the module `main` in the plugin's `folder`, in a fresh
    /// runtime: its top level, and the modules it imports.
    pub fn new(
        state: State,
        folder: &Path,
        main: &Path,
        source: &str,
        limits: Limits,
    ) -> Result<Self, PluginError> {
        let name = state::lock(&state).name.clone();
        let runtime = Runtime::new().map_err(engine_error)?;
        runtime.set_memory_limit(limits.memory);
        runtime.set_max_stack_size(STACK_SIZE);
        let deadline = Rc::new(Cell::new(None::<Instant>));
        let expiry = Rc::clone(&deadline);
        runtime.set_interrupt_handler(Some(Box::new(move || expired(&expiry))));
        let modules = PluginModules::new(folder);
        runtime.set_loader(modules.clone(), modules);
        let context = Context::full(&runtime).map_err(engine_error)?;
        let mut plugin = Self {
            name,
            state,
            registry: Shared::default(),
            exports: None,
            deadline,
            limits,
            context,
        };
        let module = PluginModules::name(folder, main);
        let exports = plugin.context.with(|ctx| {
            plugin.install(&ctx).map_err(|err| describe(&ctx, err))?;
            let namespace = plugin.within_limit(&ctx, || {
                let (module, evaluated) = Module::declare(ctx.clone(), module, source)?.eval()?;
                match evaluated.finish::<()>() {
                    Err(Error::WouldBlock) => Err(Exception::throw_message(
                        &ctx,
                        "the module's top level is still waiting on an await, but the \
                         plugin API is only available from on_load on",
                    )),
                    other => other,
                }?;
                module.namespace()
            })?;
            Ok(Persistent::save(&ctx, namespace))
        });
        plugin.exports = Some(exports.map_err(PluginError::Script)?);
        Ok(plugin)
    }

    /// Gives the runtime the API: `console`, and `@bedrock-rs/core`'s
    /// objects, kept in the runtime's userdata for the module to export.
    fn install<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<()> {
        let logger = self.logger(ctx)?;
        // `console` is the Logger too, as `print` is in Luau.
        let console = Object::new(ctx.clone())?;
        for (name, level) in [
            ("log", "info"),
            ("info", "info"),
            ("debug", "debug"),
            ("trace", "trace"),
            ("warn", "warn"),
            ("error", "error"),
        ] {
            console.set(name, logger.get::<_, Value<'js>>(level)?)?;
        }
        freeze(ctx, &console)?;
        ctx.globals().set("console", console)?;

        let (player, prototype) = self.player_class(ctx)?;
        let (world, instance) = self.world_class(ctx)?;
        {
            let mut registry = self.registry.borrow_mut();
            registry.player = Some(Persistent::save(ctx, prototype));
            registry.world = Some(Persistent::save(ctx, instance));
        }
        let core = Object::new(ctx.clone())?;
        core.set("Logger", logger)?;
        core.set("Server", self.server(ctx)?)?;
        core.set("Player", player)?;
        core.set("World", world)?;
        freeze(ctx, &core)?;
        ctx.store_userdata(core)
            .map_err(|_| Exception::throw_internal(ctx, "the plugin API was set up twice"))?;
        Ok(())
    }

    /// `Logger.trace/debug/info/warn/error(...)`: the arguments, separated by
    /// spaces (see [`display`]).
    fn logger<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
        let logger = Object::new(ctx.clone())?;
        for (name, level) in LEVELS {
            let state = State::clone(&self.state);
            let log = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<()> {
                    let message = args.0.iter().map(display).collect::<Vec<_>>().join(" ");
                    state::lock(&state)
                        .log(level, &message)
                        .map_err(|err| throw(&ctx, err))
                },
            )?
            .with_name(name)?;
            logger.set(name, log)?;
        }
        freeze(ctx, &logger)?;
        Ok(logger)
    }

    /// `Server`: `on`, `broadcast`, `getPlayer`, `getPlayers`,
    /// `registerCommand` and `getWorld`.
    fn server<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
        let server = Object::new(ctx.clone())?;

        let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
        let on = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, event: Opt<Value<'js>>, handler: Opt<Value<'js>>| {
                let event = text_arg(event.0.as_ref(), "on expects an event name")
                    .map_err(|err| throw(&ctx, err))?;
                let handler = handler
                    .0
                    .and_then(Value::into_function)
                    .ok_or_else(|| throw(&ctx, "expected a handler function"))?;
                state::lock(&state)
                    .subscribe(&event)
                    .map_err(|err| throw(&ctx, err))?;
                registry
                    .borrow_mut()
                    .handlers
                    .entry(event)
                    .or_default()
                    .push(Persistent::save(&ctx, handler));
                rquickjs::Result::Ok(())
            },
        )?;
        server.set("on", on.with_name("on")?)?;

        let state = State::clone(&self.state);
        let broadcast = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, message: Opt<Value<'js>>| -> rquickjs::Result<()> {
                let message = text_arg(message.0.as_ref(), "broadcast expects a message string")
                    .map_err(|err| throw(&ctx, err))?;
                state::lock(&state)
                    .broadcast(&message)
                    .map_err(|err| throw(&ctx, err))
            },
        )?;
        server.set("broadcast", broadcast.with_name("broadcast")?)?;

        let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
        let get_player = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, uuid: Opt<Value<'js>>| -> rquickjs::Result<Option<Object<'js>>> {
                let uuid = text_arg(uuid.0.as_ref(), "getPlayer expects a UUID string")
                    .map_err(|err| throw(&ctx, err))?;
                let player = state::lock(&state)
                    .player(&uuid)
                    .map_err(|err| throw(&ctx, err))?;
                player
                    .map(|player| make_player(&ctx, &registry, &player))
                    .transpose()
            },
        )?;
        server.set("getPlayer", get_player.with_name("getPlayer")?)?;

        let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
        let get_players = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Array<'js>> {
                let players = state::lock(&state)
                    .players()
                    .map_err(|err| throw(&ctx, err))?;
                // A fresh array the plugin may sort or filter; the players
                // in it are frozen.
                let list = Array::new(ctx.clone())?;
                for (index, player) in players.iter().enumerate() {
                    list.set(index, make_player(&ctx, &registry, player)?)?;
                }
                Ok(list)
            },
        )?;
        server.set("getPlayers", get_players.with_name("getPlayers")?)?;

        let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
        let register = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, definition: Opt<Value<'js>>| -> rquickjs::Result<()> {
                let definition = definition
                    .0
                    .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                let mut handlers = Vec::new();
                let spec = definition_to_json(&definition, &mut Vec::new(), &mut handlers, false)
                    .and_then(|json| parse_command(&json))
                    .map_err(|err| throw(&ctx, format!("invalid command: {err}")))?;
                let name = spec.name.clone();
                state::lock(&state)
                    .register_command(spec)
                    .map_err(|err| throw(&ctx, err))?;
                let mut registry = registry.borrow_mut();
                for (path, handler) in handlers {
                    registry
                        .commands
                        .insert((name.clone(), path), Persistent::save(&ctx, handler));
                }
                Ok(())
            },
        )?;
        server.set("registerCommand", register.with_name("registerCommand")?)?;

        let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
        let get_world = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
                // Usable from on_load on, like the rest.
                if state::lock(&state).phase == Phase::TopLevel {
                    return Err(throw(&ctx, state::TOP_LEVEL));
                }
                let world = registry.borrow().world.clone();
                world
                    .ok_or_else(|| throw(&ctx, "the world is not available"))?
                    .restore(&ctx)
            },
        )?;
        server.set("getWorld", get_world.with_name("getWorld")?)?;

        freeze(ctx, &server)?;
        Ok(server)
    }

    /// `Player`, whose prototype holds the methods every player object has.
    /// Players come from the server, so calling it is an error; it is there
    /// for `instanceof Player`.
    fn player_class<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<(Function<'js>, Object<'js>)> {
        let prototype = Object::new(ctx.clone())?;
        let method = |name: &'static str,
                      act: fn(&PluginState, &str, &[Value<'js>]) -> Result<(), String>|
         -> rquickjs::Result<Function<'js>> {
            let state = State::clone(&self.state);
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      this: This<Value<'js>>,
                      args: Rest<Value<'js>>|
                      -> rquickjs::Result<()> {
                    let uuid = this
                        .0
                        .as_object()
                        .and_then(|this| this.get::<_, Value<'js>>("uuid").ok())
                        .and_then(|uuid| uuid.as_string()?.to_string().ok())
                        .ok_or_else(|| {
                            throw(
                                &ctx,
                                format!("{name} is a method: call it as player.{name}(...)"),
                            )
                        })?;
                    act(&state::lock(&state), &uuid, &args.0).map_err(|err| throw(&ctx, err))
                },
            )?
            .with_name(name)
        };
        prototype.set(
            "sendMessage",
            method("sendMessage", |state, uuid, args| {
                state.send_message(
                    uuid,
                    &text_arg(args.first(), "sendMessage expects a message string")?,
                )
            })?,
        )?;
        prototype.set(
            "kick",
            method("kick", |state, uuid, args| {
                let reason = optional_text(args.first(), "kick expects a reason string")?;
                state.kick(uuid, reason.as_deref())
            })?,
        )?;
        prototype.set(
            "setGameMode",
            method("setGameMode", |state, uuid, args| {
                state.set_game_mode(
                    uuid,
                    &text_arg(args.first(), "setGameMode expects a game mode name")?,
                )
            })?,
        )?;
        prototype.set(
            "setHealth",
            method("setHealth", |state, uuid, args| {
                state.set_health(
                    uuid,
                    number_arg(args.first(), "setHealth expects a number")?,
                )
            })?,
        )?;
        prototype.set(
            "damage",
            method("damage", |state, uuid, args| {
                let amount = number_arg(args.first(), "damage expects an amount above 0")?;
                let cause = optional_text(args.get(1), "damage expects a cause string")?;
                state.damage(uuid, amount, cause.as_deref())
            })?,
        )?;
        let class = not_constructible(ctx, "Player", "players come from the server")?;
        link(ctx, &class, &prototype)?;
        Ok((class, prototype))
    }

    /// `World` and the one world, `Server.getWorld()`: its scheduler.
    /// `run(callback)`, `runTimeout(callback, ticks = 1)`,
    /// `runInterval(callback, ticks = 1)`, `clearRun(id)` and
    /// `waitTicks(ticks = 1)`, a Promise.
    fn world_class<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<(Function<'js>, Object<'js>)> {
        let prototype = Object::new(ctx.clone())?;

        let schedule = |name: &'static str,
                        repeats: bool,
                        default: Option<f64>|
         -> rquickjs::Result<Function<'js>> {
            let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      callback: Opt<Value<'js>>,
                      ticks: Opt<Value<'js>>|
                      -> rquickjs::Result<u32> {
                    let callback = callback
                        .0
                        .and_then(Value::into_function)
                        .ok_or_else(|| throw(&ctx, "expected a callback function"))?;
                    // `run` takes no delay: it is always the next tick.
                    let ticks = match (default, given(ticks.0)) {
                        (None, _) => 0.0,
                        (Some(default), None) => default,
                        (Some(_), Some(ticks)) => {
                            number_arg(Some(&ticks), "ticks must be a number")
                                .map_err(|err| throw(&ctx, err))?
                        }
                    };
                    let id = state::lock(&state)
                        .schedule(ticks, repeats.then_some(ticks))
                        .map_err(|err| throw(&ctx, err))?;
                    registry
                        .borrow_mut()
                        .tasks
                        .insert(id, Persistent::save(&ctx, callback));
                    Ok(id)
                },
            )?
            .with_name(name)
        };
        prototype.set("run", schedule("run", false, None)?)?;
        prototype.set("runTimeout", schedule("runTimeout", false, Some(1.0))?)?;
        prototype.set("runInterval", schedule("runInterval", true, Some(1.0))?)?;

        let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
        let clear = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, id: Opt<Value<'js>>| -> rquickjs::Result<()> {
                let id = number_arg(id.0.as_ref(), "clearRun expects a task id")
                    .map_err(|err| throw(&ctx, err))?;
                state::lock(&state)
                    .clear_run(id)
                    .map_err(|err| throw(&ctx, err))?;
                if id.fract() == 0.0 && (1.0..=f64::from(u32::MAX)).contains(&id) {
                    registry.borrow_mut().tasks.remove(&(id as u32));
                }
                Ok(())
            },
        )?;
        prototype.set("clearRun", clear.with_name("clearRun")?)?;

        // A Promise that a one-off task resolves.
        let (state, registry) = (State::clone(&self.state), Shared::clone(&self.registry));
        let wait = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, ticks: Opt<Value<'js>>| -> rquickjs::Result<Promise<'js>> {
                let ticks = match given(ticks.0) {
                    None => 1.0,
                    Some(ticks) => number_arg(Some(&ticks), "ticks must be a number")
                        .map_err(|err| throw(&ctx, err))?,
                };
                let id = state::lock(&state)
                    .schedule(ticks, None)
                    .map_err(|err| throw(&ctx, err))?;
                let (promise, resolve, _) = ctx.promise()?;
                registry
                    .borrow_mut()
                    .tasks
                    .insert(id, Persistent::save(&ctx, resolve));
                Ok(promise)
            },
        )?;
        prototype.set("waitTicks", wait.with_name("waitTicks")?)?;

        let class = not_constructible(ctx, "World", "the world is Server.getWorld()")?;
        link(ctx, &class, &prototype)?;
        let world = Object::new(ctx.clone())?;
        world.set_prototype(Some(&prototype))?;
        freeze(ctx, &world)?;
        Ok((class, world))
    }

    /// Runs `run` within the execution time limit, then the promise jobs it
    /// queued, within the same limit.
    fn within_limit<'js, T>(
        &self,
        ctx: &Ctx<'js>,
        run: impl FnOnce() -> rquickjs::Result<T>,
    ) -> Result<T, String> {
        self.deadline
            .set(Some(Instant::now() + self.limits.execution));
        let result = run().map_err(|err| describe(ctx, err));
        // A job that fails rejects the promise it belongs to, which reports
        // it; one stopped by the time limit is caught below.
        while ctx.execute_pending_job() {}
        let timed_out = expired(&self.deadline);
        self.deadline.set(None);
        if timed_out {
            return Err(TIME_LIMIT.into());
        }
        result
    }

    /// Calls `function` (a lifecycle function, handler or task): what it
    /// returned, or for an `async` one, what its Promise resolved to. `None`
    /// while that Promise is still waiting; if it fails later, the failure is
    /// reported as `what`'s.
    fn call<'js>(
        &self,
        ctx: &Ctx<'js>,
        what: &str,
        function: &Function<'js>,
        args: impl IntoArgs<'js>,
    ) -> Result<Option<Value<'js>>, String> {
        let value = self.within_limit(ctx, || function.call::<_, Value<'js>>(args))?;
        let Some(promise) = value.as_promise() else {
            return Ok(Some(value));
        };
        match promise.state() {
            PromiseState::Resolved | PromiseState::Rejected => promise
                .result::<Value<'js>>()
                .transpose()
                .map_err(|err| describe(ctx, err)),
            PromiseState::Pending => {
                let state = State::clone(&self.state);
                let what = what.to_owned();
                let report = Function::new(ctx.clone(), move |reason: Value<'js>| {
                    state::lock(&state).report_failure(&what, &describe_value(&reason));
                })
                .map_err(|err| describe(ctx, err))?;
                promise
                    .catch()
                    .and_then(|catch| catch.call::<_, Value<'js>>((This(promise.clone()), report)))
                    .map_err(|err| describe(ctx, err))?;
                Ok(None)
            }
        }
    }

    /// Calls the lifecycle function `name`, if the plugin exports one.
    fn lifecycle(&mut self, name: &str) -> Result<(), PluginError> {
        let Some(exports) = self.exports.clone() else {
            return Ok(());
        };
        self.context
            .with(|ctx| {
                let exports = exports.restore(&ctx).map_err(|err| describe(&ctx, err))?;
                let value = exports
                    .get::<_, Value<'_>>(name)
                    .map_err(|err| describe(&ctx, err))?;
                if value.is_undefined() {
                    return Ok(());
                }
                let function = value.clone().into_function().ok_or_else(|| {
                    format!("{name} must be a function, not a {}", type_of(&value))
                })?;
                self.call(&ctx, name, &function, ()).map(drop)
            })
            .map_err(PluginError::Script)
    }

    fn report(&self, what: &str, error: &str) {
        state::lock(&self.state).report_failure(what, error);
    }

    /// The event handlers receive (see [`wire::event`]), with `cancel()` and
    /// `isCancelled()` for cancellable events; frozen.
    fn event_payload<'js>(
        &self,
        ctx: &Ctx<'js>,
        event: &Event,
        cancelled: &Rc<Cell<bool>>,
    ) -> rquickjs::Result<Object<'js>> {
        let json = wire::event(event);
        let payload = Object::new(ctx.clone())?;
        if let Some(Json::Object(data)) = json.get("data") {
            for (key, value) in data {
                payload.set(key.as_str(), json_to_js(ctx, &self.registry, value)?)?;
            }
        }
        if event.is_cancellable() {
            let cancel = Rc::clone(cancelled);
            payload.set(
                "cancel",
                Function::new(ctx.clone(), move || cancel.set(true))?.with_name("cancel")?,
            )?;
            let cancel = Rc::clone(cancelled);
            payload.set(
                "isCancelled",
                Function::new(ctx.clone(), move || cancel.get())?.with_name("isCancelled")?,
            )?;
        }
        freeze(ctx, &payload)?;
        Ok(payload)
    }

    /// The `ctx` command handlers receive: `sender` (the player, or
    /// `undefined` for the console), `console`, `command`, `path` (the
    /// subcommands typed), `args` by name, and `reply(message)` /
    /// `error(message)` to answer. A string the handler returns (or its
    /// Promise resolves to) is replied too. Replies once the handler has
    /// finished, after an `await`, reach a player as chat and the console as
    /// a log line.
    fn command_context<'js>(
        &self,
        ctx: &Ctx<'js>,
        call: &CommandCall,
        reply: &Rc<RefCell<CommandReply>>,
        finished: &Rc<Cell<bool>>,
    ) -> rquickjs::Result<Object<'js>> {
        let json = wire::command_call(call);
        let context = Object::new(ctx.clone())?;
        context.set("sender", json_to_js(ctx, &self.registry, &json["sender"])?)?;
        context.set("console", call.sender == CommandSender::Console)?;
        context.set("command", call.command.as_str())?;
        context.set("path", json_to_js(ctx, &self.registry, &json["path"])?)?;
        context.set("args", json_to_js(ctx, &self.registry, &json["args"])?)?;
        for (name, success) in [("reply", true), ("error", false)] {
            let (reply, finished, state) = (
                Rc::clone(reply),
                Rc::clone(finished),
                State::clone(&self.state),
            );
            let sender = call.sender.clone();
            let respond = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, message: Opt<Value<'js>>| -> rquickjs::Result<()> {
                    let text = text_arg(message.0.as_ref(), "expected a message string")
                        .ok()
                        .filter(|text| !text.is_empty())
                        .ok_or_else(|| throw(&ctx, "expected a message string"))?;
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
                        .map_err(|err| throw(&ctx, err))
                },
            )?;
            context.set(name, respond.with_name(name)?)?;
        }
        freeze(ctx, &context)?;
        Ok(context)
    }
}

impl Drop for JavaScriptPlugin {
    fn drop(&mut self) {
        // The runtime aborts if it is freed while values are still held from
        // Rust, so let them all go first. (The API's closures hold the
        // registry too, but they are freed with the runtime, after this.)
        self.exports = None;
        if let Ok(mut registry) = self.registry.try_borrow_mut() {
            *registry = Registry::default();
        }
    }
}

impl Plugin for JavaScriptPlugin {
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
        let Some(handlers) = self.registry.borrow().handlers.get(event.name()).cloned() else {
            return false;
        };
        let cancelled = Rc::new(Cell::new(false));
        let what = format!("{} handler", event.name());
        self.context.with(|ctx| {
            let payload = match self.event_payload(&ctx, event, &cancelled) {
                Ok(payload) => payload,
                Err(err) => {
                    self.report(&format!("{} event", event.name()), &describe(&ctx, err));
                    return;
                }
            };
            for handler in handlers {
                let result = handler
                    .restore(&ctx)
                    .map_err(|err| describe(&ctx, err))
                    .and_then(|handler| self.call(&ctx, &what, &handler, (payload.clone(),)));
                if let Err(err) = result {
                    self.report(&what, &err);
                }
            }
        });
        cancelled.get()
    }

    fn run_command(&mut self, call: &CommandCall) -> CommandReply {
        let key = (call.command.clone(), call.path.join(" "));
        let Some(handler) = self.registry.borrow().commands.get(&key).cloned() else {
            return CommandReply::error(format!("/{} is no longer available", call.command));
        };
        let what = format!("/{} command", call.command);
        let reply = Rc::new(RefCell::new(CommandReply::default()));
        let finished = Rc::new(Cell::new(false));
        let result = self.context.with(|ctx| {
            let handler = handler.restore(&ctx).map_err(|err| describe(&ctx, err))?;
            let context = self
                .command_context(&ctx, call, &reply, &finished)
                .map_err(|err| describe(&ctx, err))?;
            let returned = self.call(&ctx, &what, &handler, (context,))?;
            Ok::<_, String>(returned.and_then(|value| value.as_string()?.to_string().ok()))
        });
        finished.set(true);
        let mut reply = reply.take();
        match result {
            Ok(Some(text)) => reply.push_ok(text),
            // Nothing, or still waiting: what it replies from now on is late.
            Ok(None) => {}
            Err(err) => {
                self.report(&what, &err);
                reply.push_error(HANDLER_FAILED);
            }
        }
        reply
    }

    fn run_tasks(&mut self, ids: &[u32]) {
        self.context.with(|ctx| {
            for id in ids {
                let task = {
                    let mut registry = self.registry.borrow_mut();
                    // A one-off is done once it runs.
                    if state::lock(&self.state).tasks.contains(*id) {
                        registry.tasks.get(id).cloned()
                    } else {
                        registry.tasks.remove(id)
                    }
                };
                let Some(task) = task else {
                    continue;
                };
                let result = task
                    .restore(&ctx)
                    .map_err(|err| describe(&ctx, err))
                    .and_then(|task| self.call(&ctx, "scheduled task", &task, ()));
                if let Err(err) = result {
                    self.report("scheduled task", &err);
                }
            }
        });
    }
}

/// Compiles a plugin's main module and everything it imports, without
/// running any of it: what a reload checks before it stops the version that
/// runs. Declaring a module compiles it and loads its imports, through the
/// same loader the plugin runs with, so a missing file fails here too.
pub(crate) fn check(
    folder: &Path,
    source: &PluginSource,
    limits: Limits,
) -> Result<(), PluginError> {
    let runtime = Runtime::new().map_err(engine_error)?;
    runtime.set_memory_limit(limits.memory);
    runtime.set_max_stack_size(STACK_SIZE);
    let modules = PluginModules::new(folder);
    runtime.set_loader(modules.clone(), modules);
    let context = Context::full(&runtime).map_err(engine_error)?;
    let name = PluginModules::name(folder, Path::new(&source.manifest.main));
    context
        .with(|ctx| {
            Module::declare(ctx.clone(), name, source.source.clone())
                .map(drop)
                .map_err(|err| describe(&ctx, err))
        })
        .map_err(PluginError::Script)
}

fn engine_error(error: Error) -> PluginError {
    PluginError::Script(format!("couldn't start the JavaScript engine: {error}"))
}

fn expired(deadline: &Cell<Option<Instant>>) -> bool {
    deadline
        .get()
        .is_some_and(|deadline| Instant::now() >= deadline)
}

/// An error thrown into the plugin's code, as `new Error(message)`.
fn throw(ctx: &Ctx<'_>, message: impl AsRef<str>) -> Error {
    Exception::throw_message(ctx, message.as_ref())
}

/// What went wrong in a call into the engine, taking the exception it threw.
fn describe(ctx: &Ctx<'_>, error: Error) -> String {
    match error {
        Error::Exception => describe_value(&ctx.catch()),
        other => other.to_string(),
    }
}

/// A thrown value as the log shows it: `TypeError: message` and its stack
/// for errors, anything else as `String(value)` would show it.
fn describe_value(value: &Value<'_>) -> String {
    let Some(error) = value.as_object().filter(|_| value.is_error()) else {
        return display(value);
    };
    let field = |name: &str| {
        error
            .get::<_, Option<Coerced<String>>>(name)
            .ok()
            .flatten()
            .map(|text| text.0)
    };
    let mut text = match (field("name"), field("message")) {
        (Some(name), Some(message)) if !message.is_empty() => format!("{name}: {message}"),
        (Some(name), _) => name,
        (None, message) => message.unwrap_or_else(|| "Error".into()),
    };
    if let Some(stack) = field("stack").filter(|stack| !stack.trim().is_empty()) {
        text.push('\n');
        text.push_str(stack.trim_end());
    }
    text
}

/// A value as `Logger` writes it: strings as they are, errors with their
/// stack, objects and arrays as JSON, anything else as `String(value)`.
fn display(value: &Value<'_>) -> String {
    if let Some(text) = value.as_string() {
        return text.to_string().unwrap_or_default();
    }
    if value.is_error() {
        return describe_value(value);
    }
    let ctx = value.ctx();
    if value.is_object() && !value.is_function() {
        match ctx.json_stringify(value.clone()) {
            Ok(Some(json)) => {
                if let Ok(json) = json.to_string() {
                    return json;
                }
            }
            Ok(None) => {}
            // A cycle, or a BigInt: shown as String() shows it instead.
            Err(_) => drop(ctx.catch()),
        }
    }
    match value.get::<Coerced<String>>() {
        Ok(text) => text.0,
        Err(_) => {
            ctx.catch();
            format!("[{}]", type_of(value))
        }
    }
}

/// `typeof value`.
fn type_of(value: &Value<'_>) -> &'static str {
    if value.is_undefined() {
        "undefined"
    } else if value.is_null() {
        "null"
    } else if value.is_bool() {
        "boolean"
    } else if value.is_number() {
        "number"
    } else if value.is_string() {
        "string"
    } else if value.is_symbol() {
        "symbol"
    } else if value.is_big_int() {
        "bigint"
    } else if value.is_function() {
        "function"
    } else {
        "object"
    }
}

/// An argument that was given: not left out, `undefined` or `null`.
fn given(value: Option<Value<'_>>) -> Option<Value<'_>> {
    value.filter(|value| !(value.is_undefined() || value.is_null()))
}

fn text_arg(value: Option<&Value<'_>>, message: &str) -> Result<String, String> {
    value
        .and_then(Value::as_string)
        .and_then(|text| text.to_string().ok())
        .ok_or_else(|| message.to_owned())
}

fn optional_text(value: Option<&Value<'_>>, message: &str) -> Result<Option<String>, String> {
    match value {
        Some(value) if !(value.is_undefined() || value.is_null()) => {
            text_arg(Some(value), message).map(Some)
        }
        _ => Ok(None),
    }
}

fn number_arg(value: Option<&Value<'_>>, message: &str) -> Result<f64, String> {
    value
        .and_then(Value::as_number)
        .ok_or_else(|| message.to_owned())
}

/// `Object.freeze(object)`.
fn freeze<'js>(ctx: &Ctx<'js>, object: &Object<'js>) -> rquickjs::Result<()> {
    ctx.globals()
        .get::<_, Object<'js>>("Object")?
        .get::<_, Function<'js>>("freeze")?
        .call::<_, Value<'js>>((object.clone(),))
        .map(drop)
}

/// A class the API hands out instances of but plugins cannot make: calling
/// it is an error saying `why`.
fn not_constructible<'js>(
    ctx: &Ctx<'js>,
    name: &'static str,
    why: &'static str,
) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<()> {
        Err(Exception::throw_type(
            &ctx,
            &format!("{name} can't be made: {why}"),
        ))
    })?
    .with_name(name)
}

/// Makes `prototype` the class's, as `class` syntax would, and freezes both.
fn link<'js>(
    ctx: &Ctx<'js>,
    class: &Function<'js>,
    prototype: &Object<'js>,
) -> rquickjs::Result<()> {
    class.set("prototype", prototype.clone())?;
    prototype.set("constructor", class.clone())?;
    freeze(ctx, prototype)?;
    freeze(ctx, class)
}

/// A player object: frozen `{ name, uuid }`, with `Player`'s methods.
fn make_player<'js>(
    ctx: &Ctx<'js>,
    registry: &Shared,
    player: &Player,
) -> rquickjs::Result<Object<'js>> {
    let prototype = registry
        .borrow()
        .player
        .clone()
        .ok_or_else(|| throw(ctx, "players are not available"))?
        .restore(ctx)?;
    let object = Object::new(ctx.clone())?;
    object.set_prototype(Some(&prototype))?;
    object.set("name", player.name.as_str())?;
    object.set("uuid", player.uuid.as_str())?;
    freeze(ctx, &object)?;
    Ok(object)
}

/// JSON from the server as frozen JavaScript values, with players (an object
/// with a `name` and a `uuid`) as player objects and `null` as `undefined`.
fn json_to_js<'js>(ctx: &Ctx<'js>, registry: &Shared, json: &Json) -> rquickjs::Result<Value<'js>> {
    Ok(match json {
        Json::Null => Value::new_undefined(ctx.clone()),
        Json::Bool(value) => Value::new_bool(ctx.clone(), *value),
        Json::Number(number) => match number.as_i64().and_then(|int| i32::try_from(int).ok()) {
            Some(int) => Value::new_int(ctx.clone(), int),
            None => Value::new_float(ctx.clone(), number.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(text) => rquickjs::String::from_str(ctx.clone(), text)?.into_value(),
        Json::Array(items) => {
            let array = Array::new(ctx.clone())?;
            for (index, item) in items.iter().enumerate() {
                array.set(index, json_to_js(ctx, registry, item)?)?;
            }
            freeze(ctx, array.as_object())?;
            array.into_value()
        }
        Json::Object(map) if wire::is_player(json) => {
            let player = Player {
                name: map["name"].as_str().unwrap_or_default().to_owned(),
                uuid: map["uuid"].as_str().unwrap_or_default().to_owned(),
            };
            make_player(ctx, registry, &player)?.into_value()
        }
        Json::Object(map) => {
            let object = Object::new(ctx.clone())?;
            for (key, value) in map {
                object.set(key.as_str(), json_to_js(ctx, registry, value)?)?;
            }
            freeze(ctx, &object)?;
            object.into_value()
        }
    })
}

/// A command definition as JSON for [`parse_command`], with each `run`
/// function taken out into `handlers` under its subcommand path.
fn definition_to_json<'js>(
    value: &Value<'js>,
    path: &mut Vec<String>,
    handlers: &mut Vec<(String, Function<'js>)>,
    in_subcommands: bool,
) -> Result<Json, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(Json::Null);
    }
    if let Some(value) = value.as_bool() {
        return Ok(Json::Bool(value));
    }
    if let Some(int) = value.as_int() {
        return Ok(Json::from(int));
    }
    if let Some(number) = value.as_float() {
        return serde_json::Number::from_f64(number)
            .map(Json::Number)
            .ok_or_else(|| "numbers must be finite".into());
    }
    if let Some(text) = value.as_string() {
        return text
            .to_string()
            .map(Json::String)
            .map_err(|err| err.to_string());
    }
    if value.is_function() {
        return Err("functions are only allowed as run handlers".into());
    }
    if let Some(array) = value.as_array() {
        let mut items = Vec::new();
        for item in array.iter::<Value<'js>>() {
            let item = item.map_err(|err| err.to_string())?;
            items.push(definition_to_json(&item, path, handlers, false)?);
        }
        return Ok(Json::Array(items));
    }
    let Some(object) = value.as_object() else {
        return Err(format!("a {} cannot be part of a command", type_of(value)));
    };
    let mut map = serde_json::Map::new();
    for property in object.props::<String, Value<'js>>() {
        let (key, value) = property.map_err(|err| err.to_string())?;
        let json = if in_subcommands {
            // Each entry is a subcommand node, named by its key.
            path.push(key.clone());
            let node = definition_to_json(&value, path, handlers, false);
            path.pop();
            node?
        } else if key == "run" {
            match value.clone().into_function() {
                Some(handler) => {
                    handlers.push((path.join(" "), handler));
                    Json::Bool(true)
                }
                None => definition_to_json(&value, path, handlers, false)?,
            }
        } else {
            definition_to_json(&value, path, handlers, key == "subcommands")?
        };
        map.insert(key, json);
    }
    Ok(Json::Object(map))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;
    use crate::state::Shared as Common;
    use crate::{Action, ArgValue};

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
        let shared = Common::new(
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

    fn load(setup: &Setup, source: &str) -> Result<JavaScriptPlugin, PluginError> {
        load_from(setup, Path::new("."), source)
    }

    fn load_from(
        setup: &Setup,
        folder: &Path,
        source: &str,
    ) -> Result<JavaScriptPlugin, PluginError> {
        JavaScriptPlugin::new(
            State::clone(&setup.state),
            folder,
            Path::new("index.js"),
            source,
            LIMITS,
        )
    }

    /// Runs a loaded plugin up to enabled, as the manager would.
    fn enable(setup: &Setup, mut plugin: JavaScriptPlugin) -> JavaScriptPlugin {
        state::lock(&setup.state).phase = Phase::Loading;
        plugin.on_load().unwrap();
        state::lock(&setup.state).phase = Phase::Enabled;
        plugin.on_enable().unwrap();
        plugin
    }

    fn enabled(setup: &Setup, source: &str) -> JavaScriptPlugin {
        enable(setup, load(setup, source).unwrap())
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
    fn tick(setup: &Setup, plugin: &mut JavaScriptPlugin, until: u64) {
        let clock = Arc::clone(&state::lock(&setup.state).shared().clock);
        for now in clock.load(std::sync::atomic::Ordering::Relaxed) + 1..=until {
            clock.store(now, std::sync::atomic::Ordering::Relaxed);
            let due = state::lock(&setup.state).tasks.take_due(now);
            plugin.run_tasks(&due);
        }
    }

    fn command(path: &[&str], args: Vec<(&str, ArgValue)>) -> CommandCall {
        CommandCall {
            plugin: "test".into(),
            command: "warp".into(),
            path: path.iter().map(|name| (*name).to_owned()).collect(),
            args: args
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
            sender: CommandSender::Player(steve()),
        }
    }

    #[test]
    fn lifecycle_functions_run_in_order_with_the_api() {
        let setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                import { Logger } from "@bedrock-rs/core";
                const greeting = "hello";
                export function on_load() { Logger.info(greeting, "load"); }
                export function on_enable() { console.log("enable"); }
                export function on_disable() { Logger.warn("disable", 1, { a: [true] }); }
            "#,
        );
        plugin.on_disable().unwrap();
        assert_eq!(
            messages(&setup),
            ["hello load", "enable", r#"disable 1 {"a":[true]}"#]
        );
    }

    #[test]
    fn the_top_level_cannot_use_the_api() {
        let setup = setup();
        for source in [
            r#"import { Logger } from "@bedrock-rs/core"; Logger.info("too early");"#,
            r#"console.log("too early");"#,
            r#"import { Server } from "@bedrock-rs/core"; Server.getWorld();"#,
        ] {
            let err = load(&setup, source).err().unwrap().to_string();
            assert!(err.contains("not available at the top level"), "{err}");
        }
        // A module need not export anything.
        let mut plugin = load(&setup, "const x = 1;").unwrap();
        assert!(plugin.on_load().is_ok());
        let mut plugin = load(&setup, "export const on_load = 5;").unwrap();
        let err = plugin.on_load().unwrap_err().to_string();
        assert!(
            err.contains("on_load must be a function, not a number"),
            "{err}"
        );
        let err = load(&setup, "export function on_load( {")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("SyntaxError"), "{err}");
        let err = load(&setup, r#"import x from "lodash";"#)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("bundle anything else"), "{err}");
    }

    #[test]
    fn commands_register_in_on_load_only_and_run() {
        let setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                import { Server, Player } from "@bedrock-rs/core";
                export function on_load() {
                    Server.registerCommand({
                        name: "warp",
                        args: [{ name: "name", type: "string" }],
                        run: (ctx) => { ctx.reply(`to ${ctx.args.name}`); },
                        subcommands: {
                            set: { run: (ctx) => `set by ${ctx.sender.name}` },
                            who: { run: (ctx) => String(ctx.sender instanceof Player && !ctx.console) },
                            later: { run: async () => { await null; return "awaited"; } },
                            broken: { run: () => { throw new Error("oops"); } },
                        },
                    });
                }
            "#,
        );
        let commands = state::lock(&setup.state).commands.clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "warp");
        let reply = plugin.run_command(&command(
            &[],
            vec![("name", ArgValue::String("spawn".into()))],
        ));
        assert_eq!(reply, CommandReply::ok("to spawn"));
        for (path, expected) in [
            ("set", CommandReply::ok("set by Steve")),
            ("who", CommandReply::ok("true")),
            ("later", CommandReply::ok("awaited")),
            ("broken", CommandReply::error(HANDLER_FAILED)),
        ] {
            assert_eq!(
                plugin.run_command(&command(&[path], Vec::new())),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn on_enable_cannot_register_commands() {
        let setup = setup();
        let mut plugin = load(
            &setup,
            r#"
                import { Server } from "@bedrock-rs/core";
                export function on_enable() { Server.registerCommand({ name: "x", run() {} }); }
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
                import { Server } from "@bedrock-rs/core";
                export function on_load() {
                    Server.on("player_chat", (event) => {
                        if (event.message.includes("badword")) event.cancel();
                    });
                    Server.on("player_join", (event) => {
                        event.player.sendMessage(`hi ${event.player.name}`);
                    });
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
                import { Logger, Server } from "@bedrock-rs/core";
                export async function on_enable() {
                    const world = Server.getWorld();
                    world.run(() => Logger.info("next tick"));
                    world.runTimeout(() => Logger.info("after 5"), 5);
                    const interval = world.runInterval(() => Logger.info("every 3"), 3);
                    Logger.info("waiting");
                    await world.waitTicks(4);
                    Logger.info("waited 4");
                    world.clearRun(interval);
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
                import { Logger, Server } from "@bedrock-rs/core";
                export function on_load() {
                    Server.on("player_join", () => { throw new Error("first"); });
                    Server.on("player_join", () => Logger.info("second"));
                }
                export function on_enable() {
                    const world = Server.getWorld();
                    world.run(() => { throw new Error("task"); });
                    world.run(async () => { await world.waitTicks(1); throw new Error("later"); });
                }
            "#,
        );
        plugin.dispatch(&Event::PlayerJoin(steve()));
        tick(&setup, &mut plugin, 3);
        assert_eq!(messages(&setup), ["second"]);
    }

    #[test]
    fn runaway_scripts_are_stopped() {
        let setup = setup();
        let err = load(&setup, "while (true) {}").err().unwrap().to_string();
        assert!(err.contains(TIME_LIMIT), "{err}");
        for source in [
            "export function on_load() { while (true) {} }",
            "export async function on_load() { while (true) await null; }",
            "export function on_load() { const a = []; while (true) a.push(new Array(1e5).fill(1)); }",
        ] {
            let mut plugin = load(&setup, source).unwrap();
            state::lock(&setup.state).phase = Phase::Loading;
            let err = plugin.on_load().unwrap_err().to_string();
            assert!(
                err.contains(TIME_LIMIT) || err.contains("out of memory"),
                "{source}: {err}"
            );
        }
    }

    #[test]
    fn the_api_is_read_only_and_players_have_methods() {
        let mut setup = setup();
        let mut plugin = enabled(
            &setup,
            r#"
                import * as Core from "@bedrock-rs/core";
                const { Server, Player } = Core;
                const fails = (what) => { try { what(); return false; } catch { return true; } };
                export function on_load() {
                    Server.on("player_join", (event) => {
                        if (!fails(() => { Server.broadcast = null; })) throw "Server is writable";
                        if (!fails(() => { event.player.name = "Alex"; })) throw "players are writable";
                        if (!fails(() => new Player())) throw "Player can be made";
                        if (!(event.player instanceof Player)) throw "not a Player";
                        const send = event.player.sendMessage;
                        try { send("x"); throw "unbound method worked"; } catch (err) {
                            if (!String(err.message).includes("is a method")) throw err;
                        }
                        Player.prototype.kick.call(event.player);
                    });
                }
            "#,
        );
        plugin.dispatch(&Event::PlayerJoin(steve()));
        assert!(messages(&setup).is_empty(), "{:?}", messages(&setup));
        assert!(matches!(setup.actions.try_recv(), Ok(Action::Kick { .. })));
    }

    #[test]
    fn checking_compiles_the_module_and_its_imports_without_running_them() {
        let folder =
            std::env::temp_dir().join(format!("bedrockrs-js-check-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(folder.join("lib")).unwrap();
        let write = |file: &str, contents: &str| fs::write(folder.join(file), contents).unwrap();
        write(
            "plugin.json",
            r#"{"name":"checked","description":"","version":"1","author":"","main":"index.js"}"#,
        );
        // Running this would fail at once; checking does not run it.
        write(
            "index.js",
            r#"import { Logger } from "@bedrock-rs/core";
               import { x } from "./lib/x.js";
               import config from "./config.json";
               throw new Error("ran");"#,
        );
        write("lib/x.js", "export const x = 1;");
        write("config.json", r#"{ "a": 1 }"#);
        let check_folder = || {
            let source = PluginSource::read(&folder).unwrap().unwrap();
            check(&folder, &source, LIMITS).map_err(|err| err.to_string())
        };
        assert_eq!(check_folder(), Ok(()));

        write("lib/x.js", "export const x = ;");
        let err = check_folder().unwrap_err();
        assert!(
            err.contains("SyntaxError") && err.contains("lib/x.js"),
            "{err}"
        );
        write("lib/x.js", "export const x = 1;");
        write("config.json", r#"{ "a": }"#);
        let err = check_folder().unwrap_err();
        assert!(err.contains("config.json"), "{err}");
        write("config.json", r#"{ "a": 1 }"#);
        fs::remove_file(folder.join("lib/x.js")).unwrap();
        let err = check_folder().unwrap_err();
        assert!(err.contains("no such file"), "{err}");
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn modules_import_each_other_from_the_plugins_folder() {
        let folder =
            std::env::temp_dir().join(format!("bedrockrs-js-imports-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(folder.join("lib")).unwrap();
        fs::write(
            folder.join("lib").join("greet.js"),
            r#"import { Logger } from "@bedrock-rs/core";
               import config from "../config.json";
               export const greet = (who) => Logger.info(`${config.greeting} ${who}`);"#,
        )
        .unwrap();
        fs::write(folder.join("config.json"), r#"{ "greeting": "hello" }"#).unwrap();
        let setup = setup();
        let plugin = load_from(
            &setup,
            &folder,
            r#"import { greet } from "./lib/greet.js";
               export function on_load() { greet("world"); }"#,
        )
        .unwrap();
        enable(&setup, plugin);
        assert_eq!(messages(&setup), ["hello world"]);
        fs::remove_dir_all(&folder).unwrap();
    }
}
