//! Chat, slash commands, game modes and operator status.

use bedrockrs_plugins::CommandReply;
use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    CommandMessage, CommandOrigin, CommandOutput, CommandRequest, SetPlayerGameType, Text, TextType,
};

use crate::commands::{PlayerSender, Sender};
use crate::game_mode::GameMode;

use super::{MAX_CHAT_LENGTH, Reply, Session, SessionEvent};

impl Session {
    /// Whether the player is an operator; set from the operator list at
    /// login, before they spawn.
    pub fn set_operator(&mut self, operator: bool) {
        self.operator = operator;
    }

    /// The player, as someone running commands.
    pub fn command_sender(&self) -> Sender {
        Sender::Player(PlayerSender {
            uuid: self.uuid,
            name: self.player.clone(),
            operator: self.operator,
        })
    }

    /// Changes the player's game mode: their client is told, along with
    /// what the mode lets them do. A player who may no longer fly stops.
    pub fn set_game_mode(&mut self, mode: GameMode) -> Reply {
        self.game_mode = mode;
        self.flying = match mode {
            GameMode::Spectator => true,
            _ => self.flying && mode.may_fly(),
        };
        tracing::info!("{}'s game mode is now {}", self.player, mode.name());
        if !self.stage.in_world() {
            return Reply::default();
        }
        Reply {
            packets: vec![
                SetPlayerGameType {
                    game_type: mode.id(),
                }
                .encode(),
                self.own_abilities().encode(),
            ],
            events: vec![
                SessionEvent::GameModeChanged(mode),
                SessionEvent::Flying(self.flying),
            ],
            ..Reply::default()
        }
    }

    /// The player became an operator or stopped being one: their client is
    /// told their new permissions, and they are told in chat.
    pub fn operator_changed(&mut self, operator: bool) -> Reply {
        self.operator = operator;
        if !self.stage.in_world() {
            return Reply::default();
        }
        let message = if operator {
            "§eYou are now an operator."
        } else {
            "§eYou are no longer an operator."
        };
        Reply::send(vec![
            self.own_abilities().encode(),
            Text::system(message).encode(),
        ])
    }

    /// What running a command printed, for the client that sent it.
    pub fn command_output(&self, origin: CommandOrigin, reply: &CommandReply) -> Vec<u8> {
        CommandOutput {
            origin,
            success_count: u32::from(reply.succeeded()),
            messages: reply
                .lines
                .iter()
                .map(|line| CommandMessage {
                    success: line.success,
                    message: line.text.clone(),
                })
                .collect(),
        }
        .encode()
    }

    /// Relays the player's chat messages; clients send no other kind of text.
    pub(super) fn text(&mut self, text: Text) -> Reply {
        if text.text_type != TextType::Chat {
            tracing::debug!(player = %self.player, text_type = ?text.text_type, "ignoring text that is not chat");
            return Reply::default();
        }
        // Line breaks and other control characters would let a player fake
        // extra lines, such as a message from someone else.
        let message: String = text
            .message
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let message = message.trim();
        if message.is_empty() {
            return Reply::default();
        }
        if message.chars().count() > MAX_CHAT_LENGTH {
            let warning =
                format!("§cChat messages can be at most {MAX_CHAT_LENGTH} characters long.");
            return Reply::send(vec![Text::raw(warning).encode()]);
        }
        Reply {
            events: vec![SessionEvent::Chat(message.to_owned())],
            ..Reply::default()
        }
    }

    /// A slash command the player typed, for the server to run. Control
    /// characters become spaces, as in chat.
    pub(super) fn command_request(&mut self, request: CommandRequest) -> Reply {
        let line: String = request
            .command_line
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if line.chars().count() > MAX_CHAT_LENGTH {
            let warning = format!("§cCommands can be at most {MAX_CHAT_LENGTH} characters long.");
            return Reply::send(vec![Text::raw(warning).encode()]);
        }
        Reply {
            events: vec![SessionEvent::Command {
                line,
                origin: request.origin,
            }],
            ..Reply::default()
        }
    }
}
