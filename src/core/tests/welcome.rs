//! The sample plugin (examples/plugins/hello-luau), end to end: it must leave vanilla's join message alone
//! and greet a joining player privately exactly once, and answer its slash
//! command, subcommands included, through the plugin host.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bedrockrs_core::auth::Authenticator;
use bedrockrs_core::commands::Sender;
use bedrockrs_core::game_mode::GameMode;
use bedrockrs_core::players::{EYE_HEIGHT, Joining, Movement, Profile, View};
use bedrockrs_core::server::{self, PLUGIN_ACTION_QUEUE, Server};
use bedrockrs_core::world::World;
use bedrockrs_plugins::{Event, Player, PluginConfig, PluginHost};
use bedrockrs_protocol::packet::{self, id};
use bedrockrs_protocol::packets::Text;
use bedrockrs_protocol::types::{ChunkPos, Vec3};
use tokio::sync::mpsc;

/// A server running the sample plugin from a copy in a directory of its own.
fn sample_plugin_server(test: &str) -> (Arc<Server>, PluginHost, PathBuf) {
    let directory = std::env::temp_dir().join(format!("bedrockrs-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let hello = directory.join("hello");
    std::fs::create_dir_all(&hello).unwrap();
    for file in ["plugin.json", "main.luau"] {
        std::fs::copy(
            format!("../../examples/plugins/hello-luau/{file}"),
            hello.join(file),
        )
        .unwrap();
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
    (server, plugins, directory)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sample_plugin_greets_a_joining_player_once() {
    let (server, plugins, directory) = sample_plugin_server("welcome");
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
        armor: [bedrockrs_protocol::packets::ItemInstance::EMPTY; 4],
        game_mode: GameMode::Creative,
        health: 20.0,
        outbound,
    });
    let join = Event::PlayerJoin(Player {
        name: "Steve".into(),
        uuid: uuid.to_string(),
    });
    assert!(
        !server.plugins.dispatch_cancellable(join).await,
        "vanilla's join message stays"
    );

    // Collect everything the player is sent for a while: a second greeting
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
    assert_eq!(texts, ["§7Only you can see this. Try §f/hello§7."]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sample_plugin_answers_its_command() {
    let (server, plugins, directory) = sample_plugin_server("command");
    // The plugin's commands reach the server as an action once it loaded.
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.commands.find("hello").is_none() {
        assert!(
            Instant::now() < deadline,
            "the plugin's command never arrived"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let lines = |reply: bedrockrs_plugins::CommandReply| {
        reply
            .lines
            .into_iter()
            .map(|line| (line.success, line.text))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        lines(server.run_command(&Sender::Console, "/hi").await),
        [(
            true,
            "Hello! Try /hello wave, /hello to <player> or /hello kickme".to_owned()
        )],
        "the alias runs the command"
    );
    assert_eq!(
        lines(server.run_command(&Sender::Console, "hello kickme").await),
        [(false, "Only players can be kicked".to_owned())]
    );
    assert_eq!(
        lines(
            server
                .run_command(&Sender::Console, "hello to Nobody")
                .await
        ),
        [(false, "No player named \"Nobody\" is online".to_owned())],
        "arguments are checked before the plugin hears of the command"
    );
    assert_eq!(
        lines(
            server
                .run_command(
                    &Sender::Console,
                    "hello admin announce Server restarting soon"
                )
                .await
        ),
        [(true, "Announced".to_owned())]
    );
    drop(plugins);
    let _ = std::fs::remove_dir_all(&directory);
}
