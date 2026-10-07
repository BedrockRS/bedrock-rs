//! Slash commands: what the client may autocomplete, what it runs, and what
//! running it printed.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};

/// Parameter types, and the flags combined with them in
/// [`CommandParameter::param_type`].
pub mod arg_type {
    /// Marks a basic type (one of the constants below).
    pub const VALID: u32 = 0x10_0000;
    /// The low bits index [`super::AvailableCommands::enums`].
    pub const ENUM: u32 = 0x20_0000;
    /// The low bits index [`super::AvailableCommands::dynamic_enums`].
    pub const SOFT_ENUM: u32 = 0x400_0000;

    pub const INT: u32 = 1;
    pub const FLOAT: u32 = 3;
    /// A player name or selector, autocompleted with the players online.
    pub const TARGET: u32 = 8;
    /// One word, or a quoted string.
    pub const STRING: u32 = 56;
    /// The rest of the line.
    pub const RAW_TEXT: u32 = 70;
}

/// No alias enum, in [`CommandData::aliases_enum`].
pub const NO_ALIASES: u32 = u32::MAX;

/// Every command a player may use, sent whenever that changes. The client
/// shows them in `/help` and autocompletes them; running them is up to the
/// server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvailableCommands {
    /// The values of every enum, which [`CommandEnum::values`] index.
    pub enum_values: Vec<String>,
    pub enums: Vec<CommandEnum>,
    pub commands: Vec<CommandData>,
    /// Enums whose values may later change without resending everything.
    pub dynamic_enums: Vec<CommandEnum>,
}

/// A named set of values, such as a subcommand or `survival|creative`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEnum {
    /// The name shown as the parameter's type.
    pub name: String,
    /// Indices into [`AvailableCommands::enum_values`]; for dynamic enums,
    /// the values' positions in `dynamic_values`.
    pub values: Vec<u32>,
    /// The values themselves; only dynamic enums carry them inline.
    pub dynamic_values: Vec<String>,
}

/// One command. Command names must be lower case, or the client crashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandData {
    pub name: String,
    pub description: String,
    /// An index into [`AvailableCommands::enums`], or [`NO_ALIASES`].
    pub aliases_enum: u32,
    /// Each way of calling the command.
    pub overloads: Vec<Vec<CommandParameter>>,
}

/// One parameter of an overload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandParameter {
    pub name: String,
    /// An [`arg_type`] basic type with [`arg_type::VALID`], or an enum
    /// index with [`arg_type::ENUM`].
    pub param_type: u32,
    pub optional: bool,
}

impl Packet for AvailableCommands {
    const ID: u32 = id::AVAILABLE_COMMANDS;
}

impl Encode for AvailableCommands {
    fn encode_payload(&self, writer: &mut Writer) {
        write_list(writer, &self.enum_values, |writer, value| {
            writer.string(value);
        });
        // Chained subcommand values and suffixes: none.
        writer.var_u32(0);
        writer.var_u32(0);
        write_list(writer, &self.enums, |writer, command_enum| {
            writer.string(&command_enum.name);
            write_list(writer, &command_enum.values, |writer, index| {
                writer.u32_le(*index);
            });
        });
        // Chained subcommands: none.
        writer.var_u32(0);
        write_list(writer, &self.commands, |writer, command| {
            writer.string(&command.name);
            writer.string(&command.description);
            writer.u16_le(0); // flags
            // Permissions are checked by the server; every command is
            // listed for "any" player, and players only get the ones they
            // may run.
            writer.string("any");
            writer.u32_le(command.aliases_enum);
            writer.var_u32(0); // chained subcommand offsets
            write_list(writer, &command.overloads, |writer, overload| {
                writer.bool(false); // not chaining
                write_list(writer, overload, |writer, parameter| {
                    writer.string(&parameter.name);
                    writer.u32_le(parameter.param_type);
                    writer.bool(parameter.optional);
                    writer.u8(0); // options
                });
            });
        });
        write_list(writer, &self.dynamic_enums, |writer, command_enum| {
            writer.string(&command_enum.name);
            write_list(writer, &command_enum.dynamic_values, |writer, value| {
                writer.string(value);
            });
        });
        // Enum constraints: none.
        writer.var_u32(0);
    }
}

fn write_list<T>(writer: &mut Writer, items: &[T], mut write: impl FnMut(&mut Writer, &T)) {
    writer.var_u32(u32::try_from(items.len()).expect("lists stay far below u32::MAX"));
    for item in items {
        write(writer, item);
    }
}

/// Where a command came from. A [`CommandOutput`] echoes its request's origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOrigin {
    /// `player` for a player typing a command.
    pub origin: String,
    /// Identifies this run of the command; 16 bytes in wire order.
    pub uuid: [u8; 16],
    pub request_id: String,
    pub player_unique_id: i64,
}

impl CommandOrigin {
    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            origin: reader.string()?.to_owned(),
            uuid: reader.uuid()?,
            request_id: reader.string()?.to_owned(),
            player_unique_id: reader.u64_le()? as i64,
        })
    }

    fn write(&self, writer: &mut Writer) {
        writer.string(&self.origin);
        writer.uuid(self.uuid);
        writer.string(&self.request_id);
        writer.i64_le(self.player_unique_id);
    }
}

/// A command a player typed, with its leading `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub command_line: String,
    pub origin: CommandOrigin,
    pub internal: bool,
    pub version: String,
}

impl Packet for CommandRequest {
    const ID: u32 = id::COMMAND_REQUEST;
}

impl Decode for CommandRequest {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            command_line: reader.string()?.to_owned(),
            origin: CommandOrigin::read(reader)?,
            internal: reader.bool()?,
            version: reader.string()?.to_owned(),
        })
    }
}

/// What running a command printed, shown in the chat of the player who ran
/// it: successes in white, failures in red.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub origin: CommandOrigin,
    pub success_count: u32,
    pub messages: Vec<CommandMessage>,
}

/// One line of a [`CommandOutput`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMessage {
    pub success: bool,
    pub message: String,
}

impl Packet for CommandOutput {
    const ID: u32 = id::COMMAND_OUTPUT;
}

impl Encode for CommandOutput {
    fn encode_payload(&self, writer: &mut Writer) {
        self.origin.write(writer);
        writer.string("alloutput");
        writer.u32_le(self.success_count);
        write_list(writer, &self.messages, |writer, message| {
            writer.string(&message.message);
            writer.bool(message.success);
            writer.var_u32(0); // no translation parameters
        });
        writer.bool(false); // no data set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn origin() -> CommandOrigin {
        CommandOrigin {
            origin: "player".into(),
            uuid: [7; 16],
            request_id: String::new(),
            player_unique_id: 0,
        }
    }

    #[test]
    fn decodes_a_command_request() {
        let mut bytes = vec![0x4D, 0x05];
        bytes.extend(b"/help");
        bytes.push(0x06);
        bytes.extend(b"player");
        bytes.extend([7; 16]);
        bytes.push(0x00); // request ID
        bytes.extend([0; 8]); // player unique ID
        bytes.push(0x00); // not internal
        bytes.push(0x02);
        bytes.extend(b"52");

        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::COMMAND_REQUEST);
        let request: CommandRequest = decode(payload).unwrap();
        assert_eq!(
            request,
            CommandRequest {
                command_line: "/help".into(),
                origin: origin(),
                internal: false,
                version: "52".into(),
            }
        );
    }

    #[test]
    fn command_output_layout() {
        let output = CommandOutput {
            origin: origin(),
            success_count: 1,
            messages: vec![CommandMessage {
                success: true,
                message: "ok".into(),
            }],
        };
        let mut expected = vec![0x4F, 0x06];
        expected.extend(b"player");
        expected.extend([7; 16]);
        expected.push(0x00);
        expected.extend([0; 8]);
        expected.push(0x09);
        expected.extend(b"alloutput");
        expected.extend(1u32.to_le_bytes());
        expected.extend([0x01, 0x02, b'o', b'k', 0x01, 0x00]);
        expected.push(0x00);
        assert_eq!(output.encode(), expected);
    }

    #[test]
    fn available_commands_layout() {
        let commands = AvailableCommands {
            enum_values: vec!["set".into()],
            enums: vec![CommandEnum {
                name: "SubCommandset".into(),
                values: vec![0],
                dynamic_values: Vec::new(),
            }],
            commands: vec![CommandData {
                name: "warp".into(),
                description: "Warps".into(),
                aliases_enum: NO_ALIASES,
                overloads: vec![vec![CommandParameter {
                    name: "set".into(),
                    param_type: arg_type::ENUM | arg_type::VALID,
                    optional: false,
                }]],
            }],
            dynamic_enums: Vec::new(),
        };
        let mut expected = vec![0x4C];
        expected.extend([0x01, 0x03]);
        expected.extend(b"set");
        expected.extend([0x00, 0x00]); // chained values, suffixes
        expected.extend([0x01, 0x0D]);
        expected.extend(b"SubCommandset");
        expected.push(0x01);
        expected.extend(0u32.to_le_bytes());
        expected.push(0x00); // chained subcommands
        expected.extend([0x01, 0x04]);
        expected.extend(b"warp");
        expected.push(0x05);
        expected.extend(b"Warps");
        expected.extend([0x00, 0x00]); // flags
        expected.push(0x03);
        expected.extend(b"any");
        expected.extend(u32::MAX.to_le_bytes());
        expected.push(0x00); // chained offsets
        expected.extend([0x01, 0x00, 0x01, 0x03]); // one overload, one parameter
        expected.extend(b"set");
        expected.extend((arg_type::ENUM | arg_type::VALID).to_le_bytes());
        expected.extend([0x00, 0x00]);
        expected.extend([0x00, 0x00]); // dynamic enums, constraints
        assert_eq!(commands.encode(), expected);
    }
}
