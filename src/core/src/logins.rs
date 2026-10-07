//! One session per player: a verified login with a UUID that is already
//! connected kicks the older session, as vanilla does. The rest of the server
//! reaches a player's session through the same registry, to kick them or
//! change their game mode or operator status.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use bedrockrs_protocol::packets::DisconnectReason;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::game_mode::GameMode;

/// Why a session must disconnect its player, and what the player is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KickNotice {
    pub reason: DisconnectReason,
    pub message: String,
}

/// Something the rest of the server tells a player's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Disconnect the player.
    Kick(KickNotice),
    SetGameMode(GameMode),
    /// The player became an operator, or stopped being one.
    SetOperator(bool),
    /// The commands players may use changed; send the player theirs.
    RefreshCommands,
}

/// Where a session receives its [`Control`]s.
pub type Controls = mpsc::Sender<Control>;

/// How many controls may wait for a session.
pub const CONTROL_QUEUE: usize = 16;

/// The message a session kicked by a newer login shows.
pub const LOGGED_IN_ELSEWHERE: &str = "You logged in from another location.";

/// The sessions of logged-in players, by verified UUID.
#[derive(Debug, Default)]
pub struct Logins {
    last_id: AtomicU64,
    active: Mutex<HashMap<Uuid, (u64, Controls)>>,
}

impl Logins {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the session logged in as `uuid`, kicking any older session
    /// logged in as the same player. The claim lasts until it is dropped.
    pub fn claim(&self, uuid: Uuid, controls: Controls) -> LoginClaim<'_> {
        let id = self.last_id.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some((_, older)) = self.active().insert(uuid, (id, controls)) {
            tracing::info!(%uuid, "the player logged in again; kicking the older session");
            let _ = older.try_send(Control::Kick(KickNotice {
                reason: DisconnectReason::LOGGED_IN_OTHER_LOCATION,
                message: LOGGED_IN_ELSEWHERE.to_owned(),
            }));
        }
        LoginClaim {
            logins: self,
            uuid,
            id,
        }
    }

    pub fn is_logged_in(&self, uuid: Uuid) -> bool {
        self.active().contains_key(&uuid)
    }

    /// Disconnects the player logged in as `uuid`, showing them `message`.
    /// Returns whether they were logged in.
    pub fn kick(&self, uuid: Uuid, message: String) -> bool {
        self.send(
            uuid,
            Control::Kick(KickNotice {
                reason: DisconnectReason::KICKED,
                message,
            }),
        )
    }

    /// Tells the session logged in as `uuid` to do something. Returns whether
    /// there is such a session.
    pub fn send(&self, uuid: Uuid, control: Control) -> bool {
        let Some((_, controls)) = self.active().get(&uuid).cloned() else {
            return false;
        };
        if controls.try_send(control).is_err() {
            tracing::warn!(%uuid, "a session is not keeping up with what it is told");
        }
        true
    }

    /// Tells every logged-in session to do something.
    pub fn send_all(&self, control: &Control) {
        let sessions: Vec<Controls> = self
            .active()
            .values()
            .map(|(_, controls)| controls.clone())
            .collect();
        for controls in sessions {
            let _ = controls.try_send(control.clone());
        }
    }

    fn active(&self) -> MutexGuard<'_, HashMap<Uuid, (u64, Controls)>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A session's claim to its player's UUID; dropping it releases the UUID,
/// unless a newer session has taken it over.
#[must_use = "the claim is released when dropped"]
#[derive(Debug)]
pub struct LoginClaim<'a> {
    logins: &'a Logins,
    uuid: Uuid,
    id: u64,
}

impl Drop for LoginClaim<'_> {
    fn drop(&mut self) {
        let mut active = self.logins.active();
        if active.get(&self.uuid).is_some_and(|(id, _)| *id == self.id) {
            active.remove(&self.uuid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_login_kicks_the_first() {
        let logins = Logins::new();
        let uuid = Uuid::new_v4();
        let (first_kick, mut first) = mpsc::channel(1);
        let (second_kick, mut second) = mpsc::channel(1);

        let first_claim = logins.claim(uuid, first_kick);
        assert!(first.try_recv().is_err());
        let second_claim = logins.claim(uuid, second_kick);
        let Control::Kick(notice) = first.try_recv().unwrap() else {
            panic!("expected a kick");
        };
        assert_eq!(notice.reason, DisconnectReason::LOGGED_IN_OTHER_LOCATION);
        assert_eq!(notice.message, LOGGED_IN_ELSEWHERE);
        assert!(second.try_recv().is_err(), "the newer session stays");

        // The kicked session leaving does not release the newer one's claim.
        drop(first_claim);
        assert!(logins.is_logged_in(uuid));
        drop(second_claim);
        assert!(!logins.is_logged_in(uuid));
    }

    #[test]
    fn logged_in_players_can_be_kicked() {
        let logins = Logins::new();
        let uuid = Uuid::new_v4();
        assert!(!logins.kick(uuid, "bye".into()), "not logged in");

        let (kick, mut kicks) = mpsc::channel(1);
        let _claim = logins.claim(uuid, kick);
        assert!(logins.kick(uuid, "bye".into()));
        assert_eq!(
            kicks.try_recv().unwrap(),
            Control::Kick(KickNotice {
                reason: DisconnectReason::KICKED,
                message: "bye".into()
            })
        );
    }

    #[test]
    fn sessions_are_told_what_to_do() {
        let logins = Logins::new();
        let uuid = Uuid::new_v4();
        assert!(
            !logins.send(uuid, Control::SetOperator(true)),
            "not logged in"
        );
        let (controls, mut received) = mpsc::channel(CONTROL_QUEUE);
        let _claim = logins.claim(uuid, controls);
        assert!(logins.send(uuid, Control::SetGameMode(GameMode::Survival)));
        logins.send_all(&Control::RefreshCommands);
        assert_eq!(
            received.try_recv().unwrap(),
            Control::SetGameMode(GameMode::Survival)
        );
        assert_eq!(received.try_recv().unwrap(), Control::RefreshCommands);
    }

    #[test]
    fn different_players_do_not_kick_each_other() {
        let logins = Logins::new();
        let (kick, mut kicks) = mpsc::channel(1);
        let _steve = logins.claim(Uuid::new_v4(), kick.clone());
        let _alex = logins.claim(Uuid::new_v4(), kick);
        assert!(kicks.try_recv().is_err());
    }
}
