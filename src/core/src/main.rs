//! `bedrockrs`: the BedrockRS server binary.
//!
//! NetherNet settings can be overridden with environment variables:
//! - `BEDROCKRS_SIGNALING_ADDR`: TCP address for HTTP signaling (default `0.0.0.0:19132`)
//! - `BEDROCKRS_MEDIA_PORT`: UDP port for WebRTC traffic (default `19133`)
//! - `BEDROCKRS_MEDIA_IPS`: comma-separated local addresses for WebRTC traffic
//!   (default: every IPv4 interface that is up)
//! - `BEDROCKRS_ADVERTISE_IPS`: comma-separated public addresses to offer clients
//! - `BEDROCKRS_ICE_LITE`: `false` switches from ICE-lite to full ICE (default `true`)
//!
//! `BEDROCKRS_WORLD_DIR` sets where the world is saved (default `worlds/world`), and
//! `BEDROCKRS_AUTHENTICATION=false` turns off checking players' sign-in (offline
//! testing only: anyone can then join as anyone).
//!
//! What the console shows is set in `bedrockrs.toml` (see [`config`]), created
//! with defaults on first run. `RUST_LOG`, when set, overrides it; chat is
//! logged under the `chat` target. `NO_COLOR` turns colours off.
//!
//! Commands typed into the console run with every permission, with or without
//! their `/`: `op <player>` makes the first operator, and `stop` stops the
//! server, as Ctrl+C does. Operators are kept in `ops.json`.
//!
//! [`config`]: bedrockrs_core::config

use std::io::{BufRead as _, IsTerminal as _};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context as _;
use bedrockrs_core::auth::Authenticator;
use bedrockrs_core::commands::Sender;
use bedrockrs_core::config::{CONFIG_FILE, Config};
use bedrockrs_core::console::ConsoleFormat;
use bedrockrs_core::ops::{OPS_FILE, Operators};
use bedrockrs_core::server::{self, PLUGIN_ACTION_QUEUE, Server};
use bedrockrs_core::session;
use bedrockrs_core::tick::TickLoop;
use bedrockrs_core::world::World;
use bedrockrs_net::{Connection, Listener, ListenerConfig, ServerStatus};
use bedrockrs_plugins::{PluginConfig, PluginHost};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let loaded = Config::load_or_create(Path::new(CONFIG_FILE))?;
    // `RUST_LOG`, when set, replaces the filter the configuration asks for.
    let filter = match std::env::var("RUST_LOG") {
        Ok(directives) if !directives.trim().is_empty() => {
            EnvFilter::try_new(&directives).context("invalid RUST_LOG")?
        }
        _ => EnvFilter::new(loaded.config.logs.filter()),
    };
    // Colours only for a terminal, and never with NO_COLOR set (no-color.org).
    let ansi = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .event_format(ConsoleFormat { ansi })
        .init();
    if loaded.created {
        tracing::info!("created {CONFIG_FILE} with the default settings");
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        game = bedrockrs_protocol::GAME_VERSION,
        protocol = bedrockrs_protocol::PROTOCOL_VERSION,
        tps = bedrockrs_core::TICKS_PER_SECOND,
        "BedrockRS"
    );

    let (actions, plugin_actions) = mpsc::channel(PLUGIN_ACTION_QUEUE);
    let plugins = PluginHost::start(PluginConfig::default(), actions)
        .context("failed to start the plugin host")?;
    tracing::info!(loaded = ?plugins.loaded(), "plugins ready");
    let world_directory = env_value::<PathBuf>("BEDROCKRS_WORLD_DIR")?
        .unwrap_or_else(|| PathBuf::from("worlds/world"));
    let world = World::open(&world_directory)
        .with_context(|| format!("failed to open the world in {}", world_directory.display()))?;
    tracing::info!(
        directory = %world_directory.display(),
        saved_chunks = world.saved_chunks(),
        "world loaded"
    );
    let authenticator = if env_value::<bool>("BEDROCKRS_AUTHENTICATION")?.unwrap_or(true) {
        Authenticator::online().context("failed to set up player authentication")?
    } else {
        tracing::warn!(
            "player authentication is OFF: anyone can join as anyone. Only use this for offline testing"
        );
        Authenticator::offline()
    };
    let ops = Operators::open(Path::new(OPS_FILE))?;
    let server = Arc::new(
        Server::new(world, plugins.dispatcher(), authenticator)
            .with_operators(ops)
            .with_default_game_mode(loaded.config.players.default_game_mode),
    );
    tokio::spawn(server::apply_plugin_actions(
        Arc::clone(&server),
        plugin_actions,
    ));
    // Stops when dropped, as `main` returns.
    let _game_loop =
        TickLoop::start(Arc::clone(&server)).context("failed to start the game loop")?;

    let status = ServerStatus {
        name: "BedrockRS".into(),
        protocol: bedrockrs_protocol::PROTOCOL_VERSION,
        version: bedrockrs_protocol::GAME_VERSION.into(),
        level: "Bedrock level".into(),
        players: 0,
        max_players: 20,
        game_type: 0,
    };
    let mut listener = Listener::bind(listener_config()?, status)
        .await
        .context("failed to start the NetherNet listener")?;
    tracing::info!(
        signaling = %listener.signaling_addr(),
        media = ?listener.media_addrs(),
        identity = listener.key_fingerprint(),
        "NetherNet listening"
    );
    let mut console = console_lines();
    tracing::info!("type help in the console for a list of commands");

    loop {
        tokio::select! {
            connection = listener.accept() => match connection {
                Some(connection) => {
                    tokio::spawn(serve(connection, Arc::clone(&server)));
                }
                None => break,
            },
            Some(line) = console.recv() => {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    for line in server.run_command(&Sender::Console, &line).await.lines {
                        if line.success {
                            tracing::info!("{}", line.text);
                        } else {
                            tracing::warn!("{}", line.text);
                        }
                    }
                });
            }
            () = server.stop_requested() => {
                tracing::info!("shutting down");
                break;
            }
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for Ctrl+C")?;
                tracing::info!("shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// Lines typed into the console, read on a thread of their own. Stops when
/// the console closes, as it does when the server runs without one.
fn console_lines() -> mpsc::Receiver<String> {
    let (lines, received) = mpsc::channel(16);
    let reader = std::thread::Builder::new()
        .name("console".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if !line.trim().is_empty() && lines.blocking_send(line).is_err() {
                    break;
                }
            }
        });
    if let Err(err) = reader {
        tracing::warn!(%err, "failed to read the console; console commands are off");
    }
    received
}

/// Runs a client's protocol session until it disconnects.
async fn serve(connection: Connection, server: Arc<Server>) {
    let network_id = connection.network_id();
    tracing::debug!(
        network_id,
        issuer = ?connection.client_identity().map(|identity| &identity.issuer),
        "client connected"
    );
    session::run(connection, server).await;
    tracing::debug!(network_id, "client disconnected");
}

fn listener_config() -> anyhow::Result<ListenerConfig> {
    let mut config = ListenerConfig::default();
    if let Some(addr) = env_value("BEDROCKRS_SIGNALING_ADDR")? {
        config.signaling_addr = addr;
    }
    if let Some(port) = env_value("BEDROCKRS_MEDIA_PORT")? {
        config.media_port = port;
    }
    if let Some(ips) = env_addresses("BEDROCKRS_MEDIA_IPS")? {
        config.media_ips = ips;
    }
    if let Some(ips) = env_addresses("BEDROCKRS_ADVERTISE_IPS")? {
        config.advertise_ips = ips;
    }
    if let Some(ice_lite) = env_value("BEDROCKRS_ICE_LITE")? {
        config.ice_lite = ice_lite;
    }
    Ok(config)
}

fn env_value<T>(name: &str) -> anyhow::Result<Option<T>>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Ok(value) => value
            .trim()
            .parse()
            .map(Some)
            .with_context(|| format!("invalid {name}: {value:?}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("invalid {name}")),
    }
}

fn env_addresses(name: &str) -> anyhow::Result<Option<Vec<IpAddr>>> {
    let Some(value) = env_value::<String>(name)? else {
        return Ok(None);
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .map(|ip| {
            ip.parse()
                .with_context(|| format!("invalid address {ip:?} in {name}"))
        })
        .collect::<anyhow::Result<_>>()
        .map(Some)
}
