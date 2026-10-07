//! Chat, and other text shown to players.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};

/// How a [`Text`] message is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextType {
    /// Plain text in the chat.
    Raw = 0,
    /// A player's chat message, shown as `<source_name> message`.
    Chat = 1,
    /// A translation key, filled in with the parameters.
    Translation = 2,
    Popup = 3,
    JukeboxPopup = 4,
    /// Text above the hotbar.
    Tip = 5,
    System = 6,
    Whisper = 7,
    Announcement = 8,
    ObjectWhisper = 9,
    Object = 10,
    ObjectAnnouncement = 11,
}

impl TextType {
    const ALL: [Self; 12] = [
        Self::Raw,
        Self::Chat,
        Self::Translation,
        Self::Popup,
        Self::JukeboxPopup,
        Self::Tip,
        Self::System,
        Self::Whisper,
        Self::Announcement,
        Self::ObjectWhisper,
        Self::Object,
        Self::ObjectAnnouncement,
    ];

    fn layout(self) -> Layout {
        match self {
            Self::Raw
            | Self::Tip
            | Self::System
            | Self::ObjectWhisper
            | Self::Object
            | Self::ObjectAnnouncement => Layout::MessageOnly,
            Self::Chat | Self::Whisper | Self::Announcement => Layout::AuthorAndMessage,
            Self::Translation | Self::Popup | Self::JukeboxPopup => Layout::MessageAndParams,
        }
    }
}

/// The body variants of a Text packet, numbered as on the wire. Each
/// [`TextType`] belongs to exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    MessageOnly = 0,
    AuthorAndMessage = 1,
    MessageAndParams = 2,
}

/// A message for the client to show, or a chat message from the client.
///
/// Clients only send [`TextType::Chat`]. Chat, whispers and announcements
/// carry their author in `source_name`; translations and popups carry
/// `parameters`. Other types send neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub text_type: TextType,
    /// Whether the client translates `message` and `parameters`.
    pub needs_translation: bool,
    pub source_name: String,
    /// The text itself. Clients reject empty messages.
    pub message: String,
    /// Values filled into a translation or popup; at most four.
    pub parameters: Vec<String>,
    /// The sender's XUID. Clients only show chat whose XUID is empty or belongs
    /// to someone in their player list.
    pub xuid: String,
    /// Identifies the sender on platforms with their own chat rules.
    pub platform_chat_id: String,
    /// Shown instead of `message` to players who filter profanity.
    pub filtered_message: Option<String>,
}

impl Text {
    /// Longest message clients accept, in bytes.
    pub const MAX_MESSAGE_LEN: usize = 65_536;

    /// A message from the server itself, such as a plugin announcement.
    pub fn system(message: impl Into<String>) -> Self {
        Self {
            text_type: TextType::System,
            ..Self::raw(message)
        }
    }

    /// Plain text in the chat, from no one in particular.
    pub fn raw(message: impl Into<String>) -> Self {
        Self {
            text_type: TextType::Raw,
            needs_translation: false,
            source_name: String::new(),
            message: message.into(),
            parameters: Vec::new(),
            xuid: String::new(),
            platform_chat_id: String::new(),
            filtered_message: None,
        }
    }
}

impl Packet for Text {
    const ID: u32 = id::TEXT;
}

impl Encode for Text {
    fn encode_payload(&self, writer: &mut Writer) {
        let layout = self.text_type.layout();
        writer.bool(self.needs_translation);
        writer.var_u32(layout as u32);
        writer.u8(self.text_type as u8);
        if layout == Layout::AuthorAndMessage {
            writer.string(&self.source_name);
        }
        writer.string(&self.message);
        if layout == Layout::MessageAndParams {
            let count = u32::try_from(self.parameters.len()).expect("a handful of parameters");
            writer.var_u32(count);
            for parameter in &self.parameters {
                writer.string(parameter);
            }
        }
        writer.string(&self.xuid);
        writer.string(&self.platform_chat_id);
        writer.bool(self.filtered_message.is_some());
        if let Some(filtered) = &self.filtered_message {
            writer.string(filtered);
        }
    }
}

impl Decode for Text {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let needs_translation = reader.bool()?;
        let layout = reader.var_u32()?;
        let value = reader.u8()?;
        let text_type = TextType::ALL
            .into_iter()
            .find(|text_type| *text_type as u8 == value)
            .ok_or(DecodeError::InvalidValue {
                field: "text type",
                value: value.into(),
            })?;
        if layout != text_type.layout() as u32 {
            return Err(DecodeError::InvalidValue {
                field: "text layout",
                value: layout.into(),
            });
        }

        let source_name = match text_type.layout() {
            Layout::AuthorAndMessage => reader.string()?.to_owned(),
            _ => String::new(),
        };
        let message = reader.string()?.to_owned();
        let parameters = match text_type.layout() {
            Layout::MessageAndParams => {
                let count = reader.var_u32()?;
                (0..count)
                    .map(|_| reader.string().map(str::to_owned))
                    .collect::<Result<_, _>>()?
            }
            _ => Vec::new(),
        };
        let xuid = reader.string()?.to_owned();
        let platform_chat_id = reader.string()?.to_owned();
        let filtered_message = if reader.bool()? {
            Some(reader.string()?.to_owned())
        } else {
            None
        };
        Ok(Self {
            text_type,
            needs_translation,
            source_name,
            message,
            parameters,
            xuid,
            platform_chat_id,
            filtered_message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn decode_text(bytes: &[u8]) -> Result<Text, DecodeError> {
        let (header, payload) = read_header(bytes).unwrap();
        assert_eq!(header.id, id::TEXT);
        decode(payload)
    }

    #[test]
    fn raw_text_layout() {
        assert_eq!(
            Text::raw("hi").encode(),
            [
                0x09, // packet ID
                0x00, // not translated
                0x00, 0x00, // message-only body, raw
                0x02, b'h', b'i', // message
                0x00, 0x00, // no XUID or platform chat ID
                0x00, // no filtered message
            ]
        );
    }

    #[test]
    fn decodes_a_client_chat_message() {
        let mut bytes = vec![0x09, 0x00, 0x01, 0x01, 0x05];
        bytes.extend(b"Steve");
        bytes.push(0x05);
        bytes.extend(b"hello");
        bytes.push(0x10);
        bytes.extend(b"2535400000000000");
        bytes.extend([0x00, 0x01, 0x05]);
        bytes.extend(b"h***o");

        let text = decode_text(&bytes).unwrap();
        assert_eq!(
            text,
            Text {
                text_type: TextType::Chat,
                source_name: "Steve".into(),
                xuid: "2535400000000000".into(),
                filtered_message: Some("h***o".into()),
                ..Text::raw("hello")
            }
        );
        assert_eq!(text.encode(), bytes);
    }

    #[test]
    fn translations_carry_parameters() {
        let text = Text {
            text_type: TextType::Translation,
            needs_translation: true,
            parameters: vec!["Steve".into()],
            ..Text::raw("%multiplayer.player.joined")
        };
        let bytes = text.encode();
        assert_eq!(bytes[1..4], [0x01, 0x02, 0x02]);
        assert_eq!(decode_text(&bytes).unwrap(), text);
    }

    #[test]
    fn rejects_unknown_types_and_mismatched_layouts() {
        // Chat sent with the message-only layout.
        assert_eq!(
            decode_text(&[0x09, 0x00, 0x00, 0x01, 0x01, b'x', 0x00, 0x00, 0x00]),
            Err(DecodeError::InvalidValue {
                field: "text layout",
                value: 0
            })
        );
        assert_eq!(
            decode_text(&[0x09, 0x00, 0x00, 0x0C, 0x01, b'x', 0x00, 0x00, 0x00]),
            Err(DecodeError::InvalidValue {
                field: "text type",
                value: 12
            })
        );
    }
}
