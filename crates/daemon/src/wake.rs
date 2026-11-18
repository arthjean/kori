// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! How the daemon's own threads wait, and what they are called.
//!
//! A thread with nothing to do until a deadline sleeps until that deadline, in
//! one wait, and a stop request ends the wait at once. The loops used to sleep
//! in 50 ms slices so they would notice a stop: four of them woke the processor
//! about 114 times a second to find, nearly every time, that nothing had
//! changed. [`Alarm`] is the one place that rule lives, so a loop cannot follow
//! a different one.
//!
//! Every thread the daemon starts goes through [`spawn`], which names it
//! `korid-<role>`. The name is what `top -H`, `/proc/<pid>/task/*/comm` and a
//! test counting wakeups go by, and an unnamed thread is one nobody can account
//! for.

use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// What every daemon thread's name starts with.
pub const THREAD_PREFIX: &str = "korid-";

/// Start a thread named `korid-<role>`.
///
/// Fallible rather than panicking like `std::thread::spawn`: the process can
/// run out of threads, and each caller decides what that means for its own
/// work instead of the whole daemon aborting mid-write. Linux keeps fifteen
/// bytes of a thread name, so `role` stays short.
pub fn spawn<T, F>(role: &str, body: F) -> std::io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(format!("{THREAD_PREFIX}{role}"))
        .spawn(body)
}

/// Why a wait on an [`Alarm`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The deadline was reached, or had already passed.
    Due,
    /// Somebody rang: what the deadline was computed from may have changed.
    Rung,
    /// A stop was requested. The waiter finishes and exits.
    Stopped,
}

/// A deadline wait that a stop request or a ring interrupts.
#[derive(Debug, Default)]
pub struct Alarm {
    state: Mutex<Bell>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct Bell {
    stopped: bool,
    /// Bumped on every ring, so a waiter can tell a ring it has not seen.
    rings: u64,
}

impl Alarm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask every waiter to stop, now and for every later wait.
    pub fn stop(&self) {
        if let Ok(mut bell) = self.state.lock() {
            bell.stopped = true;
        }
        self.changed.notify_all();
    }

    /// Whether a stop was requested.
    ///
    /// A poisoned lock reads as stopped: a thread panicked while holding it,
    /// and stopping is the direction that cannot leave a write half done.
    pub fn is_stopped(&self) -> bool {
        self.state.lock().map_or(true, |bell| bell.stopped)
    }

    /// Wake the waiter so it recomputes its deadline.
    pub fn ring(&self) {
        if let Ok(mut bell) = self.state.lock() {
            bell.rings = bell.rings.wrapping_add(1);
        }
        self.changed.notify_all();
    }

    /// How many rings have happened, for a later [`Self::wait`].
    ///
    /// Read *before* looking at whatever the deadline is computed from. A ring
    /// that lands between that look and the wait is then still seen, where a
    /// count taken at the start of the wait would have missed it and slept on
    /// a stale deadline.
    pub fn mark(&self) -> u64 {
        self.state.lock().map_or(0, |bell| bell.rings)
    }

    /// Wait until `deadline`, until a ring after `seen`, or until a stop.
    ///
    /// One wait, however far the deadline is. A deadline already in the past
    /// returns [`Wake::Due`] without waiting at all, so the wait can be
    /// neither negative nor longer than the caller asked. A spurious wakeup
    /// waits again for whatever is left.
    pub fn wait(&self, seen: u64, deadline: Instant) -> Wake {
        let Ok(mut bell) = self.state.lock() else {
            return Wake::Stopped;
        };
        loop {
            if bell.stopped {
                return Wake::Stopped;
            }
            if bell.rings != seen {
                return Wake::Rung;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Wake::Due;
            }
            bell = match self.changed.wait_timeout(bell, left) {
                Ok((bell, _)) => bell,
                Err(_) => return Wake::Stopped,
            };
        }
    }

    /// Wait until `deadline` or a stop, ignoring rings.
    pub fn sleep_until(&self, deadline: Instant) -> Wake {
        loop {
            match self.wait(self.mark(), deadline) {
                Wake::Rung => continue,
                other => return other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_deadline_in_the_past_returns_at_once() {
        let alarm = Alarm::new();
        let started = Instant::now();
        let past = started
            .checked_sub(Duration::from_secs(5))
            .unwrap_or(started);
        assert_eq!(alarm.sleep_until(past), Wake::Due);
        assert!(started.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn a_wait_lasts_until_its_deadline_and_no_longer() {
        let alarm = Alarm::new();
        let started = Instant::now();
        assert_eq!(
            alarm.sleep_until(started + Duration::from_millis(60)),
            Wake::Due
        );
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(60), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn a_stop_ends_a_long_wait_at_once() {
        let alarm = Arc::new(Alarm::new());
        let waiter = {
            let alarm = Arc::clone(&alarm);
            spawn("test-wait", move || {
                let started = Instant::now();
                let wake = alarm.sleep_until(started + Duration::from_secs(30));
                (wake, started.elapsed())
            })
            .unwrap()
        };
        std::thread::sleep(Duration::from_millis(30));
        let asked = Instant::now();
        alarm.stop();
        let (wake, _) = waiter.join().unwrap();
        assert_eq!(wake, Wake::Stopped);
        assert!(asked.elapsed() < Duration::from_millis(100));
        // And every later wait is over before it starts.
        assert_eq!(
            alarm.sleep_until(Instant::now() + Duration::from_secs(30)),
            Wake::Stopped
        );
    }

    #[test]
    fn a_ring_between_the_mark_and_the_wait_is_not_lost() {
        let alarm = Alarm::new();
        let seen = alarm.mark();
        alarm.ring();
        let started = Instant::now();
        assert_eq!(
            alarm.wait(seen, started + Duration::from_secs(30)),
            Wake::Rung
        );
        assert!(started.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn a_spawned_thread_carries_the_daemon_prefix() {
        let name = spawn("test-name", || {
            std::thread::current().name().map(str::to_string)
        })
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(name.as_deref(), Some("korid-test-name"));
    }

    /// Every thread the daemon starts is named, because a thread started any
    /// other way is one the wakeup budget cannot see.
    #[test]
    fn no_daemon_source_starts_a_thread_without_naming_it() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&source).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Test modules sit at the end of each file and may start threads
            // of their own; what ships is everything before them.
            let shipped = text
                .split("#[cfg(test)]\nmod tests")
                .next()
                .unwrap_or_default();
            for (line, content) in shipped.lines().enumerate() {
                if content.contains("thread::spawn(") || content.contains("thread::Builder") {
                    offenders.push(format!("{}:{}", path.display(), line + 1));
                }
            }
        }
        // This file is the one place a builder is allowed.
        offenders.retain(|place| !place.contains("wake.rs"));
        assert!(offenders.is_empty(), "unnamed spawns: {offenders:?}");
    }
}
