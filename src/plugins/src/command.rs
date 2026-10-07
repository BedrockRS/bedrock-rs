//! Slash commands as data: what a command looks like, how it was called and
//! what it answered. The server's own commands and plugin commands are
//! described the same way; the server parses command lines against these
//! descriptions and tells clients about them.

use crate::Player;

/// Who may run a command or subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    /// Every player.
    Any,
    /// Operators and the server console.
    Operator,
}

impl Permission {
    /// The name plugins use for it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Operator => "operator",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "any" => Some(Self::Any),
            "operator" => Some(Self::Operator),
            _ => None,
        }
    }
}

/// A command: its name, how it is described, and its tree of subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Lower case, without the `/`.
    pub name: String,
    pub description: String,
    /// Other names that run the same command.
    pub aliases: Vec<String>,
    pub root: CommandNode,
}

/// A command or one of its subcommands, at any depth. A node runs when it has
/// overloads, and may also lead to subcommands. A subcommand's name wins over
/// an argument when both match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandNode {
    /// Shown by `/help <command>`; the command's own description for its root.
    pub description: String,
    /// Who may run this node and the subcommands below it; subcommands can
    /// only narrow it.
    pub permission: Permission,
    /// Each way of running this node, as its list of arguments, tried in
    /// order: `/gamemode <gameMode: GameMode>` and `/gamemode <gameMode: int>`.
    /// Empty for a node that only leads to subcommands.
    pub overloads: Vec<Vec<ArgSpec>>,
    /// Subcommands by name, in the order they are listed.
    pub subcommands: Vec<(String, CommandNode)>,
}

impl CommandNode {
    /// A node that runs with no arguments.
    pub fn runs() -> Self {
        Self::runs_with(Vec::new())
    }

    /// A node that runs with these arguments.
    pub fn runs_with(args: Vec<ArgSpec>) -> Self {
        Self {
            description: String::new(),
            permission: Permission::Any,
            overloads: vec![args],
            subcommands: Vec::new(),
        }
    }

    /// Another way of running this node, tried after the ones before it.
    pub fn or_with(mut self, args: Vec<ArgSpec>) -> Self {
        self.overloads.push(args);
        self
    }

    /// Whether the node runs, rather than only leading to subcommands.
    pub fn runs_itself(&self) -> bool {
        !self.overloads.is_empty()
    }

    /// A node that only leads to subcommands.
    pub fn group(subcommands: Vec<(String, CommandNode)>) -> Self {
        Self {
            description: String::new(),
            permission: Permission::Any,
            overloads: Vec::new(),
            subcommands,
        }
    }

    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn permission(mut self, permission: Permission) -> Self {
        self.permission = permission;
        self
    }

    /// The node at `path` below this one.
    pub fn find(&self, path: &[String]) -> Option<&CommandNode> {
        match path.split_first() {
            None => Some(self),
            Some((first, rest)) => self
                .subcommands
                .iter()
                .find(|(name, _)| name == first)
                .and_then(|(_, node)| node.find(rest)),
        }
    }
}

/// One argument of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: String,
    pub kind: ArgKind,
    /// Optional arguments may only be followed by optional arguments.
    pub optional: bool,
}

impl ArgSpec {
    pub fn required(name: impl Into<String>, kind: ArgKind) -> Self {
        Self {
            name: name.into(),
            kind,
            optional: false,
        }
    }

    pub fn optional(name: impl Into<String>, kind: ArgKind) -> Self {
        Self {
            optional: true,
            ..Self::required(name, kind)
        }
    }
}

/// What an argument accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgKind {
    /// One word, or a quoted string with spaces.
    String,
    /// Everything left on the line; only the last argument.
    Text,
    /// A whole number.
    Int,
    /// Any number.
    Number,
    /// `true` or `false`.
    Bool,
    /// An online player, by name or `@s` for whoever runs the command.
    Player,
    /// One of `values`, matched without regard to case. `name` is shown as
    /// the argument's type, e.g. `<gameMode: GameMode>`.
    Enum { name: String, values: Vec<String> },
}

impl ArgKind {
    /// A game mode, by any name [`GAME_MODE_VALUES`] lists.
    pub fn game_mode() -> Self {
        Self::Enum {
            name: "GameMode".into(),
            values: GAME_MODE_VALUES.map(String::from).to_vec(),
        }
    }

    /// How the argument's type shows in usage lines, `<name: type>`, named
    /// as the client names it while the command is typed.
    pub fn type_name(&self) -> &str {
        match self {
            Self::String => "string",
            Self::Text => "text",
            Self::Int => "int",
            Self::Number => "float",
            Self::Bool => "Boolean",
            Self::Player => "target",
            Self::Enum { name, .. } => name,
        }
    }
}

/// The game mode names commands and plugins accept: vanilla's `GameMode`
/// enum, short forms included. `default` and `d` stand for the server's
/// default game mode. (Vanilla `/gamemode` also takes 0, 1 and 2, as a
/// separate whole-number overload.)
pub const GAME_MODE_VALUES: [&str; 9] = [
    "survival",
    "creative",
    "adventure",
    "spectator",
    "s",
    "c",
    "a",
    "default",
    "d",
];

/// An argument as parsed.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgValue {
    String(String),
    Int(i64),
    Number(f64),
    Bool(bool),
    Player(Player),
}

/// Who ran a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSender {
    /// Someone typing into the server's console.
    Console,
    Player(Player),
}

/// A plugin command someone ran, parsed and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandCall {
    /// The plugin that registered the command.
    pub plugin: String,
    /// The command's name, whichever alias was typed.
    pub command: String,
    /// The subcommands typed, from the command down to the node that runs.
    pub path: Vec<String>,
    /// The node's arguments, by name; optional ones left out are absent.
    pub args: Vec<(String, ArgValue)>,
    pub sender: CommandSender,
}

/// What a command printed, line by line; shown to whoever ran it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandReply {
    pub lines: Vec<ReplyLine>,
}

/// One line of a [`CommandReply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyLine {
    /// Failures show in red.
    pub success: bool,
    pub text: String,
}

impl CommandReply {
    pub fn ok(text: impl Into<String>) -> Self {
        let mut reply = Self::default();
        reply.push_ok(text);
        reply
    }

    pub fn error(text: impl Into<String>) -> Self {
        let mut reply = Self::default();
        reply.push_error(text);
        reply
    }

    pub fn push_ok(&mut self, text: impl Into<String>) {
        self.lines.push(ReplyLine {
            success: true,
            text: text.into(),
        });
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.lines.push(ReplyLine {
            success: false,
            text: text.into(),
        });
    }

    /// Whether the command succeeded: it printed no errors.
    pub fn succeeded(&self) -> bool {
        self.lines.iter().all(|line| line.success)
    }
}

/// A command registered by a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    pub plugin: String,
    pub spec: CommandSpec,
}
