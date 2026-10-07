//! What a player's client is told about commands, so it can autocomplete them.
//!
//! Each way of running a command becomes an overload: its subcommands, as
//! enums of one value each (which the client shows as plain words), then the
//! node's arguments. Players only hear of what they may run.

use std::collections::HashMap;

use bedrockrs_plugins::ArgKind;
use bedrockrs_protocol::packets::{
    AvailableCommands, CommandData, CommandEnum, CommandParameter, NO_ALIASES, arg_type,
};

use super::{Commands, Sender, parse};

impl Commands {
    /// The commands `sender` may run, for their client.
    pub fn available_to(&self, sender: &Sender) -> AvailableCommands {
        let mut builder = Builder::default();
        for command in self.list() {
            let spec = &command.spec;
            let mut overloads: Vec<Vec<CommandParameter>> = Vec::new();
            for (path, node) in parse::runnable(&spec.root, &[], sender) {
                for args in &node.overloads {
                    let mut parameters: Vec<CommandParameter> = path
                        .iter()
                        .map(|word| CommandParameter {
                            name: word.clone(),
                            param_type: builder.enum_type(
                                &format!("SubCommand{word}"),
                                std::slice::from_ref(word),
                            ),
                            optional: false,
                        })
                        .collect();
                    for arg in args {
                        parameters.push(CommandParameter {
                            name: arg.name.clone(),
                            param_type: builder.arg_type(&arg.kind),
                            optional: arg.optional,
                        });
                    }
                    overloads.push(parameters);
                }
            }
            if overloads.is_empty() {
                continue;
            }
            // The client lists a command by the names in its alias enum
            // instead of its own name, so the name goes in too, as
            // PocketMine does; without it only the aliases show.
            let aliases_enum = if spec.aliases.is_empty() {
                NO_ALIASES
            } else {
                let mut names = spec.aliases.clone();
                names.push(spec.name.clone());
                builder.enum_index(&format!("{}Aliases", capitalized(&spec.name)), &names)
            };
            builder.packet.commands.push(CommandData {
                name: spec.name.clone(),
                description: spec.description.clone(),
                aliases_enum,
                overloads,
            });
        }
        builder.packet
    }
}

/// `hello` → `Hello`, for enum names such as `HelloAliases`.
fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[derive(Default)]
struct Builder {
    packet: AvailableCommands,
    /// Each value's index in `enum_values`.
    values: HashMap<String, u32>,
    /// Each enum's index by name, with its values.
    enums: HashMap<String, (u32, Vec<String>)>,
}

impl Builder {
    fn arg_type(&mut self, kind: &ArgKind) -> u32 {
        let basic = match kind {
            ArgKind::String => arg_type::STRING,
            ArgKind::Text => arg_type::RAW_TEXT,
            ArgKind::Int => arg_type::INT,
            ArgKind::Number => arg_type::FLOAT,
            ArgKind::Player => arg_type::TARGET,
            ArgKind::Bool => {
                return self.enum_type("Boolean", &["true".to_owned(), "false".to_owned()]);
            }
            ArgKind::Enum { name, values } => return self.enum_type(name, values),
        };
        arg_type::VALID | basic
    }

    fn enum_type(&mut self, name: &str, values: &[String]) -> u32 {
        arg_type::VALID | arg_type::ENUM | self.enum_index(name, values)
    }

    /// The index of the enum called `name` with these values, added if new.
    /// Different values under a name already used get a numbered name.
    fn enum_index(&mut self, name: &str, values: &[String]) -> u32 {
        let mut unique = name.to_owned();
        let mut suffix = 2;
        loop {
            match self.enums.get(&unique) {
                Some((index, existing)) if existing == values => return *index,
                Some(_) => {
                    unique = format!("{name}{suffix}");
                    suffix += 1;
                }
                None => break,
            }
        }
        let indices = values.iter().map(|value| self.value_index(value)).collect();
        let index = u32::try_from(self.packet.enums.len()).expect("few enums");
        self.packet.enums.push(CommandEnum {
            name: unique.clone(),
            values: indices,
            dynamic_values: Vec::new(),
        });
        self.enums.insert(unique, (index, values.to_vec()));
        index
    }

    fn value_index(&mut self, value: &str) -> u32 {
        if let Some(index) = self.values.get(value) {
            return *index;
        }
        let index = u32::try_from(self.packet.enum_values.len()).expect("few enum values");
        self.packet.enum_values.push(value.to_owned());
        self.values.insert(value.to_owned(), index);
        index
    }
}

#[cfg(test)]
mod tests {
    use bedrockrs_plugins::{ArgSpec, CommandNode, CommandSpec, Permission, PluginCommand};
    use uuid::Uuid;

    use super::*;
    use crate::commands::PlayerSender;

    fn player(operator: bool) -> Sender {
        Sender::Player(PlayerSender {
            uuid: Uuid::nil(),
            name: "Steve".into(),
            operator,
        })
    }

    fn commands() -> Commands {
        let commands = Commands::new();
        commands.set_plugin_commands(vec![PluginCommand {
            plugin: "warps".into(),
            spec: CommandSpec {
                name: "warp".into(),
                description: "Warps".into(),
                aliases: vec!["w".into()],
                root: CommandNode {
                    description: "Warps".into(),
                    permission: Permission::Any,
                    overloads: vec![vec![ArgSpec::required("name", ArgKind::String)]],
                    subcommands: vec![
                        (
                            "admin".into(),
                            CommandNode::group(vec![("reload".into(), CommandNode::runs())])
                                .permission(Permission::Operator),
                        ),
                        (
                            "set".into(),
                            CommandNode::runs_with(vec![
                                ArgSpec::required("name", ArgKind::String),
                                ArgSpec::optional("public", ArgKind::Bool),
                            ]),
                        ),
                    ],
                },
            },
        }]);
        commands
    }

    fn command<'a>(packet: &'a AvailableCommands, name: &str) -> Option<&'a CommandData> {
        packet.commands.iter().find(|command| command.name == name)
    }

    fn enum_name(packet: &AvailableCommands, param: &CommandParameter) -> String {
        assert_ne!(param.param_type & arg_type::ENUM, 0, "{param:?}");
        let index = param.param_type & 0xFFFF;
        packet.enums[index as usize].name.clone()
    }

    #[test]
    fn subcommands_become_overloads() {
        let packet = commands().available_to(&player(false));
        let warp = command(&packet, "warp").unwrap();
        // `/warp <name>` and `/warp set <name> [public]`; admin needs an operator.
        assert_eq!(warp.overloads.len(), 2);
        assert_eq!(
            warp.overloads[0],
            [CommandParameter {
                name: "name".into(),
                param_type: arg_type::VALID | arg_type::STRING,
                optional: false,
            }]
        );
        let set = &warp.overloads[1];
        assert_eq!(set[0].name, "set");
        assert_eq!(enum_name(&packet, &set[0]), "SubCommandset");
        assert_eq!(enum_name(&packet, &set[2]), "Boolean");
        assert!(set[2].optional);

        let aliases = &packet.enums[warp.aliases_enum as usize];
        assert_eq!(aliases.name, "WarpAliases");
        let names: Vec<&str> = aliases
            .values
            .iter()
            .map(|index| packet.enum_values[*index as usize].as_str())
            .collect();
        assert_eq!(
            names,
            ["w", "warp"],
            "the name itself, or only the alias shows"
        );

        let operator = commands().available_to(&player(true));
        let warp = command(&operator, "warp").unwrap();
        let admin = &warp.overloads[1];
        assert_eq!(
            admin
                .iter()
                .map(|param| param.name.as_str())
                .collect::<Vec<_>>(),
            ["admin", "reload"]
        );
    }

    #[test]
    fn players_only_hear_of_commands_they_may_run() {
        let packet = commands().available_to(&player(false));
        assert!(command(&packet, "help").is_some());
        assert!(command(&packet, "gamemode").is_none());
        assert!(command(&packet, "help").unwrap().aliases_enum == NO_ALIASES);

        let operator = commands().available_to(&player(true));
        let gamemode = command(&operator, "gamemode").unwrap();
        assert_eq!(enum_name(&operator, &gamemode.overloads[0][0]), "GameMode");
        // Vanilla's names only; numbers are an overload of their own.
        let names = &operator.enums[(gamemode.overloads[0][0].param_type & 0xFFFF) as usize];
        let names: Vec<&str> = names
            .values
            .iter()
            .map(|index| operator.enum_values[*index as usize].as_str())
            .collect();
        assert_eq!(
            names,
            [
                "survival",
                "creative",
                "adventure",
                "spectator",
                "s",
                "c",
                "a",
                "default",
                "d"
            ]
        );
        assert_eq!(
            gamemode.overloads[1][0],
            CommandParameter {
                name: "gameMode".into(),
                param_type: arg_type::VALID | arg_type::INT,
                optional: false,
            }
        );
        assert_eq!(
            gamemode.overloads[0][1].param_type,
            arg_type::VALID | arg_type::TARGET
        );
    }

    #[test]
    fn enum_values_are_shared_and_names_kept_apart() {
        let mut builder = Builder::default();
        let a = builder.enum_index("Colour", &["red".into(), "blue".into()]);
        let b = builder.enum_index("Colour", &["red".into(), "blue".into()]);
        let c = builder.enum_index("Colour", &["red".into()]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(builder.packet.enums[c as usize].name, "Colour2");
        assert_eq!(builder.packet.enum_values, ["red", "blue"]);
    }
}
