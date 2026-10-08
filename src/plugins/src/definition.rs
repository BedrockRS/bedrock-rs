//! Command definitions as plugins write them, read into [`CommandSpec`]s.
//!
//! Both engines hand `Server.registerCommand(definition)` over as JSON, with
//! each handler (`run`) replaced by `true`: the handlers stay in the plugin,
//! keyed by the command's name and the subcommand path. So a definition means
//! the same, and fails with the same message, in Luau and in JavaScript:
//!
//! ```js
//! Server.registerCommand({
//!   name: "warp",
//!   description: "Travel between warps",
//!   aliases: ["w"],
//!   permission: "any",            // or "operator"; the default is "any"
//!   args: [{ name: "name", type: "string" }],
//!   run: (ctx) => { ... },        // /warp <name>
//!   subcommands: {
//!     set: { description: "Make a warp", args: [...], run: (ctx) => { ... } },
//!     admin: { permission: "operator", subcommands: { reload: { run: ... } } },
//!   },
//! });
//! ```
//!
//! Argument types: `string` (a word or a quoted string), `text` (the rest of
//! the line), `int`, `number`, `bool`, `player`, `gamemode`, and `enum` with
//! `values: [...]` (and optionally `enum: "TypeName"`). A `gamemode` takes
//! vanilla's names, `default` included. Arguments may be `optional: true`,
//! after the required ones. Subcommands are listed alphabetically, as Luau
//! tables have no order.

use serde_json::{Map, Value};

use crate::{ArgKind, ArgSpec, CommandNode, CommandSpec, Permission};

/// Reads a command definition. Every node with a handler has `run: true`.
pub fn parse_command(definition: &Value) -> Result<CommandSpec, String> {
    let definition = object(definition, "a command definition")?;
    let name = string(definition, "name")?.ok_or("a command needs a name")?;
    check_name(&name)?;
    let description = string(definition, "description")?.unwrap_or_default();
    let aliases = match definition.get("aliases") {
        Some(Value::Null) | None => Vec::new(),
        Some(list) => strings(list).map_err(|err| format!("aliases: {err}"))?,
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
    let mut root = parse_node(definition, &[], Permission::Any)?;
    root.description.clone_from(&description);
    Ok(CommandSpec {
        name,
        description,
        aliases,
        root,
    })
}

fn parse_node(
    node: &Map<String, Value>,
    path: &[String],
    inherited: Permission,
) -> Result<CommandNode, String> {
    let at = || {
        if path.is_empty() {
            String::new()
        } else {
            format!("subcommand {:?}: ", path.join(" "))
        }
    };
    let permission = match string(node, "permission")? {
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
    let description = string(node, "description")?.unwrap_or_default();
    let runs = match node.get("run") {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return Err(format!("{}run must be a function", at())),
    };
    let args = match node.get("args") {
        None | Some(Value::Null) => None,
        Some(list) => Some(parse_args(list).map_err(|err| format!("{}{err}", at()))?),
    };
    // One handler per node, so plugin nodes have one overload.
    let overloads = match (runs, args) {
        (true, args) => vec![args.unwrap_or_default()],
        (false, Some(_)) => return Err(format!("{}args without a run function", at())),
        (false, None) => Vec::new(),
    };

    let mut subcommands = Vec::new();
    match node.get("subcommands") {
        None | Some(Value::Null) => {}
        // An empty Luau table reads as an empty list.
        Some(Value::Array(list)) if list.is_empty() => {}
        Some(Value::Object(table)) => {
            for (name, below_node) in table {
                check_name(name).map_err(|err| format!("{}subcommand: {err}", at()))?;
                let mut below = path.to_vec();
                below.push(name.clone());
                let below_node = object(below_node, "a subcommand").map_err(|_| {
                    format!("{}subcommands must map names to subcommand tables", at())
                })?;
                check_keys(
                    below_node,
                    &["description", "permission", "args", "run", "subcommands"],
                )
                .map_err(|err| format!("subcommand {:?}: {err}", below.join(" ")))?;
                subcommands.push((name.clone(), parse_node(below_node, &below, permission)?));
            }
        }
        Some(_) => {
            return Err(format!(
                "{}subcommands must map names to subcommand tables",
                at()
            ));
        }
    }
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

fn parse_args(list: &Value) -> Result<Vec<ArgSpec>, String> {
    let Value::Array(list) = list else {
        return Err("args must be a list of argument tables".into());
    };
    let mut args: Vec<ArgSpec> = Vec::new();
    for (index, arg) in list.iter().enumerate() {
        let arg =
            object(arg, "an argument").map_err(|_| "args must be a list of argument tables")?;
        let name =
            string(arg, "name")?.ok_or_else(|| format!("argument {} needs a name", index + 1))?;
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(format!("argument name {name:?} must be one word"));
        }
        if args.iter().any(|other| other.name == name) {
            return Err(format!("two arguments are named {name:?}"));
        }
        check_keys(arg, &["name", "type", "optional", "values", "enum"])
            .map_err(|err| format!("argument {name:?}: {err}"))?;
        let type_name = string(arg, "type")?.unwrap_or_else(|| "string".into());
        let kind = match type_name.as_str() {
            "string" => ArgKind::String,
            "text" => ArgKind::Text,
            "int" => ArgKind::Int,
            "number" => ArgKind::Number,
            "bool" => ArgKind::Bool,
            "player" => ArgKind::Player,
            "gamemode" => ArgKind::game_mode(),
            "enum" => {
                let values = arg
                    .get("values")
                    .filter(|values| !values.is_null())
                    .ok_or_else(|| format!("enum argument {name:?} needs values"))?;
                let values = strings(values).map_err(|err| format!("argument {name:?}: {err}"))?;
                if values.is_empty() || values.iter().any(|value| value.contains(' ')) {
                    return Err(format!(
                        "enum argument {name:?} needs values without spaces"
                    ));
                }
                let enum_name = string(arg, "enum")?.unwrap_or_else(|| name.clone());
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
        let optional = match arg.get("optional") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(optional)) => *optional,
            Some(other) => {
                return Err(format!(
                    "optional has the wrong type ({})",
                    type_name_of(other)
                ));
            }
        };
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

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(format!(
            "{what} must be a table, not {}",
            type_name_of(other)
        )),
    }
}

/// A string field, if present.
fn string(map: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(format!(
            "{key} has the wrong type ({})",
            type_name_of(other)
        )),
    }
}

fn strings(list: &Value) -> Result<Vec<String>, String> {
    match list {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => Ok(text.clone()),
                _ => Err("expected a list of strings".to_owned()),
            })
            .collect(),
        _ => Err("expected a list of strings".into()),
    }
}

/// Rejects keys a definition does not use, such as a misspelled `subcomands`.
fn check_keys(map: &Map<String, Value>, known: &[&str]) -> Result<(), String> {
    match map.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(format!(
            "unknown field {key}; expected one of: {}",
            known.join(", ")
        )),
        None => Ok(()),
    }
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

/// How a value's type is named in errors, as scripts call it.
fn type_name_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "nil",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) | Value::Object(_) => "table",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn warp() -> Value {
        json!({
            "name": "warp",
            "description": "Travel between warps",
            "aliases": ["w"],
            "args": [{ "name": "name", "type": "string" }],
            "run": true,
            "subcommands": {
                "set": {
                    "description": "Make a warp",
                    "args": [
                        { "name": "name", "type": "string" },
                        { "name": "public", "type": "bool", "optional": true }
                    ],
                    "run": true
                },
                "admin": {
                    "permission": "operator",
                    "subcommands": { "reload": { "run": true } }
                }
            }
        })
    }

    #[test]
    fn definitions_become_command_trees() {
        let spec = parse_command(&warp()).unwrap();
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
    fn invalid_definitions_say_why() {
        for (definition, expected) in [
            (json!({ "name": "Warp", "run": true }), "lower case"),
            (
                json!({ "name": "warp" }),
                "needs a run function or subcommands",
            ),
            (
                json!({ "name": "warp", "args": [{ "name": "x" }] }),
                "args without a run function",
            ),
            (
                json!({ "name": "warp", "run": true, "args": [{ "name": "a", "optional": true }, { "name": "b" }] }),
                "cannot follow optional",
            ),
            (
                json!({ "name": "warp", "run": true, "args": [{ "name": "a", "type": "text" }, { "name": "b" }] }),
                "must be the last",
            ),
            (
                json!({ "name": "warp", "run": true, "args": [{ "name": "a", "type": "vector" }] }),
                "unknown type",
            ),
            (
                json!({ "name": "warp", "run": true, "args": [{ "name": "a", "type": "enum" }] }),
                "needs values",
            ),
            (
                json!({ "name": "warp", "subcomands": {}, "run": true }),
                "unknown field subcomands",
            ),
            (
                json!({ "name": "warp", "subcommands": { "set": { "permission": "admin", "run": true } } }),
                "subcommand \"set\": unknown permission",
            ),
            (
                json!({ "name": "warp", "run": "yes" }),
                "run must be a function",
            ),
            (json!("warp"), "must be a table"),
        ] {
            let err = parse_command(&definition).unwrap_err();
            assert!(err.contains(expected), "{definition}: {err}");
        }
    }

    #[test]
    fn empty_subcommand_tables_are_allowed() {
        // An empty Luau table cannot say whether it is a list or a map.
        let spec = parse_command(&json!({ "name": "x", "run": true, "subcommands": [] })).unwrap();
        assert!(spec.root.subcommands.is_empty());
    }
}
