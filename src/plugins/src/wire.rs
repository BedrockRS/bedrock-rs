//! What crosses into a plugin, as JSON, whatever the engine: events and
//! command calls. Each engine turns these into its own values (Luau tables,
//! JavaScript objects), so a plugin sees the same fields in both languages. A
//! JSON object with a `name` and a `uuid` string becomes a `Player`, with its
//! methods.

use serde_json::{Value, json};

use crate::{ArgValue, CommandCall, CommandSender, Event, Player};

pub fn player(player: &Player) -> Value {
    json!({ "name": player.name, "uuid": player.uuid })
}

/// An event: `{ event, cancellable, data }`. The data handlers get:
/// - `player_join`, `player_quit`, `player_respawn`: `{ player }`;
/// - `player_chat`: `{ player, message }`;
/// - `block_break`, `block_place`: `{ player, position: { x, y, z }, block }`;
/// - `player_damage`: `{ player, cause, amount, health }`;
/// - `player_death`: `{ player, cause, message }`.
///
/// Cancellable events also get `cancel()` and `isCancelled()`.
pub fn event(event: &Event) -> Value {
    let data = match event {
        Event::PlayerJoin(who) | Event::PlayerQuit(who) | Event::PlayerRespawn(who) => {
            json!({ "player": player(who) })
        }
        Event::PlayerChat {
            player: who,
            message,
        } => json!({ "player": player(who), "message": message }),
        Event::BlockBreak(change) | Event::BlockPlace(change) => json!({
            "player": player(&change.player),
            "position": {
                "x": change.position.x,
                "y": change.position.y,
                "z": change.position.z,
            },
            "block": change.block,
        }),
        Event::PlayerDamage(damage) => json!({
            "player": player(&damage.player),
            "cause": damage.cause,
            "amount": damage.amount,
            "health": damage.health,
        }),
        Event::PlayerDeath {
            player: who,
            cause,
            message,
        } => json!({ "player": player(who), "cause": cause, "message": message }),
    };
    json!({
        "event": event.name(),
        "cancellable": event.is_cancellable(),
        "data": data,
    })
}

/// A command someone ran: `{ command, path, args, sender }`, where `args`
/// holds each argument by name and `sender` is the player, or `null` for
/// the console.
pub fn command_call(call: &CommandCall) -> Value {
    let args: serde_json::Map<String, Value> = call
        .args
        .iter()
        .map(|(name, value)| {
            let value = match value {
                ArgValue::String(text) => json!(text),
                ArgValue::Int(int) => json!(int),
                ArgValue::Number(number) => json!(number),
                ArgValue::Bool(boolean) => json!(boolean),
                ArgValue::Player(who) => player(who),
            };
            (name.clone(), value)
        })
        .collect();
    let sender = match &call.sender {
        CommandSender::Console => Value::Null,
        CommandSender::Player(who) => player(who),
    };
    json!({
        "command": call.command,
        "path": call.path,
        "args": args,
        "sender": sender,
    })
}

/// Whether a JSON value is a player: an object with a `name` and a `uuid`.
pub fn is_player(value: &Value) -> bool {
    value.get("uuid").is_some_and(Value::is_string)
        && value.get("name").is_some_and(Value::is_string)
        && value.as_object().is_some_and(|object| object.len() == 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockChange, Position};

    fn steve() -> Player {
        Player {
            name: "Steve".into(),
            uuid: "174319cc-f69f-30d8-a279-6ace57f2011e".into(),
        }
    }

    #[test]
    fn events_carry_their_name_and_whether_they_cancel() {
        let chat = event(&Event::PlayerChat {
            player: steve(),
            message: "hi".into(),
        });
        assert_eq!(chat["event"], "player_chat");
        assert_eq!(chat["cancellable"], true);
        assert_eq!(chat["data"]["message"], "hi");
        assert!(is_player(&chat["data"]["player"]));

        let placed = event(&Event::BlockPlace(BlockChange {
            player: steve(),
            position: Position { x: 1, y: -60, z: 3 },
            block: "minecraft:stone".into(),
        }));
        assert_eq!(placed["cancellable"], false);
        assert_eq!(placed["data"]["position"]["y"], -60);
    }

    #[test]
    fn calls_carry_their_arguments_by_name() {
        let call = CommandCall {
            plugin: "warps".into(),
            command: "warp".into(),
            path: vec!["set".into()],
            args: vec![
                ("name".into(), ArgValue::String("home".into())),
                ("count".into(), ArgValue::Int(3)),
                ("who".into(), ArgValue::Player(steve())),
            ],
            sender: CommandSender::Console,
        };
        let json = command_call(&call);
        assert_eq!(json["path"][0], "set");
        assert_eq!(json["args"]["count"], 3);
        assert!(is_player(&json["args"]["who"]));
        assert!(json["sender"].is_null());
    }
}
