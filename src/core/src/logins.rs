//! One session per player: a verified login with a UUID that is already
//! connected kicks the older session, as vanilla does. Plugins kick players
//! through the same registry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use bedrockrs_protocol::packets::DisconnectReason;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Why a session must disconnect its player, and what the player is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KickNotice {
    pub reason: DisconnectReason,
    pub message: String,
}

/// Tells a session to disconnect its player.
pub type Kick = mpsc::Sender<KickNotice>;

/// The message a session kicked by a newer login shows.
pub const LOGGED_IN_ELSEWHERE: &str = "You logged in from another location.";

/// The sessions of logged-in players, by verified UUID.
#[derive(Debug, Default)]
pub struct Logins {
    last_id: AtomicU64,
    active: Mutex<HashMap<Uuid, (u64, Kick)>>,
}

impl Logins {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the session logged in as `uuid`, kicking any older session
    /// logged in as the same player. The claim lasts until it is dropped.
    pub fn claim(&self, uuid: Uuid, kick: Kick) -> LoginClaim<'_> {
        let id = self.last_id.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some((_, older)) = self.active().insert(uuid, (id, kick)) {
            tracing::info!(%uuid, "the player logged in again; kicking the older session");
            let _ = older.try_send(KickNotice {
                reason: DisconnectReason::LOGGED_IN_OTHER_LOCATION,
                message: LOGGED_IN_ELSEWHERE.to_owned(),
            });
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
        let Some((_, kick)) = self.active().get(&uuid).cloned() else {
            return false;
        };
        let _ = kick.try_send(KickNotice {
            reason: DisconnectReason::KICKED,
            message,
        });
        true
    }

    fn active(&self) -> MutexGuard<'_, HashMap<Uuid, (u64, Kick)>> {
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
        let notice = first.try_recv().unwrap();
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
            KickNotice {
                reason: DisconnectReason::KICKED,
                message: "bye".into()
            }
        );
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
