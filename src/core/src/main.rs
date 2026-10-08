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
//! `BEDROCKRS_AUTHENTICATION=false` turns off checking players' sign-in (offline
//! testing only: anyone can then join as anyone).
//!
//! Settings are in `server.properties` (see [`config`]), vanilla's file, created
//! with defaults on first run: `level-name` is the world folder in `worlds/`, in
//! vanilla's layout (see [`storage`]), and `log-level` and `log-chat` what the
//! console shows. `RUST_LOG`, when set, overrides those; chat is logged under the
//! `chat` target. `NO_COLOR` turns colours off.
//!
//! Commands typed into the console run with every permission, with or without
//! their `/`: `op <player>` makes the first operator, and `stop` stops the
//! server, as Ctrl+C does. Operators (and any visitors or members set by hand)
//! are kept in `permissions.json`, by PlayFab ID, as vanilla keeps them by
//! XUID (see [`permissions`]). In a terminal, commands are typed at a `> `
//! prompt below the log, with history.
//!
//! [`config`]: bedrockrs_core::config
//! [`permissions`]: bedrockrs_core::permissions
//! [`storage`]: bedrockrs_core::storage

use std::io::{BufRead as _, IsTerminal as _};
use std::net::IpAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bedrockrs_core::auth::Authenticator;
use bedrockrs_core::commands::Sender;
use bedrockrs_core::config::{LevelType, OLD_CONFIG_FILE, PROPERTIES_FILE, ServerProperties};
use bedrockrs_core::console::{ConsoleFormat, ConsoleOutput, Prompt, PromptInput};
use bedrockrs_core::game_rules::GameRules;
use bedrockrs_core::permissions::{OLD_OPS_FILE, PERMISSIONS_FILE, Permissions};
use bedrockrs_core::server::{self, PLUGIN_ACTION_QUEUE, Server};
use bedrockrs_core::session;
use bedrockrs_core::tick::TickLoop;
use bedrockrs_core::world::World;
use bedrockrs_net::{Connection, Listener, ListenerConfig, ServerStatus};
use bedrockrs_plugins::{PluginConfig, PluginHost};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;

/// Longest the server waits, as it stops, for players' sessions to show them
/// they were disconnected and save them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let loaded = ServerProperties::load_or_create(Path::new(PROPERTIES_FILE))?;
    let properties = loaded.properties;
    // `RUST_LOG`, when set, replaces the filter the configuration asks for.
    let filter = match std::env::var("RUST_LOG") {
        Ok(directives) if !directives.trim().is_empty() => {
            EnvFilter::try_new(&directives).context("invalid RUST_LOG")?
        }
        _ => EnvFilter::new(properties.logs.filter()),
    };
    // Colours only for a terminal, and never with NO_COLOR set (no-color.org).
    let ansi = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let output = ConsoleOutput::default();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .event_format(ConsoleFormat { ansi })
        .with_writer(output.clone())
        .init();
    if loaded.created {
        tracing::info!("Created {PROPERTIES_FILE} with the default settings");
    }
    if Path::new(OLD_CONFIG_FILE).exists() {
        tracing::warn!(
            "{OLD_CONFIG_FILE} is no longer read: settings are in {PROPERTIES_FILE} (log-level, log-chat and gamemode)"
        );
    }
    if Path::new(OLD_OPS_FILE).exists() {
        tracing::warn!(
            "{OLD_OPS_FILE} is no longer read: operators are in {PERMISSIONS_FILE}, by PlayFab ID. Op them again with /op"
        );
    }
    if !properties.ignored.is_empty() {
        tracing::debug!(properties = ?properties.ignored, "ignoring properties BedrockRS does not use yet");
    }

    tracing::info!(
        "Starting BedrockRS {} for Minecraft: Bedrock Edition {}",
        env!("CARGO_PKG_VERSION"),
        bedrockrs_protocol::GAME_VERSION
    );
    tracing::debug!(
        protocol = bedrockrs_protocol::PROTOCOL_VERSION,
        tps = bedrockrs_core::TICKS_PER_SECOND,
        "versions"
    );

    let (actions, plugin_actions) = mpsc::channel(PLUGIN_ACTION_QUEUE);
    let plugins = PluginHost::start(PluginConfig::default(), actions)
        .context("failed to start the plugin host")?;
    tracing::debug!(loaded = ?plugins.loaded(), "plugins ready");
    let world_directory = properties.world_directory();
    // Only flat worlds are generated so far; the properties allow no other.
    let LevelType::Flat = properties.level_type;
    let world = World::open(
        &world_directory,
        &properties.level_name,
        properties.default_game_mode,
    )
    .with_context(|| format!("Failed to open the world in {}", world_directory.display()))?;
    tracing::info!("Opened world: {}", world.name());
    tracing::debug!(directory = %world_directory.display(), "world");
    let authenticator = if env_value::<bool>("BEDROCKRS_AUTHENTICATION")?.unwrap_or(true) {
        Authenticator::online().context("Failed to set up player authentication")?
    } else {
        tracing::warn!(
            "Player authentication is OFF: anyone can join as anyone. Only use this for offline testing"
        );
        Authenticator::offline()
    };
    let permissions =
        Permissions::open(Path::new(PERMISSIONS_FILE))?.with_default(properties.default_permission);
    let game_rules = match world.level() {
        Some(level) => GameRules::of_level(Arc::clone(level), &world_directory),
        None => GameRules::in_memory(),
    };
    let server = Arc::new(
        Server::new(world, plugins.dispatcher(), authenticator)
            .with_permissions(permissions)
            .with_default_game_mode(properties.default_game_mode)
            .with_game_rules(game_rules),
    );
    tokio::spawn(server::apply_plugin_actions(
        Arc::clone(&server),
        plugin_actions,
    ));
    // Stops when dropped, as `main` returns.
    let _game_loop =
        TickLoop::start(Arc::clone(&server)).context("Failed to start the game loop")?;

    let status = ServerStatus {
        name: "BedrockRS".into(),
        protocol: bedrockrs_protocol::PROTOCOL_VERSION,
        version: bedrockrs_protocol::GAME_VERSION.into(),
        level: server.world.name().to_owned(),
        players: 0,
        max_players: 20,
        game_type: 0,
    };
    let mut listener = Listener::bind(listener_config()?, status)
        .await
        .context("Failed to start the NetherNet listener")?;
    tracing::info!("Listening on {}", listener.signaling_addr());
    tracing::debug!(
        media = ?listener.media_addrs(),
        identity = listener.key_fingerprint(),
        "NetherNet"
    );
    let mut console = Console::open(&output);
    tracing::info!("Type '/help' in the console for a list of commands");
    let mut sessions = JoinSet::new();

    loop {
        tokio::select! {
            connection = listener.accept() => match connection {
                Some(connection) => {
                    sessions.spawn(serve(connection, Arc::clone(&server)));
                }
                None => break,
            },
            // Sessions that ended are forgotten.
            Some(_) = sessions.join_next() => {}
            // Run here rather than in a task of its own, so what a command
            // prints comes before what it causes, such as stopping.
            input = console.next() => match input {
                PromptInput::Line(line) => {
                    for line in server.run_command(&Sender::Console, &line).await.lines {
                        if line.success {
                            tracing::info!("{}", line.text);
                        } else {
                            // In red, as players see it.
                            tracing::warn!("§c{}", line.text);
                        }
                    }
                }
                PromptInput::Stop => break,
            },
            () = server.stop_requested() => break,
            result = tokio::signal::ctrl_c() => {
                result.context("Failed to listen for Ctrl+C")?;
                break;
            }
        }
    }
    console.close();
    // Players are shown why they were disconnected, rather than timing out,
    // and saved as they leave.
    server.close();
    let ended = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while sessions.join_next().await.is_some() {}
    })
    .await;
    if ended.is_err() {
        tracing::debug!(sessions = sessions.len(), "sessions still open at shutdown");
    }
    // Plugins hear everyone leave, then disable (and save what they keep)
    // before the server says it stopped.
    drop(plugins);
    tracing::info!("Server stopped");
    Ok(())
}

/// Where console commands come from.
enum Console {
    /// The `> ` prompt, when the server runs in a terminal.
    Prompt(Box<Prompt>),
    /// Plain lines from stdin, as when input is piped in.
    Lines(mpsc::Receiver<String>),
    /// No more input.
    Closed,
}

impl Console {
    fn open(output: &ConsoleOutput) -> Self {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            match Prompt::open(output) {
                Ok(prompt) => return Self::Prompt(Box::new(prompt)),
                Err(err) => tracing::warn!("Couldn't show the console prompt: {err}"),
            }
        }
        Self::Lines(console_lines())
    }

    /// The next thing typed. Never returns once input has ended.
    async fn next(&mut self) -> PromptInput {
        let input = match self {
            Self::Prompt(prompt) => prompt.next().await.map_err(|err| {
                tracing::warn!("Couldn't read the console, so console commands are off: {err}");
            }),
            Self::Lines(lines) => lines.recv().await.map(PromptInput::Line).ok_or(()),
            Self::Closed => Err(()),
        };
        match input {
            Ok(input) => input,
            Err(()) => {
                if let Self::Prompt(prompt) = std::mem::replace(self, Self::Closed) {
                    prompt.close();
                }
                std::future::pending().await
            }
        }
    }

    /// Removes the prompt, if there is one, so the terminal is left as it was.
    fn close(self) {
        if let Self::Prompt(prompt) = self {
            prompt.close();
        }
    }
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
        tracing::warn!("Couldn't read the console, so console commands are off: {err}");
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
