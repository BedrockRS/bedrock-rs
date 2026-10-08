//! What a plugin's API calls act on, and the calls themselves, shared by
//! every engine. The Luau and JavaScript engines' functions only translate
//! arguments and results: the checks, the rules of the lifecycle and the
//! error messages are all here, so a plugin behaves the same in either
//! language.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::Level;

use crate::scheduler::{MAX_DELAY, Tasks};
use crate::{
    Action, CommandSender, CommandSpec, DAMAGE_CAUSES, Event, GAME_MODE_VALUES, Output, Player,
};

/// Why the API refuses calls at the top level of a script, in either
/// language: the top level sets the plugin up, and everything it does with
/// the server starts in `on_load`, so loading always happens in one place.
pub const TOP_LEVEL: &str = "the plugin API is not available at the top level of a plugin; \
     use it from on_load, on_enable or on_disable";

/// What a kicked player is shown when the plugin gives no reason.
pub const DEFAULT_KICK_REASON: &str = "You were kicked from the server.";

/// Where a plugin is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The script's top level runs.
    TopLevel,
    /// `on_load` runs: the only time commands can be registered.
    Loading,
    /// Loaded, waiting to be enabled.
    Loaded,
    /// `on_enable` runs, or has run: events, commands and tasks reach it.
    Enabled,
    /// `on_disable` runs.
    Disabling,
    /// Disabled: nothing reaches it any more.
    Disabled,
}

/// What every plugin shares: the server's queue of actions, where output
/// goes, the players online and the current tick.
#[derive(Clone)]
pub(crate) struct Shared {
    pub actions: mpsc::Sender<Action>,
    pub output: Output,
    /// The players plugins know are online, by UUID.
    pub roster: Arc<Mutex<BTreeMap<String, Player>>>,
    /// The server's current tick.
    pub clock: Arc<AtomicU64>,
}

impl Shared {
    pub fn new(actions: mpsc::Sender<Action>, output: Output) -> Self {
        Self {
            actions,
            output,
            roster: Arc::default(),
            clock: Arc::default(),
        }
    }

    pub fn now(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }

    pub fn roster(&self) -> MutexGuard<'_, BTreeMap<String, Player>> {
        self.roster.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One plugin's state, shared between the plugin manager and the plugin's
/// API functions.
pub(crate) type State = Arc<Mutex<PluginState>>;

pub(crate) fn lock(state: &State) -> MutexGuard<'_, PluginState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct PluginState {
    pub name: String,
    pub phase: Phase,
    shared: Shared,
    /// The commands it registered in `on_load`.
    pub commands: Vec<CommandSpec>,
    /// The events it listens for.
    pub events: BTreeSet<String>,
    pub tasks: Tasks,
    /// What it printed while loading, held until the server has said it
    /// loaded; `None` once released.
    held: Option<Vec<(Level, String)>>,
}

type ApiResult<T = ()> = Result<T, String>;

impl PluginState {
    pub fn new(name: &str, shared: Shared) -> State {
        Arc::new(Mutex::new(Self {
            name: name.to_owned(),
            phase: Phase::TopLevel,
            shared,
            commands: Vec::new(),
            events: BTreeSet::new(),
            tasks: Tasks::default(),
            held: Some(Vec::new()),
        }))
    }

    /// Lets the plugin's output through, and hands back what it held.
    pub fn release_output(&mut self) -> Vec<(Level, String)> {
        self.held.take().unwrap_or_default()
    }

    pub fn output(&self) -> &Output {
        &self.shared.output
    }

    #[cfg(test)]
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Fails at the top level, and once the plugin is disabled.
    fn usable(&self) -> ApiResult {
        match self.phase {
            Phase::TopLevel => Err(TOP_LEVEL.into()),
            Phase::Disabled => Err("the plugin is disabled".into()),
            _ => Ok(()),
        }
    }

    fn request(&self, action: Action) -> ApiResult {
        match self.shared.actions.try_send(action) {
            // A closed channel means the server is shutting down.
            Ok(()) | Err(TrySendError::Closed(_)) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err("the server is not keeping up with plugin actions".into())
            }
        }
    }

    // ---- Logger ---------------------------------------------------------

    pub fn log(&mut self, level: Level, message: &str) -> ApiResult {
        self.usable()?;
        match self.held.as_mut() {
            Some(lines) => lines.push((level, message.to_owned())),
            None => (self.shared.output)(&self.name, level, message),
        }
        Ok(())
    }

    /// Logs a failure inside the plugin (a handler, a task) as the server's
    /// own error, naming the plugin.
    pub fn report_failure(&self, what: &str, error: &str) {
        tracing::error!("Plugin {}'s {what} failed: {error}", self.name);
    }

    // ---- Server ---------------------------------------------------------

    pub fn broadcast(&self, message: &str) -> ApiResult {
        self.usable()?;
        if message.is_empty() {
            return Err("cannot broadcast an empty message".into());
        }
        self.request(Action::Broadcast(message.to_owned()))
    }

    /// The online player with this UUID, in any case.
    pub fn player(&self, uuid: &str) -> ApiResult<Option<Player>> {
        self.usable()?;
        Ok(self
            .shared
            .roster()
            .get(&uuid.to_ascii_lowercase())
            .cloned())
    }

    /// Everyone online, by name.
    pub fn players(&self) -> ApiResult<Vec<Player>> {
        self.usable()?;
        let mut players: Vec<Player> = self.shared.roster().values().cloned().collect();
        players.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(players)
    }

    /// Listens for `event`, from `on_load` or later.
    pub fn subscribe(&mut self, event: &str) -> ApiResult {
        self.usable()?;
        if !Event::NAMES.contains(&event) {
            return Err(format!(
                "unknown event {event:?}; expected one of: {}",
                Event::NAMES.join(", ")
            ));
        }
        self.events.insert(event.to_owned());
        Ok(())
    }

    /// Adds a command: only in `on_load`, so the server knows every command
    /// before the plugin is enabled.
    pub fn register_command(&mut self, spec: CommandSpec) -> ApiResult {
        self.usable()?;
        if self.phase != Phase::Loading {
            return Err("commands can only be registered in on_load".into());
        }
        if self.commands.iter().any(|other| other.name == spec.name) {
            return Err(format!("this plugin already registered /{}", spec.name));
        }
        self.commands.push(spec);
        Ok(())
    }

    /// What a command handler replied after it returned: a chat message to
    /// a player, a log line for the console.
    pub fn late_reply(&self, sender: &CommandSender, text: &str, success: bool) -> ApiResult {
        self.usable()?;
        match sender {
            CommandSender::Player(player) => {
                let message = if success {
                    text.to_owned()
                } else {
                    format!("§c{text}")
                };
                self.request(Action::SendMessage {
                    player: player.uuid.clone(),
                    message,
                })
            }
            CommandSender::Console if success => {
                tracing::info!(target: "console", "{text}");
                Ok(())
            }
            CommandSender::Console => {
                tracing::warn!(target: "console", "§c{text}");
                Ok(())
            }
        }
    }

    // ---- Player ---------------------------------------------------------

    pub fn send_message(&self, player: &str, message: &str) -> ApiResult {
        self.usable()?;
        if message.is_empty() {
            return Err("cannot send an empty message".into());
        }
        self.request(Action::SendMessage {
            player: player.to_owned(),
            message: message.to_owned(),
        })
    }

    pub fn kick(&self, player: &str, reason: Option<&str>) -> ApiResult {
        self.usable()?;
        let reason = reason
            .filter(|reason| !reason.is_empty())
            .unwrap_or(DEFAULT_KICK_REASON);
        self.request(Action::Kick {
            player: player.to_owned(),
            reason: reason.to_owned(),
        })
    }

    pub fn set_game_mode(&self, player: &str, mode: &str) -> ApiResult {
        self.usable()?;
        let mode = mode.to_ascii_lowercase();
        if !GAME_MODE_VALUES.contains(&mode.as_str()) {
            return Err(format!(
                "unknown game mode {mode:?}; expected survival, creative, adventure or spectator"
            ));
        }
        self.request(Action::SetGameMode {
            player: player.to_owned(),
            mode,
        })
    }

    pub fn set_health(&self, player: &str, health: f64) -> ApiResult {
        self.usable()?;
        if !health.is_finite() {
            return Err("setHealth expects a number".into());
        }
        self.request(Action::SetHealth {
            player: player.to_owned(),
            health: health as f32,
        })
    }

    pub fn damage(&self, player: &str, amount: f64, cause: Option<&str>) -> ApiResult {
        self.usable()?;
        if !(amount.is_finite() && amount > 0.0) {
            return Err("damage expects an amount above 0".into());
        }
        let cause = cause.unwrap_or("none");
        if !DAMAGE_CAUSES.contains(&cause) {
            return Err(format!(
                "unknown damage cause {cause:?}; expected a vanilla cause such as fall, void or entityAttack"
            ));
        }
        self.request(Action::Damage {
            player: player.to_owned(),
            amount: amount as f32,
            cause: cause.to_owned(),
        })
    }

    // ---- World (the scheduler) --------------------------------------------

    /// Schedules a task `ticks` ticks from now (the next tick for 0), then
    /// every `every` ticks if it repeats. Returns its id. Tasks run once the
    /// plugin is enabled.
    pub fn schedule(&mut self, ticks: f64, every: Option<f64>) -> ApiResult<u32> {
        self.usable()?;
        if matches!(self.phase, Phase::Disabling) {
            return Err("a plugin cannot schedule tasks while it is being disabled".into());
        }
        let delay = whole_ticks(ticks)?;
        let every = every.map(whole_ticks).transpose()?;
        if every == Some(0) {
            return Err("an interval must be at least 1 tick".into());
        }
        Ok(self.tasks.schedule(self.shared.now(), delay, every))
    }

    /// Stops a task. Unknown ids are ignored, as in `@minecraft/server`.
    pub fn clear_run(&mut self, id: f64) -> ApiResult {
        self.usable()?;
        if id.fract() == 0.0 && (1.0..=f64::from(u32::MAX)).contains(&id) {
            self.tasks.clear(id as u32);
        }
        Ok(())
    }
}

/// A number of ticks: a whole number from 0 to [`MAX_DELAY`].
fn whole_ticks(ticks: f64) -> ApiResult<u64> {
    if ticks.fract() != 0.0 || !(0.0..=MAX_DELAY as f64).contains(&ticks) {
        return Err(format!(
            "ticks must be a whole number from 0 to {MAX_DELAY}, not {ticks}"
        ));
    }
    Ok(ticks as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandNode, CommandSpec};

    fn state() -> (State, mpsc::Receiver<Action>) {
        let (actions, received) = mpsc::channel(8);
        let shared = Shared::new(actions, Arc::new(|_, _, _| {}));
        (PluginState::new("test", shared), received)
    }

    fn command(name: &str) -> CommandSpec {
        CommandSpec {
            name: name.into(),
            description: String::new(),
            aliases: Vec::new(),
            root: CommandNode::runs(),
        }
    }

    #[test]
    fn the_api_waits_for_on_load() {
        let (state, _) = state();
        let mut state = lock(&state);
        assert_eq!(state.broadcast("hi"), Err(TOP_LEVEL.into()));
        assert_eq!(state.log(Level::INFO, "hi"), Err(TOP_LEVEL.into()));
        state.phase = Phase::Loading;
        assert!(state.broadcast("hi").is_ok());
    }

    #[test]
    fn commands_are_registered_in_on_load_only() {
        let (state, _) = state();
        let mut state = lock(&state);
        state.phase = Phase::Loading;
        assert!(state.register_command(command("warp")).is_ok());
        assert!(
            state
                .register_command(command("warp"))
                .unwrap_err()
                .contains("already registered")
        );
        state.phase = Phase::Enabled;
        assert_eq!(
            state.register_command(command("home")),
            Err("commands can only be registered in on_load".into())
        );
    }

    #[test]
    fn players_are_checked_before_the_server_hears() {
        let (state, mut actions) = state();
        let mut state = lock(&state);
        state.phase = Phase::Enabled;
        assert!(state.set_game_mode("u", "hardcore").is_err());
        assert!(state.damage("u", -1.0, None).is_err());
        assert!(state.damage("u", 2.0, Some("sneeze")).is_err());
        assert!(state.send_message("u", "").is_err());
        state.kick("u", None).unwrap();
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::Kick {
                player: "u".into(),
                reason: DEFAULT_KICK_REASON.into()
            }
        );
        state.set_game_mode("u", "Creative").unwrap();
        assert_eq!(
            actions.try_recv().unwrap(),
            Action::SetGameMode {
                player: "u".into(),
                mode: "creative".into()
            }
        );
    }

    #[test]
    fn tasks_take_whole_ticks_and_not_while_disabling() {
        let (state, _) = state();
        let mut state = lock(&state);
        state.phase = Phase::Enabled;
        assert!(state.schedule(1.5, None).is_err());
        assert!(state.schedule(-1.0, None).is_err());
        assert!(state.schedule(20.0, Some(0.0)).is_err());
        let id = state.schedule(20.0, Some(20.0)).unwrap();
        state.clear_run(f64::from(id)).unwrap();
        assert!(state.tasks.is_empty());
        state.phase = Phase::Disabling;
        assert!(state.schedule(1.0, None).is_err());
    }

    #[test]
    fn output_is_held_until_released() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let (actions, _) = mpsc::channel(1);
        let shared = Shared::new(
            actions,
            Arc::new(move |_, _, message: &str| sink.lock().unwrap().push(message.to_owned())),
        );
        let state = PluginState::new("held", shared);
        let mut state = lock(&state);
        state.phase = Phase::Loading;
        state.log(Level::INFO, "early").unwrap();
        assert!(lines.lock().unwrap().is_empty());
        assert_eq!(state.release_output(), [(Level::INFO, "early".to_owned())]);
        state.log(Level::INFO, "late").unwrap();
        assert_eq!(*lines.lock().unwrap(), ["late"]);
    }
}
