//! The join path from the plugin event to the chat: the sample plugin must
//! welcome a joining player exactly once, and greet them privately.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bedrockrs_core::auth::Authenticator;
use bedrockrs_core::players::{EYE_HEIGHT, Joining, Movement, Profile, View};
use bedrockrs_core::server::{self, PLUGIN_ACTION_QUEUE, Server};
use bedrockrs_core::world::World;
use bedrockrs_plugins::{Event, Player, PluginConfig, PluginHost};
use bedrockrs_protocol::packet::{self, id};
use bedrockrs_protocol::packets::Text;
use bedrockrs_protocol::types::{ChunkPos, Vec3};
use tokio::sync::mpsc;

#[tokio::test(flavor = "multi_thread")]
async fn the_sample_plugin_welcomes_a_joining_player_once() {
    let directory = std::env::temp_dir().join(format!("bedrockrs-welcome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let hello = directory.join("hello");
    std::fs::create_dir_all(&hello).unwrap();
    for file in ["plugin.json", "main.luau"] {
        std::fs::copy(format!("../../server/plugins/hello/{file}"), hello.join(file)).unwrap();
    }

    let (actions, plugin_actions) = mpsc::channel(PLUGIN_ACTION_QUEUE);
    let config = PluginConfig {
        directory: directory.clone(),
        ..PluginConfig::default()
    };
    let plugins = PluginHost::start(config, actions).unwrap();
    let server = Arc::new(Server::new(
        World::new(),
        plugins.dispatcher(),
        Authenticator::offline(),
    ));
    tokio::spawn(server::apply_plugin_actions(
        Arc::clone(&server),
        plugin_actions,
    ));

    let (outbound, mut queue) = mpsc::channel(64);
    let uuid = uuid::Uuid::new_v4();
    let _membership = server.players.join(Joining {
        entity_id: server.players.allocate_entity_id(),
        profile: Profile {
            name: "Steve".into(),
            uuid,
            skin: bedrockrs_core::players::placeholder_skin(uuid),
        },
        movement: Movement {
            position: Vec3 {
                x: 0.5,
                y: -60.0 + EYE_HEIGHT,
                z: 0.5,
            },
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            on_ground: true,
        },
        view: View {
            centre: ChunkPos::new(0, 0),
            radius: 4,
        },
        inventory: Default::default(),
        held: (bedrockrs_protocol::packets::ItemInstance::EMPTY, 0),
        outbound,
    });
    server.plugins.dispatch(Event::PlayerJoin(Player {
        name: "Steve".into(),
        uuid: uuid.to_string(),
    }));

    // Collect everything the player is sent for a while: a second welcome
    // would arrive well within this window.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut texts = Vec::new();
    while Instant::now() < deadline {
        while let Ok(packet) = queue.try_recv() {
            let (header, payload) = packet::read_header(&packet).unwrap();
            if header.id == id::TEXT {
                texts.push(packet::decode::<Text>(payload).unwrap().message);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(plugins);
    let _ = std::fs::remove_dir_all(&directory);
    assert_eq!(
        texts,
        [
            "§eWelcome to BedrockRS, Steve!",
            "§7Only you can see this. Try §f!kickme§7 or §f!wave§7."
        ]
    );
}
