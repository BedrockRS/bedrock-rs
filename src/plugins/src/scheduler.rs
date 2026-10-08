//! A plugin's scheduled tasks: when each runs, by the server's tick.
//!
//! The API is `@minecraft/server`'s: `world.run(callback)` runs a callback
//! on the next tick, `world.runTimeout(callback, ticks)` once after `ticks`
//! ticks, `world.runInterval(callback, ticks)` every `ticks` ticks, and
//! `world.clearRun(id)` stops one; `world.waitTicks(ticks)` waits (an
//! `await` in JavaScript, a coroutine yield in Luau) by scheduling its own
//! resumption here.
//!
//! This table only knows ids and ticks. The callbacks themselves stay in the
//! plugin's VM, under the same ids, so each engine keeps its own values; the
//! plugin manager asks each plugin to run the ids that are due. Plugins never
//! see a tick: a plugin with nothing scheduled is never called.

use std::collections::BTreeMap;

/// The longest a task may wait: a day, at 20 ticks a second.
pub const MAX_DELAY: u64 = 20 * 60 * 60 * 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Task {
    /// The tick it runs at next.
    due: u64,
    /// For a repeating task, the ticks between runs.
    every: Option<u64>,
}

/// One plugin's scheduled tasks, by id.
#[derive(Debug, Default)]
pub struct Tasks {
    next_id: u32,
    tasks: BTreeMap<u32, Task>,
}

impl Tasks {
    /// Schedules a task `delay` ticks after `now`, then every `every` ticks
    /// if it repeats. Returns its id. A delay of 0 is the next tick.
    pub fn schedule(&mut self, now: u64, delay: u64, every: Option<u64>) -> u32 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.tasks.insert(
            id,
            Task {
                due: now + delay.max(1),
                every: every.map(|every| every.max(1)),
            },
        );
        id
    }

    /// Stops a task. Returns whether it was scheduled.
    pub fn clear(&mut self, id: u32) -> bool {
        self.tasks.remove(&id).is_some()
    }

    /// Stops every task.
    pub fn clear_all(&mut self) {
        self.tasks.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Whether anything is due at `now`.
    pub fn any_due(&self, now: u64) -> bool {
        self.tasks.values().any(|task| task.due <= now)
    }

    /// The ids due at `now`, in the order they were due (then scheduled):
    /// one-off tasks are taken off the table, repeating ones moved on.
    pub fn take_due(&mut self, now: u64) -> Vec<u32> {
        let mut due: Vec<(u64, u32)> = self
            .tasks
            .iter()
            .filter(|(_, task)| task.due <= now)
            .map(|(id, task)| (task.due, *id))
            .collect();
        due.sort_unstable();
        for (_, id) in &due {
            let task = self.tasks.get_mut(id).expect("just found");
            match task.every {
                Some(every) => task.due = now + every,
                None => {
                    self.tasks.remove(id);
                }
            }
        }
        due.into_iter().map(|(_, id)| id).collect()
    }

    /// Whether a task is still scheduled: a repeating one, or a one-off not
    /// yet run.
    pub fn contains(&self, id: u32) -> bool {
        self.tasks.contains_key(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_run_once_and_intervals_repeat() {
        let mut tasks = Tasks::default();
        let once = tasks.schedule(100, 5, None);
        let every = tasks.schedule(100, 2, Some(2));
        assert!(!tasks.any_due(101));
        assert_eq!(tasks.take_due(102), [every]);
        assert_eq!(tasks.take_due(104), [every]);
        assert_eq!(tasks.take_due(105), [once]);
        assert!(!tasks.contains(once), "a timeout runs once");
        assert_eq!(tasks.take_due(106), [every]);
        assert!(tasks.clear(every));
        assert!(!tasks.clear(every));
        assert!(tasks.is_empty());
    }

    #[test]
    fn a_delay_of_nothing_is_the_next_tick() {
        let mut tasks = Tasks::default();
        let id = tasks.schedule(7, 0, None);
        assert!(tasks.take_due(7).is_empty());
        assert_eq!(tasks.take_due(8), [id]);
    }

    #[test]
    fn late_ticks_run_what_was_due_in_order() {
        let mut tasks = Tasks::default();
        let later = tasks.schedule(0, 3, None);
        let sooner = tasks.schedule(0, 1, None);
        // A slow server skipped ticks: both run, the sooner first.
        assert_eq!(tasks.take_due(10), [sooner, later]);
    }
}
