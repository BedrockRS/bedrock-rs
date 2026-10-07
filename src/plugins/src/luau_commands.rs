//! Slash commands for Luau plugins: `server.command(definition)` and running
//! the handlers it registers.
//!
//! A definition is a table describing the command and, at any depth, its
//! subcommands:
//!
//! ```lua
//! server.command({
//!     name = "warp",
//!     description = "Travel between warps",
//!     aliases = { "w" },
//!     permission = "any",            -- or "operator"; the default is "any"
//!     args = { { name = "name", type = "string" } },
//!     run = function(ctx) ... end,   -- /warp <name>
//!     subcommands = {
//!         set = {
//!             description = "Make a warp where you stand",
//!             args = { { name = "name", type = "string" } },
//!             run = function(ctx) ... end,
//!         },
//!         admin = {
//!             permission = "operator",
//!             subcommands = { reload = { run = function(ctx) ... end } },
//!         },
//!     },
//! })
//! ```
//!
//! Argument types: `string` (a word or a quoted string), `text` (the rest of
//! the line), `int`, `number`, `bool`, `player`, `gamemode`, and `enum` with
//! `values = { ... }` (and optionally `enum = "TypeName"`). A `gamemode` takes
//! vanilla's names, `default` included. Arguments may be
//! `optional = true`, after the required ones.
//!
//! Handlers receive a `ctx` table: `sender` (the player, or `nil` for the
//! console), `console`, `command`, `path` (the subcommands typed), `args` by
//! name, and `reply(message)` / `error(message)` to answer. A string a handler
//! returns is replied too.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mlua::{Function, IntoLua as _, Lua, MultiValue, Table, Value};
use tokio::sync::mpsc;

use crate::luau::{method_args, player_table, request};
use crate::{
    Action, ArgKind, ArgSpec, ArgValue, CommandCall, CommandNode, CommandReply, CommandSender,
    CommandSpec, Permission,
};

/// Registry key of each VM's command handlers: command name → path → function.
const HANDLERS: &str = "bedrockrs.command_handlers";

/// What a failing handler's player is told; the details go to the log.
const HANDLER_FAILED: &str = "An error occurred while running this command.";

/// The commands one plugin registered, in order.
pub(crate) type Registered = Rc<RefCell<Vec<CommandSpec>>>;

/// Adds `server.command(definition)` to `server`, recording commands in
/// `registered` and their handlers in the VM.
pub(crate) fn install(lua: &Lua, server: &Table, registered: &Registered) -> mlua::Result<()> {
    lua.set_named_registry_value(HANDLERS, lua.create_table()?)?;
    let registered = Rc::clone(registered);
    let command = lua.create_function(move |lua, definition: Table| {
        let handlers = lua.create_table()?;
        let spec = parse_command(&definition, &handlers)
            .map_err(|err| mlua::Error::runtime(format!("invalid command: {err}")))?;
        if registered
            .borrow()
            .iter()
            .any(|other| other.name == spec.name)
        {
            return Err(mlua::Error::runtime(format!(
                "this plugin already registered /{}",
                spec.name
            )));
        }
        let all: Table = lua.named_registry_value(HANDLERS)?;
        all.raw_set(spec.name.as_str(), handlers)?;
        registered.borrow_mut().push(spec);
        Ok(())
    })?;
    server.set("command", command)
}

/// Runs the handler for `call`, collecting what it replies.
pub(crate) fn run(lua: &Lua, call: &CommandCall) -> mlua::Result<CommandReply> {
    let handler = lua
        .named_registry_value::<Table>(HANDLERS)?
        .raw_get::<Option<Table>>(call.command.as_str())?
        .map(|paths| paths.raw_get::<Option<Function>>(call.path.join(" ")))
        .transpose()?
        .flatten();
    let Some(handler) = handler else {
        return Ok(CommandReply::error(format!(
            "/{} is no longer available.",
            call.command
        )));
    };

    let reply = Rc::new(RefCell::new(CommandReply::default()));
    let finished = Rc::new(Cell::new(false));
    let ctx = context(lua, call, &reply, &finished)?;
    let result = handler.call::<Value>(ctx);
    finished.set(true);
    let mut reply = reply.take();
    match result {
        Ok(Value::String(text)) => reply.push_ok(text.to_str()?.to_owned()),
        Ok(_) => {}
        Err(err) => {
            tracing::error!(
                "Plugin {} failed running /{}: {err}",
                call.plugin,
                call.command
            );
            reply.push_error(HANDLER_FAILED);
        }
    }
    Ok(reply)
}

/// The `ctx` table handlers receive. Replies made after the handler returned
/// (from a later event, say) reach a player as a chat message, and the
/// console as a log line.
fn context(
    lua: &Lua,
    call: &CommandCall,
    reply: &Rc<RefCell<CommandReply>>,
    finished: &Rc<Cell<bool>>,
) -> mlua::Result<Table> {
    let ctx = lua.create_table()?;
    let (sender, console) = match &call.sender {
        CommandSender::Console => (Value::Nil, true),
        CommandSender::Player(player) => (Value::Table(player_table(lua, player)?), false),
    };
    ctx.raw_set("sender", sender)?;
    ctx.raw_set("console", console)?;
    ctx.raw_set("command", call.command.as_str())?;
    let path = lua.create_sequence_from(call.path.iter().map(String::as_str))?;
    path.set_readonly(true);
    ctx.raw_set("path", path)?;

    let args = lua.create_table()?;
    for (name, value) in &call.args {
        let value = match value {
            ArgValue::String(text) => text.as_str().into_lua(lua)?,
            ArgValue::Int(int) => int.into_lua(lua)?,
            ArgValue::Number(number) => number.into_lua(lua)?,
            ArgValue::Bool(boolean) => boolean.into_lua(lua)?,
            ArgValue::Player(player) => Value::Table(player_table(lua, player)?),
        };
        args.raw_set(name.as_str(), value)?;
    }
    args.set_readonly(true);
    ctx.raw_set("args", args)?;

    let actions = lua
        .app_data_ref::<mpsc::Sender<Action>>()
        .ok_or_else(|| mlua::Error::runtime("the plugin API is not installed"))?
        .clone();
    for (name, success) in [("reply", true), ("error", false)] {
        let (reply, finished, actions) = (Rc::clone(reply), Rc::clone(finished), actions.clone());
        let sender = call.sender.clone();
        let respond = lua.create_function(move |_, args: MultiValue| {
            let text = match method_args(args).next() {
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
            match &sender {
                CommandSender::Player(player) => {
                    let message = if success { text } else { format!("§c{text}") };
                    request(
                        &actions,
                        Action::SendMessage {
                            player: player.uuid.clone(),
                            message,
                        },
                    )
                }
                CommandSender::Console if success => {
                    tracing::info!(target: "console", "{text}");
                    Ok(())
                }
                CommandSender::Console => {
                    tracing::warn!(target: "console", "{text}");
                    Ok(())
                }
            }
        })?;
        ctx.raw_set(name, respond)?;
    }
    ctx.set_readonly(true);
    Ok(ctx)
}

/// Reads a command definition, putting each runnable node's handler in
/// `handlers` under its path (subcommand names joined by spaces).
fn parse_command(definition: &Table, handlers: &Table) -> Result<CommandSpec, String> {
    let name: String = field(definition, "name")?.ok_or("a command needs a name")?;
    check_name(&name)?;
    let description: String = field(definition, "description")?.unwrap_or_default();
    let aliases: Vec<String> = match field::<Table>(definition, "aliases")? {
        Some(list) => strings(&list).map_err(|err| format!("aliases: {err}"))?,
        None => Vec::new(),
    };
    for alias in &aliases {
        check_name(alias).map_err(|err| format!("alias: {err}"))?;
    }
    check_keys(
        definition,
        &[
            "name",
            "description",
            "aliases",
            "permission",
            "args",
            "run",
            "subcommands",
        ],
    )?;
    let mut root = parse_node(definition, &[], Permission::Any, handlers)?;
    root.description.clone_from(&description);
    Ok(CommandSpec {
        name,
        description,
        aliases,
        root,
    })
}

fn parse_node(
    table: &Table,
    path: &[String],
    inherited: Permission,
    handlers: &Table,
) -> Result<CommandNode, String> {
    let at = || {
        if path.is_empty() {
            String::new()
        } else {
            format!("subcommand {:?}: ", path.join(" "))
        }
    };
    let permission = match field::<String>(table, "permission")? {
        Some(name) => Permission::from_name(&name).ok_or_else(|| {
            format!(
                "{}unknown permission {name:?}; expected any or operator",
                at()
            )
        })?,
        None => Permission::Any,
    }
    // Subcommands can only narrow who may run them.
    .max(inherited);
    let description: String = field(table, "description")?.unwrap_or_default();
    let run: Option<Function> = field(table, "run")?;
    let args = match field::<Table>(table, "args")? {
        Some(list) => Some(parse_args(&list).map_err(|err| format!("{}{err}", at()))?),
        None => None,
    };
    // One handler per node, so plugin nodes have one overload.
    let overloads = match (run, args) {
        (Some(run), args) => {
            handlers
                .raw_set(path.join(" "), run)
                .map_err(|err| err.to_string())?;
            vec![args.unwrap_or_default()]
        }
        (None, Some(_)) => return Err(format!("{}args without a run function", at())),
        (None, None) => Vec::new(),
    };

    let mut subcommands = Vec::new();
    if let Some(table) = field::<Table>(table, "subcommands")? {
        for pair in table.pairs::<String, Table>() {
            let (name, node) = pair
                .map_err(|_| format!("{}subcommands must map names to subcommand tables", at()))?;
            check_name(&name).map_err(|err| format!("{}subcommand: {err}", at()))?;
            let mut below = path.to_vec();
            below.push(name.clone());
            check_keys(
                &node,
                &["description", "permission", "args", "run", "subcommands"],
            )
            .map_err(|err| format!("subcommand {:?}: {err}", below.join(" ")))?;
            subcommands.push((name, parse_node(&node, &below, permission, handlers)?));
        }
    }
    // Lua tables have no order; list subcommands alphabetically.
    subcommands.sort_by(|(a, _), (b, _)| a.cmp(b));
    if overloads.is_empty() && subcommands.is_empty() {
        return Err(format!("{}needs a run function or subcommands", at()));
    }
    Ok(CommandNode {
        description,
        permission,
        overloads,
        subcommands,
    })
}

fn parse_args(list: &Table) -> Result<Vec<ArgSpec>, String> {
    let mut args: Vec<ArgSpec> = Vec::new();
    for (index, arg) in list.sequence_values::<Table>().enumerate() {
        let arg = arg.map_err(|_| "args must be a list of argument tables".to_owned())?;
        let name: String =
            field(&arg, "name")?.ok_or_else(|| format!("argument {} needs a name", index + 1))?;
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(format!("argument name {name:?} must be one word"));
        }
        if args.iter().any(|other| other.name == name) {
            return Err(format!("two arguments are named {name:?}"));
        }
        check_keys(&arg, &["name", "type", "optional", "values", "enum"])
            .map_err(|err| format!("argument {name:?}: {err}"))?;
        let type_name: String = field(&arg, "type")?.unwrap_or_else(|| "string".into());
        let kind = match type_name.as_str() {
            "string" => ArgKind::String,
            "text" => ArgKind::Text,
            "int" => ArgKind::Int,
            "number" => ArgKind::Number,
            "bool" => ArgKind::Bool,
            "player" => ArgKind::Player,
            "gamemode" => ArgKind::game_mode(),
            "enum" => {
                let values = field::<Table>(&arg, "values")?
                    .ok_or_else(|| format!("enum argument {name:?} needs values"))?;
                let values = strings(&values).map_err(|err| format!("argument {name:?}: {err}"))?;
                if values.is_empty() || values.iter().any(|value| value.contains(' ')) {
                    return Err(format!(
                        "enum argument {name:?} needs values without spaces"
                    ));
                }
                let enum_name: String = field(&arg, "enum")?.unwrap_or_else(|| name.clone());
                ArgKind::Enum {
                    name: enum_name,
                    values,
                }
            }
            other => {
                return Err(format!(
                    "argument {name:?} has unknown type {other:?}; expected string, text, \
                     int, number, bool, player, gamemode or enum"
                ));
            }
        };
        let optional: bool = field(&arg, "optional")?.unwrap_or(false);
        if let Some(last) = args.last() {
            if last.kind == ArgKind::Text {
                return Err(format!("text argument {:?} must be the last", last.name));
            }
            if last.optional && !optional {
                return Err(format!(
                    "required argument {name:?} cannot follow optional {:?}",
                    last.name
                ));
            }
        }
        args.push(ArgSpec {
            name,
            kind,
            optional,
        });
    }
    Ok(args)
}

/// A field of a definition table, of the expected type.
fn field<T: mlua::FromLua>(table: &Table, key: &str) -> Result<Option<T>, String> {
    match table.raw_get::<Value>(key) {
        Ok(Value::Nil) => Ok(None),
        Ok(value) => table
            .raw_get::<T>(key)
            .map(Some)
            .map_err(|_| format!("{key} has the wrong type ({})", value.type_name())),
        Err(err) => Err(err.to_string()),
    }
}

fn strings(list: &Table) -> Result<Vec<String>, String> {
    list.sequence_values::<String>()
        .collect::<mlua::Result<_>>()
        .map_err(|_| "expected a list of strings".to_owned())
}

/// Rejects keys a definition does not use, such as a misspelled `subcomands`.
fn check_keys(table: &Table, known: &[&str]) -> Result<(), String> {
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|err| err.to_string())?;
        let known_key = match &key {
            Value::String(key) => key.to_str().is_ok_and(|key| known.contains(&&*key)),
            _ => false,
        };
        if !known_key {
            return Err(format!(
                "unknown field {}; expected one of: {}",
                key.to_string().unwrap_or_else(|_| "?".into()),
                known.join(", ")
            ));
        }
    }
    Ok(())
}

/// Command and subcommand names: lower case letters, digits, `_` and `-`.
/// The client crashes on upper case command names.
fn check_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{name:?} must be lower case letters, digits, _ or -"
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::Player;
    use crate::luau::{Limits, LuauEngine};

    fn engine() -> (LuauEngine, mpsc::Receiver<Action>) {
        let limits = Limits {
            memory: 16 * 1024 * 1024,
            execution: Duration::from_millis(250),
        };
        let (actions, received) = mpsc::channel(16);
        (
            LuauEngine::new(limits, Arc::new(|_, _, _| {}), actions),
            received,
        )
    }

    const WARP: &str = r#"
        server.command({
            name = "warp",
            description = "Travel between warps",
            aliases = { "w" },
            args = { { name = "name", type = "string" } },
            run = function(ctx) ctx.reply(`to {ctx.args.name}`) end,
            subcommands = {
                set = {
                    description = "Make a warp",
                    args = {
                        { name = "name", type = "string" },
                        { name = "public", type = "bool", optional = true },
                    },
                    run = function(ctx)
                        ctx.reply(`set {ctx.args.name} {tostring(ctx.args.public)}`)
                    end,
                },
                admin = {
                    permission = "operator",
                    subcommands = {
                        reload = { run = function(ctx) return "reloaded" end },
                        broken = { run = function(ctx) error("oops") end },
                    },
                },
            },
        })
    "#;

    fn steve() -> Player {
        Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        }
    }

    fn call(path: &[&str], args: Vec<(&str, ArgValue)>) -> CommandCall {
        CommandCall {
            plugin: "warps".into(),
            command: "warp".into(),
            path: path.iter().map(|name| (*name).to_owned()).collect(),
            args: args
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
            sender: CommandSender::Player(steve()),
        }
    }

    fn lines(reply: &CommandReply) -> Vec<(bool, &str)> {
        reply
            .lines
            .iter()
            .map(|line| (line.success, line.text.as_str()))
            .collect()
    }

    #[test]
    fn definitions_become_command_trees() {
        let (mut engine, _) = engine();
        engine.load_script("warps", "warps.luau", WARP).unwrap();
        let commands = engine.commands();
        assert_eq!(commands.len(), 1);
        let spec = &commands[0].spec;
        assert_eq!(commands[0].plugin, "warps");
        assert_eq!(spec.name, "warp");
        assert_eq!(spec.aliases, ["w"]);
        assert_eq!(spec.root.overloads[0][0].kind, ArgKind::String);
        let names: Vec<_> = spec
            .root
            .subcommands
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["admin", "set"], "listed alphabetically");

        let admin = spec.root.find(&["admin".into()]).unwrap();
        assert_eq!(admin.permission, Permission::Operator);
        assert!(!admin.runs_itself(), "a group only leads to subcommands");
        let reload = spec.root.find(&["admin".into(), "reload".into()]).unwrap();
        assert_eq!(reload.permission, Permission::Operator, "inherited");
        assert_eq!(reload.overloads, [Vec::new()]);
        let set = spec.root.find(&["set".into()]).unwrap();
        assert_eq!(set.description, "Make a warp");
        assert!(set.overloads[0][1].optional);
    }

    #[test]
    fn handlers_get_their_arguments_and_reply() {
        let (mut engine, _) = engine();
        engine.load_script("warps", "warps.luau", WARP).unwrap();

        let reply =
            engine.run_command(&call(&[], vec![("name", ArgValue::String("spawn".into()))]));
        assert_eq!(lines(&reply), [(true, "to spawn")]);

        let reply = engine.run_command(&call(
            &["set"],
            vec![
                ("name", ArgValue::String("home".into())),
                ("public", ArgValue::Bool(true)),
            ],
        ));
        assert_eq!(lines(&reply), [(true, "set home true")]);

        // Optional arguments left out are nil.
        let reply = engine.run_command(&call(
            &["set"],
            vec![("name", ArgValue::String("home".into()))],
        ));
        assert_eq!(lines(&reply), [(true, "set home nil")]);

        // Returned strings are replies; errors become a red line.
        let reply = engine.run_command(&call(&["admin", "reload"], Vec::new()));
        assert_eq!(lines(&reply), [(true, "reloaded")]);
        let reply = engine.run_command(&call(&["admin", "broken"], Vec::new()));
        assert_eq!(lines(&reply), [(false, HANDLER_FAILED)]);
    }

    #[test]
    fn the_context_describes_the_call() {
        let (mut engine, _) = engine();
        engine
            .load_script(
                "who",
                "who.luau",
                r#"
                    server.command({
                        name = "who",
                        args = {
                            { name = "target", type = "player" },
                            { name = "count", type = "int" },
                        },
                        run = function(ctx)
                            ctx.reply(`{ctx.sender.name} -> {ctx.args.target.name} x{ctx.args.count}`)
                            ctx:error(`console: {tostring(ctx.console)}, path: {#ctx.path}`)
                        end,
                    })
                "#,
            )
            .unwrap();
        let reply = engine.run_command(&CommandCall {
            plugin: "who".into(),
            command: "who".into(),
            path: Vec::new(),
            args: vec![
                ("target".into(), ArgValue::Player(steve())),
                ("count".into(), ArgValue::Int(3)),
            ],
            sender: CommandSender::Player(steve()),
        });
        assert_eq!(
            lines(&reply),
            [
                (true, "Steve -> Steve x3"),
                (false, "console: false, path: 0")
            ]
        );
    }

    #[test]
    fn late_replies_reach_the_player_as_chat() {
        let (mut engine, mut actions) = engine();
        engine
            .load_script(
                "later",
                "later.luau",
                r#"
                    local saved
                    server.command({ name = "later", run = function(ctx) saved = ctx end })
                    server.on("player_join", function() saved.error("too late") end)
                "#,
            )
            .unwrap();
        let mut later = call(&[], Vec::new());
        later.plugin = "later".into();
        later.command = "later".into();
        assert!(engine.run_command(&later).lines.is_empty());
        engine.dispatch(&crate::Event::PlayerJoin(steve()));
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::SendMessage {
                player: steve().uuid,
                message: "§ctoo late".into()
            }
        );
    }

    #[test]
    fn invalid_definitions_fail_to_load() {
        for (definition, expected) in [
            (r#"{ name = "Warp", run = print }"#, "lower case"),
            (
                r#"{ name = "warp" }"#,
                "needs a run function or subcommands",
            ),
            (
                r#"{ name = "warp", args = { { name = "x" } } }"#,
                "args without a run function",
            ),
            (
                r#"{ name = "warp", run = print, args = { { name = "a", optional = true }, { name = "b" } } }"#,
                "cannot follow optional",
            ),
            (
                r#"{ name = "warp", run = print, args = { { name = "a", type = "text" }, { name = "b" } } }"#,
                "must be the last",
            ),
            (
                r#"{ name = "warp", run = print, args = { { name = "a", type = "vector" } } }"#,
                "unknown type",
            ),
            (
                r#"{ name = "warp", run = print, args = { { name = "a", type = "enum" } } }"#,
                "needs values",
            ),
            (
                r#"{ name = "warp", subcomands = {}, run = print }"#,
                "unknown field subcomands",
            ),
            (
                r#"{ name = "warp", subcommands = { set = { permission = "admin", run = print } } }"#,
                "subcommand \"set\": unknown permission",
            ),
        ] {
            let (mut engine, _) = engine();
            let err = engine
                .load_script("bad", "bad.luau", &format!("server.command({definition})"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{definition}: {err}");
        }

        let (mut engine, _) = engine();
        let err = engine
            .load_script(
                "twice",
                "twice.luau",
                r#"for _ = 1, 2 do server.command({ name = "x", run = print }) end"#,
            )
            .unwrap_err();
        assert!(err.to_string().contains("already registered /x"), "{err}");
    }

    #[test]
    fn unloading_a_plugin_removes_its_commands() {
        let (mut engine, _) = engine();
        engine.load_script("warps", "warps.luau", WARP).unwrap();
        assert_eq!(engine.commands().len(), 1);
        engine.unload("warps");
        assert!(engine.commands().is_empty());
        let reply = engine.run_command(&call(&[], vec![("name", ArgValue::String("x".into()))]));
        assert_eq!(lines(&reply), [(false, "/warp is no longer available.")]);
    }
}
