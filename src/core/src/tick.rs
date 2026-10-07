//! The game loop: a dedicated OS thread that advances the server 20 times a
//! second, independently of network traffic.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::TICK_DURATION;
use crate::server::Server;

/// How far behind schedule the loop may fall before it stops catching up and
/// skips the missed ticks instead.
const MAX_LAG: Duration = Duration::from_secs(1);

/// The running game loop. Dropping it stops the loop after the current tick
/// and saves the world.
pub struct TickLoop {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TickLoop {
    /// Starts ticking `server` on a thread named `game-loop`.
    pub fn start(server: Arc<Server>) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("game-loop".into())
            .spawn(move || run(&server, &stopping))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for TickLoop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Runs ticks on a fixed schedule. A slow tick is followed by quicker ones
/// until the loop is back on schedule, unless it fell more than [`MAX_LAG`]
/// behind.
fn run(server: &Server, stop: &AtomicBool) {
    let mut tick = 0u64;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        tick += 1;
        let started = Instant::now();
        server.tick(tick);
        let spent = started.elapsed();
        if spent > TICK_DURATION {
            tracing::warn!(
                "Can't keep up: tick {tick} took {} ms, more than the 50 it has",
                spent.as_millis()
            );
        }

        next += TICK_DURATION;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else if now - next > MAX_LAG {
            let skipped = (now - next).as_millis() / TICK_DURATION.as_millis();
            tracing::warn!(tick, skipped, "the game loop fell behind; skipping ticks");
            next = now;
        }
    }
    // Keep the changes since the last periodic save.
    server.save();
}

#[cfg(test)]
mod tests {
    use bedrockrs_plugins::Dispatcher;

    use super::*;
    use crate::auth::Authenticator;
    use crate::world::World;

    #[test]
    fn ticks_about_twenty_times_a_second_until_dropped() {
        let server = Arc::new(Server::new(
            World::new(),
            Dispatcher::disconnected(),
            Authenticator::offline(),
        ));
        let ticks = TickLoop::start(Arc::clone(&server)).unwrap();
        thread::sleep(Duration::from_millis(520));
        drop(ticks);
        let count = server.current_tick();
        // 11 ticks are due in 520 ms; allow for a busy test machine.
        assert!((5..=12).contains(&count), "{count} ticks");
        thread::sleep(Duration::from_millis(120));
        assert_eq!(server.current_tick(), count, "the loop stopped");
    }
}
