//! The server's own commands.

use std::sync::Arc;

use bedrockrs_plugins::{
    ArgKind, ArgSpec, ArgValue, CommandNode, CommandReply, CommandSpec, Permission,
};
use bedrockrs_protocol::{GAME_VERSION, PROTOCOL_VERSION};
use uuid::Uuid;

use super::parse::{self, Matched};
use super::{Owner, Registered, Sender};
use crate::damage::DamageCause;
use crate::game_mode::GameMode;
use crate::game_rules::Rule;
use crate::logins::Control;
use crate::server::Server;

/// A built-in command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Help,
    List,
    Version,
    GameMode,
    GameRule,
    Kill,
    Op,
    Deop,
    Stop,
}

impl Builtin {
    pub const ALL: [Self; 9] = [
        Self::Help,
        Self::List,
        Self::Version,
        Self::GameMode,
        Self::GameRule,
        Self::Kill,
        Self::Op,
        Self::Deop,
        Self::Stop,
    ];

    fn spec(self) -> CommandSpec {
        let player = || ArgSpec::required("player", ArgKind::Player);
        let (name, description, root) = match self {
            Self::Help => (
                "help",
                "Lists the commands you can use, or explains one",
                CommandNode::runs_with(vec![ArgSpec::optional("command", ArgKind::String)]),
            ),
            Self::List => ("list", "Lists the players online", CommandNode::runs()),
            Self::Version => ("version", "Shows the server's version", CommandNode::runs()),
            // As in vanilla: game modes by name, or by number (0, 1 or 2)
            // in an overload of their own, so the numbers are not listed
            // among the names.
            Self::GameMode => (
                "gamemode",
                "Sets a player's game mode",
                CommandNode::runs_with(vec![
                    ArgSpec::required("gameMode", ArgKind::game_mode()),
                    ArgSpec::optional("player", ArgKind::Player),
                ])
                .or_with(vec![
                    ArgSpec::required("gameMode", ArgKind::Int),
                    ArgSpec::optional("player", ArgKind::Player),
                ])
                .permission(Permission::Operator),
            ),
            // As in vanilla: on its own it lists the rules; with a rule, it
            // shows it, or sets it to a value.
            Self::GameRule => (
                "gamerule",
                "Sets or queries a game rule value",
                CommandNode::runs()
                    .or_with(vec![
                        ArgSpec::required(
                            "rule",
                            ArgKind::Enum {
                                name: "BoolGameRule".into(),
                                values: Rule::ALL.map(|rule| rule.name().to_owned()).to_vec(),
                            },
                        ),
                        ArgSpec::optional("value", ArgKind::Bool),
                    ])
                    .permission(Permission::Operator),
            ),
            Self::Kill => (
                "kill",
                "Kills a player",
                CommandNode::runs_with(vec![ArgSpec::optional("target", ArgKind::Player)])
                    .permission(Permission::Operator),
            ),
            Self::Op => (
                "op",
                "Makes a player an operator",
                CommandNode::runs_with(vec![player()]).permission(Permission::Operator),
            ),
            Self::Deop => (
                "deop",
                "Takes away a player's operator status",
                CommandNode::runs_with(vec![player()]).permission(Permission::Operator),
            ),
            Self::Stop => (
                "stop",
                "Saves everything and stops the server",
                CommandNode::runs().permission(Permission::Operator),
            ),
        };
        CommandSpec {
            name: name.into(),
            description: description.into(),
            aliases: Vec::new(),
            root: root.described(description),
        }
    }

    /// Carries the command out.
    pub fn run(self, server: &Server, sender: &Sender, matched: &Matched) -> CommandReply {
        match self {
            Self::Help => help(server, sender, matched),
            Self::List => {
                let mut names = server.players.names();
                names.sort_by_key(|name| name.to_lowercase());
                CommandReply::ok(match names.len() {
                    0 => "Nobody is online".to_owned(),
                    1 => format!("1 player is online: {}", names[0]),
                    count => format!("{count} players are online: {}", names.join(", ")),
                })
            }
            Self::Version => CommandReply::ok(format!(
                "This server runs BedrockRS {} for Minecraft: Bedrock Edition {GAME_VERSION} \
                 (protocol {PROTOCOL_VERSION}).",
                env!("CARGO_PKG_VERSION")
            )),
            Self::GameMode => game_mode(server, sender, matched),
            Self::GameRule => game_rule(server, sender, matched),
            Self::Kill => {
                let target = match (matched.arg("target"), sender) {
                    (Some(ArgValue::Player(player)), _) => Uuid::parse_str(&player.uuid)
                        .ok()
                        .map(|uuid| (uuid, player.name.clone())),
                    (None, Sender::Player(player)) => Some((player.uuid, player.name.clone())),
                    (None, Sender::Console) => {
                        return CommandReply::error("Name a player: kill <target>");
                    }
                    _ => None,
                };
                let Some((uuid, name)) = target else {
                    return missing_player();
                };
                let kill = Control::Damage {
                    cause: DamageCause::SelfDestruct,
                    amount: f32::MAX,
                };
                if !server.logins.send(uuid, kill) {
                    return CommandReply::error(format!("{name} is not online"));
                }
                CommandReply::ok(format!("Killed {name}"))
            }
            Self::Op => {
                let Some((uuid, name)) = target(matched) else {
                    return missing_player();
                };
                // Permissions go by what the player's login named them by.
                let Some(ids) = server.logins.ids(uuid) else {
                    return missing_player();
                };
                match server.permissions.op(&ids, &name) {
                    Ok(true) => {}
                    Ok(false) => {
                        return CommandReply::error(format!("{name} is already an operator"));
                    }
                    Err(err) => return CommandReply::error(format!("Couldn't op {name}: {err}")),
                }
                server.logins.send(
                    uuid,
                    Control::SetPermission(crate::permissions::Permission::Operator),
                );
                tracing::info!("{} made {name} an operator", sender.name());
                CommandReply::ok(format!("Made {name} an operator"))
            }
            Self::Deop => {
                let Some((uuid, name)) = target(matched) else {
                    return missing_player();
                };
                let Some(ids) = server.logins.ids(uuid) else {
                    return missing_player();
                };
                if !server.permissions.deop(&ids) {
                    return CommandReply::error(format!("{name} is not an operator"));
                }
                let now = server.permissions.of(&ids);
                server.logins.send(uuid, Control::SetPermission(now));
                tracing::info!("{} took away {name}'s operator status", sender.name());
                CommandReply::ok(format!("{name} is no longer an operator"))
            }
            Self::Stop => {
                server.request_stop();
                CommandReply::ok("Stopping the server")
            }
        }
    }
}

/// Every built-in command.
pub fn all() -> Vec<Arc<Registered>> {
    Builtin::ALL
        .into_iter()
        .map(|builtin| {
            Arc::new(Registered {
                spec: builtin.spec(),
                owner: Owner::Builtin(builtin),
            })
        })
        .collect()
}

/// `/help`: the commands the sender may use; `/help <command>`: how to use one.
fn help(server: &Server, sender: &Sender, matched: &Matched) -> CommandReply {
    if let Some(ArgValue::String(name)) = matched.arg("command") {
        let name = name.trim_start_matches('/').to_ascii_lowercase();
        let Some(command) = server
            .commands
            .find(&name)
            .filter(|command| command.visible_to(sender))
        else {
            return CommandReply::error(format!("Unknown command: /{name}"));
        };
        let spec = &command.spec;
        let mut reply = CommandReply::ok(format!("/{}: {}", spec.name, spec.description));
        if !spec.aliases.is_empty() {
            reply.push_ok(format!("Aliases: /{}", spec.aliases.join(", /")));
        }
        reply.push_ok("Usage:");
        for line in parse::usages(spec, &spec.root, &[], sender) {
            reply.push_ok(line);
        }
        return reply;
    }
    let mut reply = CommandReply::ok("Commands you can use:");
    for command in server.commands.list() {
        if command.visible_to(sender) {
            reply.push_ok(format!(
                "/{} - {}",
                command.spec.name, command.spec.description
            ));
        }
    }
    reply.push_ok("Type /help <command> to see how to use one");
    reply
}

/// `/gamemode <gameMode> [player]`: the player is whoever runs it unless
/// named.
fn game_mode(server: &Server, sender: &Sender, matched: &Matched) -> CommandReply {
    let mode = match matched.arg("gameMode") {
        Some(ArgValue::String(name)) => GameMode::resolve(name, server.default_game_mode),
        Some(ArgValue::Int(number)) => GameMode::from_number(*number),
        _ => None,
    };
    let Some(mode) = mode else {
        return CommandReply::error(
            "Unknown game mode. Use survival, creative, adventure, spectator, default, or 0, 1 or 2",
        );
    };
    let (uuid, name) = match (target(matched), sender) {
        (Some(target), _) => target,
        (None, Sender::Player(player)) => (player.uuid, player.name.clone()),
        (None, Sender::Console) => {
            return CommandReply::error("Name a player: gamemode <gameMode> <player>");
        }
    };
    if !server.logins.send(uuid, Control::SetGameMode(mode)) {
        return CommandReply::error(format!("{name} is not online"));
    }
    let own = matches!(sender, Sender::Player(player) if player.uuid == uuid);
    if own {
        CommandReply::ok(format!("Set your game mode to {}", mode.name()))
    } else {
        CommandReply::ok(format!("Set {name}'s game mode to {}", mode.name()))
    }
}

/// `/gamerule`: every rule; `/gamerule <rule>`: its value; `/gamerule
/// <rule> <value>`: sets it, for every player at once.
fn game_rule(server: &Server, sender: &Sender, matched: &Matched) -> CommandReply {
    let values = server.game_rules.values();
    let Some(ArgValue::String(name)) = matched.arg("rule") else {
        let listed: Vec<String> = Rule::ALL
            .into_iter()
            .map(|rule| format!("{} = {}", rule.name(), values.get(rule)))
            .collect();
        return CommandReply::ok(listed.join(", "));
    };
    let Some(rule) = Rule::from_name(name) else {
        return CommandReply::error(format!("Game rule {name} is not supported yet"));
    };
    let Some(ArgValue::Bool(value)) = matched.arg("value") else {
        return CommandReply::ok(format!("{} = {}", rule.name(), values.get(rule)));
    };
    if let Some(values) = server.game_rules.set(rule, *value) {
        server.logins.send_all(&Control::GameRules(values));
        tracing::info!(
            "{} set the game rule {} to {value}",
            sender.name(),
            rule.name()
        );
    }
    CommandReply::ok(format!(
        "Game rule {} has been updated to {value}",
        rule.name()
    ))
}

/// The player named by the `player` argument.
fn target(matched: &Matched) -> Option<(Uuid, String)> {
    match matched.arg("player") {
        Some(ArgValue::Player(player)) => Uuid::parse_str(&player.uuid)
            .ok()
            .map(|uuid| (uuid, player.name.clone())),
        _ => None,
    }
}

fn missing_player() -> CommandReply {
    CommandReply::error("Name a player who is online")
}
