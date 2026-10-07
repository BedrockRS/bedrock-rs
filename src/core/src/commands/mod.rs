//! Slash commands, from players and from the console.
//!
//! Every command is a [`CommandSpec`]: a tree of subcommands, each with
//! typed arguments and a permission. The server's own commands ([`builtin`])
//! and the commands plugins register are described the same way, so plugin
//! commands get the same parsing, error messages, `/help` entries and client
//! autocompletion, subcommands included.
//!
//! A line is matched against its command's tree ([`parse`]). Built-in commands
//! then run here; plugin commands go to the plugin that registered them,
//! which answers with a [`CommandReply`].

pub mod builtin;
pub mod client;
pub mod parse;

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::time::Duration;

use bedrockrs_plugins::{
    CommandCall, CommandReply, CommandSender, CommandSpec, Permission, Player, PluginCommand,
};
use uuid::Uuid;

use crate::server::Server;
use builtin::Builtin;

/// Longest a plugin may take to answer a command.
const PLUGIN_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// Who runs a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    /// Someone at the server's console, who may run everything.
    Console,
    Player(PlayerSender),
}

/// A player running a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerSender {
    pub uuid: Uuid,
    pub name: String,
    pub operator: bool,
}

impl PlayerSender {
    /// The player as plugins see them.
    pub fn plugin_player(&self) -> Player {
        Player {
            name: self.name.clone(),
            uuid: self.uuid.to_string(),
        }
    }
}

impl Sender {
    /// Whether the sender may run what needs `permission`.
    pub fn may(&self, permission: Permission) -> bool {
        match (self, permission) {
            (_, Permission::Any) | (Self::Console, _) => true,
            (Self::Player(player), Permission::Operator) => player.operator,
        }
    }

    /// The name logs call the sender by, at the start of a sentence.
    pub fn name(&self) -> &str {
        match self {
            Self::Console => "The console",
            Self::Player(player) => &player.name,
        }
    }

    fn plugin_sender(&self) -> CommandSender {
        match self {
            Self::Console => CommandSender::Console,
            Self::Player(player) => CommandSender::Player(player.plugin_player()),
        }
    }
}

/// Who carries a command out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Builtin(Builtin),
    /// The plugin of this name.
    Plugin(String),
}

/// A command the server knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub spec: CommandSpec,
    pub owner: Owner,
}

impl Registered {
    /// Whether `name` is this command's name or one of its aliases.
    fn answers_to(&self, name: &str) -> bool {
        self.spec.name == name || self.spec.aliases.iter().any(|alias| alias == name)
    }

    /// Whether `sender` may run any part of this command.
    pub fn visible_to(&self, sender: &Sender) -> bool {
        !parse::runnable(&self.spec.root, &[], sender).is_empty()
    }
}

/// Every command, built-in ones first. Plugin commands change as plugins load
/// and unload.
#[derive(Debug)]
pub struct Commands {
    all: RwLock<Vec<Arc<Registered>>>,
}

impl Default for Commands {
    fn default() -> Self {
        Self::new()
    }
}

impl Commands {
    /// The built-in commands alone.
    pub fn new() -> Self {
        Self {
            all: RwLock::new(builtin::all()),
        }
    }

    /// The command answering to `name` (lower case), if any.
    pub fn find(&self, name: &str) -> Option<Arc<Registered>> {
        self.all()
            .iter()
            .find(|command| command.answers_to(name))
            .cloned()
    }

    /// Every command, in the order `/help` lists them.
    pub fn list(&self) -> Vec<Arc<Registered>> {
        self.all().clone()
    }

    /// Replaces the plugin commands. A command whose name is taken (by a
    /// built-in command or a plugin before it) is left out, and an alias that
    /// is taken is dropped. Returns whether anything changed.
    pub fn set_plugin_commands(&self, commands: Vec<PluginCommand>) -> bool {
        let mut all = builtin::all();
        for PluginCommand { plugin, mut spec } in commands {
            if let Some(holder) = all.iter().find(|other| other.answers_to(&spec.name)) {
                tracing::warn!(
                    "Didn't add /{} from plugin {plugin}: the name is taken by /{}",
                    spec.name,
                    holder.spec.name
                );
                continue;
            }
            spec.aliases.retain(|alias| {
                let taken = all.iter().any(|other| other.answers_to(alias));
                if taken {
                    tracing::warn!(
                        "Dropped the alias /{alias} of /{} from plugin {plugin}: it is taken",
                        spec.name
                    );
                }
                !taken && *alias != spec.name
            });
            all.push(Arc::new(Registered {
                spec,
                owner: Owner::Plugin(plugin),
            }));
        }
        let mut current = self.all.write().unwrap_or_else(PoisonError::into_inner);
        if *current == all {
            return false;
        }
        *current = all;
        true
    }

    fn all(&self) -> RwLockReadGuard<'_, Vec<Arc<Registered>>> {
        self.all.read().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Server {
    /// Runs a command line, with or without its leading `/`, and says what it
    /// printed.
    pub async fn run_command(&self, sender: &Sender, line: &str) -> CommandReply {
        let line = line.trim();
        let line = line.strip_prefix('/').unwrap_or(line);
        let tokens = parse::tokenize(line);
        let Some((name, rest)) = tokens.split_first() else {
            return CommandReply::error("Type /help for a list of commands.");
        };
        let name = name.text.to_ascii_lowercase();
        let unknown = || {
            CommandReply::error(format!(
                "Unknown command: /{name}. Type /help for a list of commands."
            ))
        };
        let Some(command) = self.commands.find(&name) else {
            return unknown();
        };
        if !command.visible_to(sender) {
            return CommandReply::error(format!("You do not have permission to use /{name}."));
        }
        let find_player = |name: &str| {
            self.players.find(name).map(|(uuid, name)| Player {
                name,
                uuid: uuid.to_string(),
            })
        };
        let matched = match parse::match_line(&command.spec, line, rest, sender, &find_player) {
            Ok(matched) => matched,
            Err(problem) => return CommandReply::error(problem),
        };
        // The console already shows what it typed.
        if let Sender::Player(player) = sender {
            tracing::info!("{} ran /{line}", player.name);
        }
        match &command.owner {
            Owner::Builtin(builtin) => builtin.run(self, sender, &matched),
            Owner::Plugin(plugin) => {
                let call = CommandCall {
                    plugin: plugin.clone(),
                    command: command.spec.name.clone(),
                    path: matched.path,
                    args: matched.args,
                    sender: sender.plugin_sender(),
                };
                match tokio::time::timeout(PLUGIN_COMMAND_TIMEOUT, self.plugins.run_command(call))
                    .await
                {
                    Ok(Some(reply)) => reply,
                    Ok(None) => unknown(),
                    Err(_) => {
                        tracing::warn!(
                            "Plugin {plugin} took too long to answer /{}",
                            command.spec.name
                        );
                        CommandReply::error("The command took too long to answer.")
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bedrockrs_plugins::{CommandNode, Dispatcher};

    use super::*;
    use crate::auth::Authenticator;
    use crate::damage::DamageCause;
    use crate::game_mode::GameMode;
    use crate::logins::Control;
    use crate::world::World;

    fn plugin_command(plugin: &str, name: &str, aliases: &[&str]) -> PluginCommand {
        PluginCommand {
            plugin: plugin.into(),
            spec: CommandSpec {
                name: name.into(),
                description: String::new(),
                aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
                root: CommandNode::runs(),
            },
        }
    }

    #[test]
    fn plugin_commands_cannot_take_names_already_in_use() {
        let commands = Commands::new();
        assert!(commands.set_plugin_commands(vec![
            plugin_command("a", "warp", &["w", "help", "warp"]),
            plugin_command("b", "warp", &[]),
            plugin_command("b", "gamemode", &[]),
            plugin_command("b", "home", &["w", "h"]),
        ]));
        let warp = commands.find("w").unwrap();
        assert_eq!(warp.owner, Owner::Plugin("a".into()));
        assert_eq!(warp.spec.aliases, ["w"], "help is taken, warp is the name");
        assert_eq!(
            commands.find("gamemode").unwrap().owner,
            Owner::Builtin(Builtin::GameMode)
        );
        let home = commands.find("h").unwrap();
        assert_eq!(
            (home.spec.name.as_str(), &home.spec.aliases[..]),
            ("home", &["h".to_owned()][..])
        );

        assert!(
            !commands.set_plugin_commands(vec![
                plugin_command("a", "warp", &["w", "help", "warp"]),
                plugin_command("b", "warp", &[]),
                plugin_command("b", "gamemode", &[]),
                plugin_command("b", "home", &["w", "h"]),
            ]),
            "nothing changed"
        );
        assert!(commands.set_plugin_commands(Vec::new()));
        assert!(commands.find("warp").is_none());
    }

    #[tokio::test]
    async fn unknown_and_forbidden_commands_are_refused() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        let steve = Sender::Player(PlayerSender {
            uuid: Uuid::new_v4(),
            name: "Steve".into(),
            operator: false,
        });
        let error = |reply: CommandReply| {
            assert!(!reply.succeeded());
            reply.lines[0].text.clone()
        };
        assert_eq!(
            error(server.run_command(&steve, "/fly").await),
            "Unknown command: /fly. Type /help for a list of commands."
        );
        assert_eq!(
            error(server.run_command(&steve, "/gamemode creative").await),
            "You do not have permission to use /gamemode."
        );
        assert_eq!(
            error(server.run_command(&steve, "  /  ").await),
            "Type /help for a list of commands."
        );
        assert!(
            server.run_command(&steve, "/HELP").await.succeeded(),
            "names are not case sensitive"
        );
    }

    #[tokio::test]
    async fn gamemode_takes_vanilla_names_and_numbers() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        )
        .with_default_game_mode(GameMode::Adventure);
        let uuid = Uuid::new_v4();
        let (controls, mut received) = tokio::sync::mpsc::channel(8);
        let _claim = server.logins.claim(uuid, controls);
        let steve = Sender::Player(PlayerSender {
            uuid,
            name: "Steve".into(),
            operator: true,
        });

        for (line, mode) in [
            ("/gamemode creative", GameMode::Creative),
            ("/gamemode S", GameMode::Survival),
            ("/gamemode spectator", GameMode::Spectator),
            ("/gamemode 1", GameMode::Creative),
            ("/gamemode 0", GameMode::Survival),
            ("/gamemode default", GameMode::Adventure),
            ("/gamemode d @s", GameMode::Adventure),
        ] {
            let reply = server.run_command(&steve, line).await;
            assert!(reply.succeeded(), "{line}: {reply:?}");
            assert_eq!(
                received.try_recv().unwrap(),
                Control::SetGameMode(mode),
                "{line}"
            );
        }
        for line in ["/gamemode sp", "/gamemode 6", "/gamemode hardcore"] {
            assert!(
                !server.run_command(&steve, line).await.succeeded(),
                "{line}"
            );
            assert!(received.try_recv().is_err(), "{line}");
        }
    }

    #[tokio::test]
    async fn kill_and_gamerule_work_as_in_vanilla() {
        let server = Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        );
        let uuid = Uuid::new_v4();
        let (controls, mut received) = tokio::sync::mpsc::channel(8);
        let _claim = server.logins.claim(uuid, controls);
        let steve = Sender::Player(PlayerSender {
            uuid,
            name: "Steve".into(),
            operator: true,
        });

        let reply = server.run_command(&steve, "/kill").await;
        assert_eq!(reply.lines[0].text, "Killed Steve");
        assert_eq!(
            received.try_recv().unwrap(),
            Control::Damage {
                cause: DamageCause::SelfDestruct,
                amount: f32::MAX
            }
        );
        assert!(
            !server
                .run_command(&Sender::Console, "kill")
                .await
                .succeeded()
        );

        let listed = server.run_command(&steve, "/gamerule").await;
        assert!(
            listed.lines[0].text.contains("keepinventory = false"),
            "{listed:?}"
        );
        let reply = server
            .run_command(&steve, "/gamerule keepInventory true")
            .await;
        assert_eq!(
            reply.lines[0].text,
            "Game rule keepinventory has been updated to true"
        );
        assert!(server.game_rules.values().keepinventory);
        let Control::GameRules(values) = received.try_recv().unwrap() else {
            panic!("every player hears of the change");
        };
        assert!(values.keepinventory);
        let reply = server.run_command(&steve, "/gamerule keepinventory").await;
        assert_eq!(reply.lines[0].text, "keepinventory = true");
        // Setting it to what it is tells nobody.
        server
            .run_command(&steve, "/gamerule keepinventory true")
            .await;
        assert!(received.try_recv().is_err());
        assert!(
            !server
                .run_command(&steve, "/gamerule dofiretick false")
                .await
                .succeeded()
        );
    }
}
