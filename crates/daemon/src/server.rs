// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! The local Unix socket server.
//!
//! One listener, owner-only permissions, no TCP or UDP endpoint anywhere. Each
//! connection is authenticated against the peer's credentials, must complete a
//! version handshake, and is served under the state mutex so commands cannot
//! interleave.

use std::io::{BufReader, BufWriter};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kori_core::display::FRAME_INTERVAL_MS;
use kori_core::ipc::{HEARTBEAT_INTERVAL, IpcError, Request, Response, read_frame, write_frame};

use crate::state::Daemon;
use crate::stream::{self, Hub};
use crate::wake::{Alarm, Wake};

/// Longest the shutdown waits for subscribers to take their `closing` push.
///
/// A little past the write timeout, so a subscriber that stopped reading has
/// been dropped by its own thread before the process goes.
const DRAIN_LIMIT: Duration = Duration::from_millis(2_500);

/// Shortest the display ticker's wait is allowed to be.
///
/// Every deadline the ticker computes is ahead of the clock when it computes
/// it, so this is a guard rather than a cadence: a deadline that somehow came
/// out in the past would otherwise compute no wait at all, and a loop that
/// takes the state mutex without ever yielding is a spin against every request
/// handler.
const TICK_FLOOR: Duration = Duration::from_millis(1);

/// A running server and the handle used to stop it.
pub struct Server {
    listener: UnixListener,
    daemon: Arc<Mutex<Daemon>>,
    /// Stops the accept loop and the ticker, and wakes the ticker when a
    /// request may have changed what it waits for.
    alarm: Arc<Alarm>,
    /// Why the shutdown was asked for, as `daemon_stopping` records it.
    stop_reason: Arc<Mutex<Option<String>>>,
    frame_interval: Duration,
    /// Wakes subscribers. The ticker and the request handlers notify it after
    /// releasing the state lock.
    hub: Arc<Hub>,
    heartbeat: Duration,
}

/// Bind the socket with owner-only permissions.
///
/// The caller holds the single-instance lock, so a file already at `path` was
/// left behind by a dead daemon and is removed. Probing it with a connect
/// first would not make this safer: the probe and the bind cannot be made
/// atomic, and two starters racing that sequence can both unlink and both
/// bind, which leaves one of them accepting on an inode no client can reach
/// (`unix(7)`). The lock removes the race the probe only narrowed.
pub fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

impl Server {
    /// Serve `daemon` on a listener that is already bound and reachable.
    pub fn attach(listener: UnixListener, daemon: Daemon) -> Self {
        Self {
            listener,
            daemon: Arc::new(Mutex::new(daemon)),
            alarm: Arc::new(Alarm::new()),
            stop_reason: Arc::new(Mutex::new(None)),
            frame_interval: Duration::from_millis(FRAME_INTERVAL_MS),
            hub: Arc::new(Hub::new()),
            heartbeat: HEARTBEAT_INTERVAL,
        }
    }

    /// Send a heartbeat after `interval` of silence instead of ten seconds.
    ///
    /// Only the cadence changes, so a test sees a heartbeat without waiting
    /// for the shipped one.
    pub fn with_heartbeat(mut self, interval: Duration) -> Self {
        self.heartbeat = interval;
        self
    }

    /// Redraw the panel at `interval` instead of once a second.
    ///
    /// Only the cadence changes. A test exercises the same streaming code
    /// without spending a real second per frame.
    pub fn with_frame_interval(mut self, interval: Duration) -> Self {
        self.frame_interval = interval;
        self
    }

    pub fn daemon(&self) -> Arc<Mutex<Daemon>> {
        Arc::clone(&self.daemon)
    }

    /// A handle that stops the accept loop.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            alarm: Arc::clone(&self.alarm),
            reason: Arc::clone(&self.stop_reason),
            socket: self.local_path(),
        }
    }

    fn local_path(&self) -> Option<std::path::PathBuf> {
        self.listener
            .local_addr()
            .ok()
            .and_then(|addr| addr.as_pathname().map(Path::to_path_buf))
    }

    /// Run the daemon's own clock forever.
    ///
    /// Everything the daemon owes the hardware rather than a client happens
    /// here: reconciling what the device reports against what this process
    /// committed, and redrawing the panel. Both belong to the daemon, not to a
    /// window: a preset keeps its readings current and a curve keeps being
    /// checked with nobody connected, which is the whole reason they live on
    /// this side of the socket.
    ///
    /// A tick that runs late does not catch up. The missed frames are counted
    /// and discarded, because the next tick draws the current reading and a
    /// backlog of old ones has nothing to offer a panel.
    ///
    /// This one thread carries both cadences: the telemetry pass at
    /// `frame_interval`, and an animation on the clock its own file declares.
    /// One thread rather than two because the writes to the panel are
    /// serialized by construction here, and a second thread would put that back
    /// on the mutex to enforce.
    ///
    /// Between two passes the thread waits for the earlier of its two
    /// deadlines, in one wait. A stop ends it at once, and so does a ring: a
    /// request handler rings after every request, because a preset applied
    /// while the ticker waits for the next telemetry pass may have started an
    /// animation whose second frame is due long before that.
    fn spawn_display_ticker(&self) -> std::io::Result<std::thread::JoinHandle<()>> {
        let daemon = Arc::clone(&self.daemon);
        let alarm = Arc::clone(&self.alarm);
        let interval = self.frame_interval;
        let hub = Arc::clone(&self.hub);
        crate::wake::spawn("ticker", move || {
            let mut next = Instant::now() + interval;
            // Nothing is playing until a preset says so, so the loop starts on
            // the idle cadence and only shortens once an animation asks it to.
            let mut deadline = next;
            // Taken before the state is read, every time, so a ring from a
            // request handled after that read wakes the next wait.
            let mut seen = alarm.mark();
            loop {
                let floor = Instant::now() + TICK_FLOOR;
                if alarm.wait(seen, deadline.max(floor)) == Wake::Stopped {
                    break;
                }
                seen = alarm.mark();
                let now = Instant::now();

                let mut animation_due = None;
                let mut ticked = false;
                if let Ok(mut daemon) = daemon.lock() {
                    // The animation is asked first: it runs at up to a dozen
                    // frames a second against the telemetry redraw's one, so it
                    // is what the sleep below is sized against.
                    animation_due = daemon.tick_animation(now);

                    if now >= next {
                        let mut missed = 0u32;
                        while next + interval <= now {
                            next += interval;
                            missed += 1;
                        }
                        next += interval;

                        daemon.skip_ticks(missed);
                        daemon.tick(now);
                        ticked = true;
                    }
                }
                // After the lock is released, and without waiting on anyone:
                // a subscriber learns of the tick when it next looks.
                if ticked {
                    hub.tick();
                }

                // Wait for whichever comes first.
                deadline = match animation_due {
                    Some(due) => due.min(next),
                    None => next,
                };
            }
        })
    }

    /// Serve connections until the shutdown handle fires.
    ///
    /// Fails only when the daemon's clock cannot be started, before any client
    /// is served: without it nothing reconciles what the hardware reports
    /// against what was committed, and a daemon that took writes it then never
    /// checked is worse than one that did not start.
    pub fn run(self) -> std::io::Result<()> {
        let ticker = self.spawn_display_ticker()?;

        for stream in self.listener.incoming() {
            if self.alarm.is_stopped() {
                break;
            }
            let Ok(stream) = stream else { continue };

            let daemon = Arc::clone(&self.daemon);
            let hub = Arc::clone(&self.hub);
            let alarm = Arc::clone(&self.alarm);
            let heartbeat = self.heartbeat;
            // One thread per client. The state mutex, not the thread count,
            // is what serializes commands.
            let started = crate::wake::spawn("client", move || {
                serve(stream, daemon, hub, alarm, heartbeat)
            });
            // The connection went with the closure, so the client sees it
            // closed; the log says why.
            if let Err(error) = started {
                note(&self.daemon, |daemon| {
                    daemon.record_client_rejected(format!(
                        "no thread could be started for the connection: {error}"
                    ))
                });
            }
        }

        // Recorded first, while every subsystem is still up, so the event is in
        // the log a subscriber reads on its way out rather than lost with it.
        let reason = self
            .stop_reason
            .lock()
            .ok()
            .and_then(|mut reason| reason.take())
            .unwrap_or_else(|| "shutdown requested".to_string());
        note(&self.daemon, |daemon| {
            daemon.record_stopping(reason.clone())
        });
        self.hub.close(reason);

        // The ticker holds the same lock the request handlers do, so it is
        // joined before the socket is removed: a frame half written while the
        // daemon is being torn down is exactly the state this product refuses
        // to leave the hardware in.
        let _ = ticker.join();

        // Every subscriber takes its last events and its `closing` push on its
        // own thread. They are waited for, briefly, so the push is on the wire
        // before the process exits; one that stopped reading is dropped by its
        // own write timeout inside this limit.
        let deadline = Instant::now() + DRAIN_LIMIT;
        while Instant::now() < deadline {
            let remaining = self.daemon.lock().map_or(0, |daemon| daemon.subscribers());
            if remaining == 0 {
                break;
            }
            self.hub.wait_any(Duration::from_millis(50));
        }

        // The socket goes before the last wait, so no further client can
        // connect while this is finishing.
        if let Some(path) = self.local_path() {
            let _ = std::fs::remove_file(path);
        }

        // A client thread already inside `handle` holds this lock for the
        // length of one command, which is the length of one hardware write.
        // Taking it once here lets that write finish before the process
        // returns. Client threads are not joined: one sitting idle on an open
        // connection would block shutdown forever, and it is the write that
        // has to complete, not the connection.
        drop(self.daemon.lock());
        Ok(())
    }
}

/// Stops a running [`Server`].
pub struct ShutdownHandle {
    alarm: Arc<Alarm>,
    reason: Arc<Mutex<Option<String>>>,
    socket: Option<std::path::PathBuf>,
}

impl ShutdownHandle {
    /// Set the flag, then poke the listener so `accept` returns.
    pub fn stop(&self) {
        self.stop_because("shutdown requested");
    }

    /// The same, naming why, for the `daemon_stopping` event.
    pub fn stop_because(&self, reason: impl Into<String>) {
        if let Ok(mut slot) = self.reason.lock() {
            slot.get_or_insert_with(|| reason.into());
        }
        self.alarm.stop();
        if let Some(path) = &self.socket {
            let _ = UnixStream::connect(path);
        }
    }
}

/// Write one event to the daemon's log, or drop it when the state is gone.
///
/// Every diagnostic this file records is one the connection can proceed without,
/// and a poisoned mutex means a handler panicked while holding it: the daemon
/// answers `Io` to every request from then on, so an event saying so has nowhere
/// useful to go anyway. Stating that policy once here rather than at each call
/// site is what keeps it a decision rather than six coincidences.
fn note(daemon: &Arc<Mutex<Daemon>>, event: impl FnOnce(&mut Daemon)) {
    if let Ok(mut daemon) = daemon.lock() {
        event(&mut daemon);
    }
}

/// Serve one connection.
fn serve(
    stream: UnixStream,
    daemon: Arc<Mutex<Daemon>>,
    hub: Arc<Hub>,
    alarm: Arc<Alarm>,
    heartbeat: Duration,
) {
    let peer = match peer_credentials(&stream) {
        Ok(peer) => peer,
        Err(reason) => {
            reject(&stream, IpcError::PeerRejected { reason }, &daemon);
            return;
        }
    };

    // The socket lives in a directory only this user can enter, and the
    // credential check makes that a guarantee rather than a side effect.
    let own_uid = rustix::process::getuid().as_raw();
    if peer.uid != own_uid {
        // `reject` records the refusal, so it is not recorded again here: one
        // client turned away is one event, and the typed error says everything
        // the bare reason did.
        reject(
            &stream,
            IpcError::PeerRejected {
                reason: format!("peer uid {} does not own this daemon", peer.uid),
            },
            &daemon,
        );
        return;
    }

    // Registered before the first byte is read, so `status` lists a connection
    // that has not finished its handshake yet: it is open, and that is what
    // the list claims. A poisoned lock leaves it unregistered, and every
    // request on it is then answered `Io` anyway.
    let connection = match daemon.lock() {
        Ok(mut daemon) => daemon.open_connection(peer.uid, peer.pid),
        Err(_) => 0,
    };
    let mut registered = Registered {
        daemon: Arc::clone(&daemon),
        hub: Arc::clone(&hub),
        connection,
        detail: "client closed the connection".to_string(),
        framing_error: false,
    };

    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = BufWriter::new(write_half);
    let mut handshake_done = false;

    loop {
        let request: Request = match read_frame(&mut reader) {
            Ok(request) => request,
            Err(error) => {
                // A framing failure leaves the stream unusable: answer once
                // with the typed reason, then close. A clean end of stream is
                // not a failure and has nothing to answer.
                if let Some(ipc_error) = error.as_ipc_error() {
                    registered.detail = error.to_string();
                    registered.framing_error = true;
                    let _ = write_frame(&mut writer, &Response::Error(ipc_error));
                }
                return;
            }
        };

        // Nothing but the handshake is served before the handshake. Whether the
        // versions match is not asked here: the daemon has to answer that for
        // the typed request anyway, and asking it in both layers was one gate
        // implemented twice, where a divergence would have let this one win in
        // silence.
        let is_hello = matches!(request, Request::Hello { .. });
        if !handshake_done && !is_hello {
            let _ = write_frame(&mut writer, &Response::Error(IpcError::HandshakeRequired));
            return;
        }

        let after_seq = match &request {
            Request::Subscribe { after_seq, .. } => Some(*after_seq),
            _ => None,
        };
        let (response, latest_seq) = match daemon.lock() {
            Ok(mut daemon) => {
                let response = daemon.handle(connection, request);
                (response, daemon.latest_event_seq())
            }
            Err(_) => (
                Response::Error(IpcError::Io {
                    detail: "daemon state is unavailable".to_string(),
                }),
                0,
            ),
        };
        // A request may have recorded an event or changed the status, which a
        // subscriber should not have to wait for the next tick to see. It may
        // also have started an animation, whose next frame the ticker has to
        // learn about now rather than at its next telemetry pass.
        hub.notify();
        alarm.ring();

        if let (Response::Subscribed { topics, interval_s }, Some(after_seq)) =
            (&response, after_seq)
        {
            let plan = stream::Plan {
                topics: topics.clone(),
                interval_s: *interval_s,
                // The cursor is taken under the same lock that accepted the
                // subscription, so no event falls between the two.
                cursor: after_seq.unwrap_or(latest_seq),
                heartbeat,
            };
            registered.detail = if write_frame(&mut writer, &response).is_ok() {
                stream::serve(reader.get_ref(), &mut writer, &daemon, &hub, plan)
            } else {
                "the subscriber closed the connection".to_string()
            };
            note(&daemon, Daemon::unsubscribe);
            hub.notify();
            return;
        }

        // The daemon's answer is what settles a handshake: a `Hello` response
        // means it accepted the version and the name, and anything else is a
        // refusal this connection cannot recover from, so it closes once the
        // typed reason has been written. A second `Hello` is held to the same
        // rule, so a name refused mid-conversation ends it exactly as it would
        // have ended the first.
        let refused_handshake = is_hello && !matches!(response, Response::Hello { .. });
        if refused_handshake {
            registered.detail = "handshake refused".to_string();
        }
        if write_frame(&mut writer, &response).is_err() || refused_handshake {
            return;
        }
        handshake_done = true;
    }
}

/// A connection's place in the daemon's registry, released however `serve`
/// returns.
///
/// A guard rather than a call before each `return`: `serve` has five ways out,
/// and a connection left in the registry would sit in `status.clients` for the
/// life of the daemon.
struct Registered {
    daemon: Arc<Mutex<Daemon>>,
    hub: Arc<Hub>,
    connection: u64,
    /// Why the connection ended, as the `client_disconnected` event says it.
    detail: String,
    /// True when a frame the daemon could not read ended it.
    framing_error: bool,
}

impl Drop for Registered {
    fn drop(&mut self) {
        let detail = std::mem::take(&mut self.detail);
        let (connection, framing_error) = (self.connection, self.framing_error);
        note(&self.daemon, |daemon| {
            daemon.close_connection(connection, detail, framing_error)
        });
        self.hub.notify();
    }
}

struct Peer {
    uid: u32,
    pid: i32,
}

fn peer_credentials(stream: &UnixStream) -> Result<Peer, String> {
    let credentials = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|error| format!("peer credentials are unavailable: {error}"))?;
    Ok(Peer {
        uid: credentials.uid.as_raw(),
        pid: credentials.pid.as_raw_nonzero().get(),
    })
}

fn reject(stream: &UnixStream, error: IpcError, daemon: &Arc<Mutex<Daemon>>) {
    note(daemon, |daemon| {
        daemon.record_client_rejected(error.to_string())
    });
    if let Ok(write_half) = stream.try_clone() {
        let mut writer = BufWriter::new(write_half);
        let _ = write_frame(&mut writer, &Response::Error(error));
    }
}
