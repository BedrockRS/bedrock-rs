//! Matching a command line against a command's tree: subcommands first, then
//! the arguments of the node reached, each parsed to its type.

use bedrockrs_plugins::{ArgKind, ArgSpec, ArgValue, CommandNode, CommandSpec, Player};

use super::Sender;

/// One word of a command line, or a quoted string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    /// Where the token starts in the line, so a text argument can take the
    /// rest of the line as typed.
    pub start: usize,
}

/// Splits a line into words. Double quotes group words, and `\"` inside them
/// is a quote; an unclosed quote runs to the end of the line.
pub fn tokenize(line: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut text = String::new();
        if c == '"' {
            chars.next();
            while let Some((_, c)) = chars.next() {
                match c {
                    '\\' if chars.peek().is_some_and(|&(_, next)| next == '"') => {
                        text.push('"');
                        chars.next();
                    }
                    '"' => break,
                    c => text.push(c),
                }
            }
        } else {
            while let Some(&(_, c)) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                text.push(c);
                chars.next();
            }
        }
        tokens.push(Token { text, start });
    }
    tokens
}

/// A command line matched to a node of the command's tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    /// The subcommands typed, by their canonical names.
    pub path: Vec<String>,
    /// The node's arguments by name; optional ones not typed are absent.
    pub args: Vec<(String, ArgValue)>,
}

impl Matched {
    pub fn arg(&self, name: &str) -> Option<&ArgValue> {
        self.args
            .iter()
            .find(|(arg, _)| arg == name)
            .map(|(_, value)| value)
    }
}

/// Matches the words after the command's name. `line` is the whole line the
/// tokens came from. `find_player` looks up an online player by name.
pub fn match_line(
    spec: &CommandSpec,
    line: &str,
    tokens: &[Token],
    sender: &Sender,
    find_player: &dyn Fn(&str) -> Option<Player>,
) -> Result<Matched, String> {
    let mut node = &spec.root;
    let mut path = Vec::new();
    let mut rest = tokens;
    while let Some((token, after)) = rest.split_first() {
        let Some((name, sub)) = node
            .subcommands
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&token.text))
        else {
            break;
        };
        path.push(name.clone());
        if !sender.may(sub.permission) {
            return Err(format!(
                "You do not have permission to use /{} {}.",
                spec.name,
                path.join(" ")
            ));
        }
        node = sub;
        rest = after;
    }

    if !node.runs_itself() {
        let problem = match rest.first() {
            Some(token) => format!("Unknown subcommand \"{}\".", token.text),
            None => "Missing a subcommand.".to_owned(),
        };
        return Err(format!(
            "{problem} Usage:\n{}",
            usages(spec, node, &path, sender).join("\n")
        ));
    }

    // The first overload that matches wins. If none does, the one that got
    // furthest says what was wrong (the first, on a tie).
    let mut best: Option<(usize, String)> = None;
    for overload in &node.overloads {
        match match_args(spec, &path, overload, line, rest, sender, find_player) {
            Ok(args) => return Ok(Matched { path, args }),
            Err((progress, problem)) => {
                if best
                    .as_ref()
                    .is_none_or(|(furthest, _)| progress > *furthest)
                {
                    best = Some((progress, problem));
                }
            }
        }
    }
    Err(best.expect("the node has overloads").1)
}

/// Parses `tokens` as one overload's arguments. A failure says how many
/// arguments matched before it, and what went wrong.
fn match_args(
    spec: &CommandSpec,
    path: &[String],
    arg_specs: &[ArgSpec],
    line: &str,
    tokens: &[Token],
    sender: &Sender,
    find_player: &dyn Fn(&str) -> Option<Player>,
) -> Result<Vec<(String, ArgValue)>, (usize, String)> {
    let usage = || usage(spec, path, arg_specs);
    let mut rest = tokens;
    let mut args = Vec::new();
    for arg in arg_specs {
        let Some((token, after)) = rest.split_first() else {
            if arg.optional {
                break;
            }
            return Err((
                args.len(),
                format!("Missing {}. Usage: {}", describe(arg), usage()),
            ));
        };
        let value = match &arg.kind {
            ArgKind::Text => {
                rest = &[];
                ArgValue::String(line[token.start..].trim_end().to_owned())
            }
            kind => {
                rest = after;
                parse_value(kind, &token.text, sender, find_player)
                    .map_err(|problem| (args.len(), format!("{problem} ({})", describe(arg))))?
            }
        };
        args.push((arg.name.clone(), value));
    }
    if let Some(extra) = rest.first() {
        return Err((
            args.len(),
            format!(
                "Too many arguments, starting at \"{}\". Usage: {}",
                line[extra.start..].trim_end(),
                usage()
            ),
        ));
    }
    Ok(args)
}

fn parse_value(
    kind: &ArgKind,
    text: &str,
    sender: &Sender,
    find_player: &dyn Fn(&str) -> Option<Player>,
) -> Result<ArgValue, String> {
    match kind {
        ArgKind::String | ArgKind::Text => Ok(ArgValue::String(text.to_owned())),
        ArgKind::Int => text
            .parse()
            .map(ArgValue::Int)
            .map_err(|_| format!("\"{text}\" is not a whole number")),
        ArgKind::Number => text
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(ArgValue::Number)
            .ok_or_else(|| format!("\"{text}\" is not a number")),
        ArgKind::Bool => match text.to_ascii_lowercase().as_str() {
            "true" => Ok(ArgValue::Bool(true)),
            "false" => Ok(ArgValue::Bool(false)),
            _ => Err(format!("\"{text}\" is not true or false")),
        },
        ArgKind::Player => {
            if text.eq_ignore_ascii_case("@s") {
                return match sender {
                    Sender::Player(player) => Ok(ArgValue::Player(player.plugin_player())),
                    Sender::Console => Err("the console is not a player".to_owned()),
                };
            }
            if text.starts_with('@') {
                return Err(format!(
                    "the selector {text} is not supported yet; use a player's name or @s"
                ));
            }
            find_player(text)
                .map(ArgValue::Player)
                .ok_or_else(|| format!("no player named \"{text}\" is online"))
        }
        ArgKind::Enum { values, .. } => values
            .iter()
            .find(|value| value.eq_ignore_ascii_case(text))
            .map(|value| ArgValue::String(value.clone()))
            .ok_or_else(|| format!("\"{text}\" is not one of: {}", values.join(", "))),
    }
}

/// An argument as usage lines show it: `<name: type>`, or `[name: type]`
/// when optional.
pub fn describe(arg: &ArgSpec) -> String {
    let inner = format!("{}: {}", arg.name, arg.kind.type_name());
    if arg.optional {
        format!("[{inner}]")
    } else {
        format!("<{inner}>")
    }
}

/// How to run the node at `path`: `/warp set <name: string>`.
pub fn usage(spec: &CommandSpec, path: &[String], args: &[ArgSpec]) -> String {
    let mut line = format!("/{}", spec.name);
    for word in path {
        line.push(' ');
        line.push_str(word);
    }
    for arg in args {
        line.push(' ');
        line.push_str(&describe(arg));
    }
    line
}

/// Every way `sender` may run `node` (at `path`) and the subcommands below
/// it, with each subcommand's description.
pub fn usages(
    spec: &CommandSpec,
    node: &CommandNode,
    path: &[String],
    sender: &Sender,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (path, node) in runnable(node, path, sender) {
        for overload in &node.overloads {
            let mut line = usage(spec, &path, overload);
            if !path.is_empty() && !node.description.is_empty() {
                line.push_str(" - ");
                line.push_str(&node.description);
            }
            lines.push(line);
        }
    }
    lines
}

/// The nodes at and below `node` that run and that `sender` may run, with
/// their paths, depth first.
pub fn runnable<'a>(
    node: &'a CommandNode,
    path: &[String],
    sender: &Sender,
) -> Vec<(Vec<String>, &'a CommandNode)> {
    let mut found = Vec::new();
    if !sender.may(node.permission) {
        return found;
    }
    if node.runs_itself() {
        found.push((path.to_vec(), node));
    }
    for (name, sub) in &node.subcommands {
        let mut below = path.to_vec();
        below.push(name.clone());
        found.extend(runnable(sub, &below, sender));
    }
    found
}

#[cfg(test)]
mod tests {
    use bedrockrs_plugins::Permission;
    use uuid::Uuid;

    use super::*;
    use crate::commands::PlayerSender;

    fn warp() -> CommandSpec {
        CommandSpec {
            name: "warp".into(),
            description: "Warps".into(),
            aliases: Vec::new(),
            root: CommandNode {
                description: "Warps".into(),
                permission: Permission::Any,
                overloads: vec![vec![ArgSpec::required("name", ArgKind::String)]],
                subcommands: vec![
                    (
                        "admin".into(),
                        CommandNode::group(vec![(
                            "reload".into(),
                            CommandNode::runs().described("Reload the warps"),
                        )])
                        .permission(Permission::Operator),
                    ),
                    (
                        "set".into(),
                        CommandNode::runs_with(vec![
                            ArgSpec::required("name", ArgKind::String),
                            ArgSpec::optional("note", ArgKind::Text),
                        ])
                        .described("Make a warp"),
                    ),
                    (
                        "give".into(),
                        CommandNode::runs_with(vec![
                            ArgSpec::required("player", ArgKind::Player),
                            ArgSpec::required("count", ArgKind::Int),
                            ArgSpec::optional("mode", ArgKind::game_mode()),
                            ArgSpec::optional("loud", ArgKind::Bool),
                        ]),
                    ),
                ],
            },
        }
    }

    fn steve(operator: bool) -> Sender {
        Sender::Player(PlayerSender {
            uuid: Uuid::nil(),
            name: "Steve".into(),
            operator,
        })
    }

    fn alex() -> Player {
        Player {
            name: "Alex".into(),
            uuid: Uuid::nil().to_string(),
        }
    }

    fn run(line: &str, sender: &Sender) -> Result<Matched, String> {
        let tokens = tokenize(line);
        let find = |name: &str| name.eq_ignore_ascii_case("alex").then(alex);
        match_line(&warp(), line, &tokens[1..], sender, &find)
    }

    fn string(text: &str) -> ArgValue {
        ArgValue::String(text.into())
    }

    #[test]
    fn quotes_group_words() {
        let texts: Vec<_> = tokenize(r#"tell "Cool Guy" hi  "say \"x\"" "open"#)
            .into_iter()
            .map(|token| token.text)
            .collect();
        assert_eq!(texts, ["tell", "Cool Guy", "hi", r#"say "x""#, "open"]);
    }

    #[test]
    fn subcommands_take_precedence_over_arguments() {
        let matched = run("warp spawn", &steve(false)).unwrap();
        assert_eq!(matched.path, Vec::<String>::new());
        assert_eq!(matched.args, [("name".into(), string("spawn"))]);

        let matched = run("warp SET home a note  with   spaces ", &steve(false)).unwrap();
        assert_eq!(matched.path, ["set"]);
        assert_eq!(
            matched.args,
            [
                ("name".into(), string("home")),
                ("note".into(), string("a note  with   spaces")),
            ]
        );
    }

    #[test]
    fn arguments_are_parsed_to_their_types() {
        let matched = run("warp give alex 5 c true", &steve(false)).unwrap();
        assert_eq!(
            matched.args,
            [
                ("player".into(), ArgValue::Player(alex())),
                ("count".into(), ArgValue::Int(5)),
                ("mode".into(), string("c")),
                ("loud".into(), ArgValue::Bool(true)),
            ]
        );
        // @s is whoever runs the command.
        let matched = run("warp give @s 1", &steve(false)).unwrap();
        assert_eq!(
            matched.arg("player"),
            Some(&ArgValue::Player(Player {
                name: "Steve".into(),
                uuid: Uuid::nil().to_string()
            }))
        );
    }

    #[test]
    fn mistakes_explain_themselves() {
        let error = |line| run(line, &steve(false)).unwrap_err();
        assert_eq!(
            error("warp give alex lots"),
            "\"lots\" is not a whole number (<count: int>)"
        );
        assert_eq!(
            error("warp give bob 1"),
            "no player named \"bob\" is online (<player: target>)"
        );
        assert_eq!(
            error("warp give alex 1 hardcore"),
            "\"hardcore\" is not one of: survival, creative, adventure, spectator, s, c, a, \
             default, d ([mode: GameMode])"
        );
        assert_eq!(
            error("warp set"),
            "Missing <name: string>. Usage: /warp set <name: string> [note: text]"
        );
        assert_eq!(
            error("warp spawn now"),
            "Too many arguments, starting at \"now\". Usage: /warp <name: string>"
        );
        assert!(error("warp give @a 1").contains("not supported yet"));
        assert_eq!(
            run("warp give @s 1", &Sender::Console).unwrap_err(),
            "the console is not a player (<player: target>)"
        );
    }

    #[test]
    fn groups_need_a_subcommand_and_check_permissions() {
        assert_eq!(
            run("warp admin reload", &steve(false)).unwrap_err(),
            "You do not have permission to use /warp admin."
        );
        assert_eq!(
            run("warp admin", &steve(true)).unwrap_err(),
            "Missing a subcommand. Usage:\n/warp admin reload - Reload the warps"
        );
        assert_eq!(
            run("warp admin fly", &steve(true)).unwrap_err(),
            "Unknown subcommand \"fly\". Usage:\n/warp admin reload - Reload the warps"
        );
        assert_eq!(
            run("warp admin reload", &steve(true)).unwrap().path,
            ["admin", "reload"]
        );
        assert!(run("warp admin reload", &Sender::Console).is_ok());
    }

    #[test]
    fn overloads_are_tried_in_order() {
        let spec = CommandSpec {
            name: "gamemode".into(),
            description: String::new(),
            aliases: Vec::new(),
            root: CommandNode::runs_with(vec![
                ArgSpec::required("gameMode", ArgKind::game_mode()),
                ArgSpec::optional("player", ArgKind::Player),
            ])
            .or_with(vec![
                ArgSpec::required("gameMode", ArgKind::Int),
                ArgSpec::optional("player", ArgKind::Player),
            ]),
        };
        let run = |line: &str| {
            let tokens = tokenize(line);
            let find = |name: &str| name.eq_ignore_ascii_case("alex").then(alex);
            match_line(&spec, line, &tokens[1..], &steve(false), &find)
        };
        assert_eq!(
            run("gamemode creative").unwrap().args,
            [("gameMode".into(), string("creative"))]
        );
        assert_eq!(
            run("gamemode 1 alex").unwrap().args,
            [
                ("gameMode".into(), ArgValue::Int(1)),
                ("player".into(), ArgValue::Player(alex())),
            ]
        );
        // Neither matches: the overload that got furthest explains.
        assert_eq!(
            run("gamemode creative bob").unwrap_err(),
            "no player named \"bob\" is online ([player: target])"
        );
        assert!(
            run("gamemode sp")
                .unwrap_err()
                .starts_with("\"sp\" is not one of: survival, creative"),
            "on a tie, the first overload explains"
        );
        assert_eq!(
            usages(&spec, &spec.root, &[], &steve(false)),
            [
                "/gamemode <gameMode: GameMode> [player: target]",
                "/gamemode <gameMode: int> [player: target]",
            ]
        );
    }

    #[test]
    fn usages_list_what_the_sender_may_run() {
        let spec = warp();
        let lines = usages(&spec, &spec.root, &[], &steve(false));
        assert_eq!(
            lines,
            [
                "/warp <name: string>",
                "/warp set <name: string> [note: text] - Make a warp",
                "/warp give <player: target> <count: int> [mode: GameMode] [loud: Boolean]",
            ]
        );
        assert_eq!(usages(&spec, &spec.root, &[], &steve(true)).len(), 4);
    }
}
