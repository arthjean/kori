// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The pushed stream a subscribed connection carries.
//!
//! Two rules shape everything here, both about the state lock that serializes
//! every hardware write:
//!
//! - **Nothing is written to a subscriber while the lock is held.** A pass
//!   takes the lock, copies what it is going to send, releases it, and only
//!   then writes. A subscriber that stops reading can stall its own thread for
//!   the length of the write timeout and nothing else.
//! - **The ticker never waits on a subscriber.** It bumps a counter in the
//!   [`Hub`] and wakes whoever is waiting, which costs a mutex held for a few
//!   instructions. It never learns how many subscribers there are or what they
//!   are doing.
//!
//! Each subscriber runs on the thread that already serves its connection,
//! pulling rather than being pushed to: on every wake it reads what changed
//! since its last pass. Its queue is therefore the frames one pass collects,
//! bounded at [`SUBSCRIBER_QUEUE`], with only the newest telemetry and status
//! in it. Events past that bound wait in the daemon's ring for the next pass,
//! which starts at once; events that left the ring before the subscriber came
//! back for them are counted into a single `lagged` frame instead.

use std::io::BufWriter;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use kori_core::ipc::{DaemonStatus, FrameError, Push, SUBSCRIBER_QUEUE, Topic, write_frame};

use crate::state::Daemon;

/// Longest a subscriber sleeps before checking that its client is still there.
const LIVENESS_CHECK: Duration = Duration::from_secs(1);

/// What wakes subscribers.
#[derive(Debug, Default)]
pub struct Hub {
    state: Mutex<Mark>,
    changed: Condvar,
}

/// Where the daemon stood when a subscriber last looked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Mark {
    /// Bumped on every change worth a look: a tick, or a request handled.
    generation: u64,
    /// Ticks of the daemon's clock, which paces telemetry and the status check.
    ticks: u64,
    /// Set once the orderly shutdown started, with its reason.
    closing: Option<String>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// One tick of the daemon's clock happened.
    pub fn tick(&self) {
        self.update(|mark| mark.ticks += 1);
    }

    /// Something may have changed: a request was handled, an event recorded.
    pub fn notify(&self) {
        self.update(|_| {});
    }

    /// Tell every subscriber the daemon is going, and why.
    pub fn close(&self, reason: String) {
        self.update(|mark| {
            mark.closing.get_or_insert(reason);
        });
    }

    fn update(&self, change: impl FnOnce(&mut Mark)) {
        if let Ok(mut mark) = self.state.lock() {
            change(&mut mark);
            mark.generation += 1;
        }
        self.changed.notify_all();
    }

    fn mark(&self) -> Mark {
        self.state
            .lock()
            .map(|mark| mark.clone())
            .unwrap_or_default()
    }

    /// Wait until the generation moves past `seen`, or `timeout` runs out.
    fn wait(&self, seen: u64, timeout: Duration) -> Mark {
        let Ok(mark) = self.state.lock() else {
            return Mark::default();
        };
        match self
            .changed
            .wait_timeout_while(mark, timeout, |mark| mark.generation == seen)
        {
            Ok((mark, _)) => mark.clone(),
            Err(_) => Mark::default(),
        }
    }

    /// Wait for any change, at most `timeout`.
    pub fn wait_any(&self, timeout: Duration) {
        let seen = self.mark().generation;
        self.wait(seen, timeout);
    }
}

/// What a subscription asked for.
#[derive(Debug, Clone)]
pub struct Plan {
    pub topics: Vec<Topic>,
    /// Telemetry is pushed every this many ticks.
    pub interval_s: u32,
    /// The last event sequence number the subscriber has, so the first push
    /// carries what came after it.
    pub cursor: u64,
    /// Longest the connection may go without a frame.
    pub heartbeat: Duration,
}

/// Serve a subscription until it ends, and say why it ended.
///
/// `stream` is the connection, read only to notice that the client closed it;
/// nothing the client sends after `subscribed` is ever parsed. The reason
/// returned is what the `client_disconnected` event records.
pub fn serve(
    stream: &UnixStream,
    writer: &mut BufWriter<UnixStream>,
    daemon: &Arc<Mutex<Daemon>>,
    hub: &Hub,
    plan: Plan,
) -> String {
    if let Err(error) = stream.set_write_timeout(Some(kori_core::ipc::SUBSCRIBER_WRITE_TIMEOUT)) {
        return format!("the subscriber's write timeout could not be set: {error}");
    }
    let wants = |topic| plan.topics.contains(&topic);
    let mut cursor = plan.cursor;
    let mut last_status: Option<DaemonStatus> = None;
    let mut mark = hub.mark();
    let mut seen_ticks: Option<u64> = None;
    let mut telemetry_tick: Option<u64> = None;
    let mut last_write = Instant::now();

    loop {
        let ticked = seen_ticks != Some(mark.ticks);
        seen_ticks = Some(mark.ticks);
        let telemetry_due = wants(Topic::Telemetry)
            && telemetry_tick
                .is_none_or(|last| mark.ticks.saturating_sub(last) >= u64::from(plan.interval_s));

        // Everything this pass sends is copied under the lock, then the lock
        // is released before the first byte is written.
        let mut queue: Vec<Push> = Vec::new();
        let mut backlog = false;
        let mut collect = |daemon: &Daemon| {
            let mut reserved = 0;
            let status = (wants(Topic::Status) && ticked).then(|| daemon.status());
            let status = status.filter(|status| last_status.as_ref() != Some(status));
            reserved += usize::from(status.is_some());
            let telemetry = telemetry_due.then(|| daemon.telemetry());
            reserved += usize::from(telemetry.is_some());

            if wants(Topic::Events) {
                // One frame is kept for the `lagged` notice itself.
                let room = SUBSCRIBER_QUEUE.saturating_sub(reserved + 1);
                let page = daemon.events_after(cursor, room as u32);
                if page.missed > 0 {
                    queue.push(Push::Lagged {
                        missed: page.missed,
                    });
                }
                // Past the events the ring dropped, whether or not this pass
                // could carry anything after them.
                cursor = cursor.max(page.oldest_seq.saturating_sub(1));
                if let Some(last) = page.events.last() {
                    cursor = last.seq;
                }
                backlog = page.latest_seq > cursor;
                queue.extend(page.events.into_iter().map(Push::Event));
            }
            if let Some(status) = status {
                last_status = Some(status.clone());
                queue.push(Push::Status(Box::new(status)));
            }
            if let Some(telemetry) = telemetry {
                queue.push(Push::Telemetry(Box::new(telemetry)));
            }
        };
        match daemon.lock() {
            Ok(daemon) => collect(&daemon),
            Err(_) => return "daemon state is unavailable".to_string(),
        }
        if telemetry_due {
            telemetry_tick = Some(mark.ticks);
        }

        for push in &queue {
            if let Err(error) = write_frame(writer, push) {
                return write_failure(&error);
            }
        }
        if !queue.is_empty() {
            last_write = Instant::now();
        }

        if let Some(reason) = mark.closing.clone() {
            let _ = write_frame(writer, &Push::Closing { reason });
            return "the daemon is shutting down".to_string();
        }
        if peer_closed(stream) {
            return "the subscriber closed the connection".to_string();
        }
        if last_write.elapsed() >= plan.heartbeat {
            let heartbeat = Push::Heartbeat {
                at_unix_ms: crate::now_unix_ms(),
            };
            if let Err(error) = write_frame(writer, &heartbeat) {
                return write_failure(&error);
            }
            last_write = Instant::now();
        }

        // More events wait than one pass could carry: take them now rather
        // than at the next tick, so a burst is late by one write, not by its
        // length in seconds.
        if backlog {
            mark = hub.mark();
            continue;
        }
        let until_heartbeat = plan.heartbeat.saturating_sub(last_write.elapsed());
        mark = hub.wait(mark.generation, until_heartbeat.min(LIVENESS_CHECK));
    }
}

/// The reason a push could not be written, in the words the event records.
fn write_failure(error: &FrameError) -> String {
    match error {
        FrameError::Io(source)
            if matches!(
                source.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            format!(
                "the subscriber did not accept a frame within {} s",
                kori_core::ipc::SUBSCRIBER_WRITE_TIMEOUT.as_secs()
            )
        }
        FrameError::Io(source)
            if matches!(
                source.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            "the subscriber closed the connection".to_string()
        }
        other => format!("a push could not be written: {other}"),
    }
}

/// Whether the client has closed its end.
///
/// A peek that does not wait: zero bytes is the end of the stream. Bytes the
/// client sent anyway are left where they are and never parsed, so a client
/// cannot steer a subscription by writing into it.
fn peer_closed(stream: &UnixStream) -> bool {
    use rustix::net::RecvFlags;
    let mut probe = [0u8; 1];
    matches!(
        rustix::net::recv(
            stream,
            &mut probe[..],
            RecvFlags::PEEK | RecvFlags::DONTWAIT
        ),
        Ok((_, 0))
    )
}
