//! The server console: one line per log event, as
//! `<YY/MM/DD HH:MM:SS.SSS> LEVEL [target] message key=value…`, with Minecraft's
//! `§` colour codes shown as terminal colours.

use std::fmt::{self, Write as _};

use bedrockrs_plugins::{PLUGIN_FIELD, PLUGIN_TARGET};
use chrono::{DateTime, Local};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

const RESET: &str = "\x1b[0m";
/// Bright black: the timestamp and field names.
const GREY: &str = "\x1b[90m";
const CYAN: &str = "\x1b[36m";

/// How timestamps are written (chrono's `strftime` syntax); `%.3f` is milliseconds.
const TIMESTAMP: &str = "%y/%m/%d %H:%M:%S%.3f";

/// Formats console lines as `<26/09/26 14:30:05.123> INF [hello] Hello`:
///
/// - the local time, grey;
/// - the level in three letters, green, yellow or red for INF, WRN and ERR
///   (blue DBG, magenta TRC);
/// - the target in cyan brackets: the plugin's name for plugin output, and
///   otherwise the crate that logged, `bedrockrs` for the server's core;
/// - the message, then any other fields as grey `key=` and their value.
///
/// `§` codes in messages and values become ANSI colours, or are removed
/// when the console does not take colours.
#[derive(Debug, Clone, Copy)]
pub struct ConsoleFormat {
    pub ansi: bool,
}

impl<S, N> FormatEvent<S, N> for ConsoleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let metadata = event.metadata();
        self.write_line(
            &mut writer,
            Local::now(),
            *metadata.level(),
            metadata.target(),
            fields,
        )
    }
}

impl ConsoleFormat {
    fn write_line(
        &self,
        out: &mut impl fmt::Write,
        time: DateTime<Local>,
        level: Level,
        target: &str,
        mut fields: Fields,
    ) -> fmt::Result {
        let plugin_name = if target == PLUGIN_TARGET {
            fields
                .others
                .iter()
                .position(|(name, _)| *name == PLUGIN_FIELD)
                .map(|index| fields.others.remove(index).1)
        } else {
            None
        };
        let label = plugin_name.as_deref().unwrap_or_else(|| source(target));
        let time = time.format(TIMESTAMP);
        let level_name = level_name(level);
        if self.ansi {
            write!(
                out,
                "{GREY}<{time}>{RESET} {}{level_name}{RESET} {CYAN}[{label}]{RESET} ",
                level_colour(level)
            )?;
        } else {
            write!(out, "<{time}> {level_name} [{label}] ")?;
        }
        out.write_str(&colourize(&fields.message, self.ansi))?;
        for (name, value) in &fields.others {
            if self.ansi {
                write!(out, " {GREY}{name}={RESET}{}", colourize(value, true))?;
            } else {
                write!(out, " {name}={}", colourize(value, false))?;
            }
        }
        writeln!(out)
    }
}

/// What logged an event, from its target: the crate, with the server's core
/// (`bedrockrs_core`, and the `bedrockrs` binary) shown as `bedrockrs`.
fn source(target: &str) -> &str {
    match target.split("::").next().unwrap_or(target) {
        "bedrockrs_core" => "bedrockrs",
        name => name,
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERR",
        Level::WARN => "WRN",
        Level::INFO => "INF",
        Level::DEBUG => "DBG",
        Level::TRACE => "TRC",
    }
}

/// An event's message and other fields, as text.
#[derive(Default)]
struct Fields {
    message: String,
    others: Vec<(&'static str, String)>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

impl Fields {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.others.push((field.name(), value));
        }
    }
}

fn level_colour(level: Level) -> &'static str {
    match level {
        Level::ERROR => "\x1b[31m",
        Level::WARN => "\x1b[33m",
        Level::INFO => "\x1b[32m",
        Level::DEBUG => "\x1b[34m",
        Level::TRACE => "\x1b[35m",
    }
}

/// Turns Bedrock's `§` formatting codes into ANSI escape codes, resetting at
/// the end if any were used, or strips them when `ansi` is off. Unknown codes
/// are removed; a `§` at the very end is kept.
pub fn colourize(text: &str, ansi: bool) -> String {
    if !text.contains('§') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut styled = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '§' {
            out.push(c);
            continue;
        }
        let Some(code) = chars.next() else {
            out.push(c);
            break;
        };
        if !ansi {
            continue;
        }
        match code.to_ascii_lowercase() {
            'r' => {
                out.push_str(RESET);
                styled = false;
            }
            'l' => {
                out.push_str("\x1b[1m");
                styled = true;
            }
            'o' => {
                out.push_str("\x1b[3m");
                styled = true;
            }
            // Obfuscated text has no terminal equivalent.
            'k' => {}
            code => {
                if let Some((r, g, b)) = colour(code) {
                    // A colour code clears bold and italic, as in game.
                    let _ = write!(out, "{RESET}\x1b[38;2;{r};{g};{b}m");
                    styled = true;
                }
            }
        }
    }
    if styled {
        out.push_str(RESET);
    }
    out
}

/// Bedrock's text colours, including the material colours only Bedrock has.
fn colour(code: char) -> Option<(u8, u8, u8)> {
    Some(match code {
        '0' => (0x00, 0x00, 0x00),
        '1' => (0x00, 0x00, 0xAA),
        '2' => (0x00, 0xAA, 0x00),
        '3' => (0x00, 0xAA, 0xAA),
        '4' => (0xAA, 0x00, 0x00),
        '5' => (0xAA, 0x00, 0xAA),
        '6' => (0xFF, 0xAA, 0x00),
        '7' => (0xAA, 0xAA, 0xAA),
        '8' => (0x55, 0x55, 0x55),
        '9' => (0x55, 0x55, 0xFF),
        'a' => (0x55, 0xFF, 0x55),
        'b' => (0x55, 0xFF, 0xFF),
        'c' => (0xFF, 0x55, 0x55),
        'd' => (0xFF, 0x55, 0xFF),
        'e' => (0xFF, 0xFF, 0x55),
        'f' => (0xFF, 0xFF, 0xFF),
        'g' => (0xDD, 0xD6, 0x05),
        'h' => (0xE3, 0xD4, 0xD1),
        'i' => (0xCE, 0xCA, 0xCA),
        'j' => (0x44, 0x3A, 0x3B),
        'm' => (0x97, 0x16, 0x07),
        'n' => (0xB4, 0x68, 0x4D),
        'p' => (0xDE, 0xB1, 0x2D),
        'q' => (0x47, 0xA0, 0x36),
        's' => (0x2C, 0xBA, 0xA8),
        't' => (0x21, 0x49, 0x7B),
        'u' => (0x9A, 0x5C, 0xC6),
        'v' => (0xEB, 0x71, 0x14),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_codes_become_ansi_and_reset_at_the_end() {
        assert_eq!(
            colourize("§eWelcome, §lSteve", true),
            "\x1b[0m\x1b[38;2;255;255;85mWelcome, \x1b[1mSteve\x1b[0m"
        );
        assert_eq!(
            colourize("§7grey§r plain", true),
            "\x1b[0m\x1b[38;2;170;170;170mgrey\x1b[0m plain"
        );
        assert_eq!(colourize("no codes", true), "no codes");
    }

    #[test]
    fn codes_are_stripped_without_colours() {
        assert_eq!(
            colourize("§cRed §kmagic§r and §zunknown", false),
            "Red magic and unknown"
        );
        assert_eq!(colourize("trailing §", false), "trailing §");
    }

    fn line(
        ansi: bool,
        level: Level,
        target: &str,
        message: &str,
        others: &[(&'static str, &str)],
    ) -> String {
        use chrono::TimeZone as _;
        let time = Local.with_ymd_and_hms(2026, 9, 26, 14, 30, 5).unwrap()
            + chrono::Duration::milliseconds(123);
        let fields = Fields {
            message: message.into(),
            others: others
                .iter()
                .map(|(name, value)| (*name, (*value).to_owned()))
                .collect(),
        };
        let mut out = String::new();
        ConsoleFormat { ansi }
            .write_line(&mut out, time, level, target, fields)
            .unwrap();
        out
    }

    #[test]
    fn lines_follow_the_console_format() {
        assert_eq!(
            line(
                false,
                Level::INFO,
                PLUGIN_TARGET,
                "Hello from Luau!",
                &[(PLUGIN_FIELD, "hello")]
            ),
            "<26/09/26 14:30:05.123> INF [hello] Hello from Luau!\n"
        );
        assert_eq!(
            line(
                false,
                Level::INFO,
                "bedrockrs_core::session",
                "player logged in",
                &[]
            ),
            "<26/09/26 14:30:05.123> INF [bedrockrs] player logged in\n"
        );
        assert_eq!(
            line(
                false,
                Level::WARN,
                "bedrockrs_net::listener",
                "§cslow",
                &[("peers", "2")]
            ),
            "<26/09/26 14:30:05.123> WRN [bedrockrs_net] slow peers=2\n"
        );
        assert_eq!(
            line(false, Level::ERROR, "chat", "[broadcast] hi", &[]),
            "<26/09/26 14:30:05.123> ERR [chat] [broadcast] hi\n"
        );
    }

    #[test]
    fn each_part_has_its_colour() {
        assert_eq!(
            line(
                true,
                Level::INFO,
                "bedrockrs",
                "§eWelcome",
                &[("players", "2")]
            ),
            "\x1b[90m<26/09/26 14:30:05.123>\x1b[0m \x1b[32mINF\x1b[0m \x1b[36m[bedrockrs]\x1b[0m \
             \x1b[0m\x1b[38;2;255;255;85mWelcome\x1b[0m \x1b[90mplayers=\x1b[0m2\n"
        );
    }
}
