// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the window's tests drive: the daemon's own fake machine, and a socket
//! proxy that sees every frame the window sends.
//!
//! The proxy sits between the window and a real daemon and forwards both ways
//! untouched. What it adds is evidence the daemon does not keep: how many
//! frames each connection sent and what they said, so a test can prove the
//! worker stopped polling, that a write carried its revision, or that an
//! abandoned edit was never sent twice. It can also hold back what the daemon
//! says, which is how a daemon that stops answering is staged, and slip a push
//! of its own onto a subscription, which is how a slow reader's `lagged` is.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "../../daemon/tests/common/mod.rs"]
pub(crate) mod harness;

/// One connection through the proxy.
struct Link {
    /// What the window sent, one decoded frame per line.
    sent: Vec<serde_json::Value>,
    /// The window's end, for frames the proxy adds.
    client: Arc<Mutex<UnixStream>>,
}

#[derive(Default)]
struct State {
    links: Vec<Link>,
}

/// A forwarding socket in front of a daemon.
pub(crate) struct Proxy {
    path: PathBuf,
    target: Arc<Mutex<PathBuf>>,
    state: Arc<Mutex<State>>,
    frozen: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl Proxy {
    pub(crate) fn start(name: &str, target: &Path) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "kori-app-proxy-{name}-{}-{}.sock",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = Self {
            path,
            target: Arc::new(Mutex::new(target.to_path_buf())),
            state: Arc::default(),
            frozen: Arc::default(),
            closed: Arc::default(),
        };
        let (target, state, frozen, closed) = (
            Arc::clone(&proxy.target),
            Arc::clone(&proxy.state),
            Arc::clone(&proxy.frozen),
            Arc::clone(&proxy.closed),
        );
        std::thread::spawn(move || {
            while !closed.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        client.set_nonblocking(false).unwrap();
                        let upstream = target.lock().unwrap().clone();
                        let Ok(daemon) = UnixStream::connect(&upstream) else {
                            continue;
                        };
                        forward(client, daemon, &state, &frozen);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        proxy
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Send new connections to another daemon.
    pub(crate) fn retarget(&self, target: &Path) {
        *self.target.lock().unwrap() = target.to_path_buf();
    }

    /// Stop passing on anything the daemon says, as a hung daemon would.
    pub(crate) fn freeze(&self) {
        self.frozen.store(true, Ordering::SeqCst);
    }

    /// Every frame connection `index` sent, in order.
    pub(crate) fn sent(&self, index: usize) -> Vec<serde_json::Value> {
        self.state
            .lock()
            .unwrap()
            .links
            .get(index)
            .map(|link| link.sent.clone())
            .unwrap_or_default()
    }

    pub(crate) fn connections(&self) -> usize {
        self.state.lock().unwrap().links.len()
    }

    /// Every frame of every connection whose `request` tag is `kind`.
    pub(crate) fn requests(&self, kind: &str) -> Vec<serde_json::Value> {
        self.state
            .lock()
            .unwrap()
            .links
            .iter()
            .flat_map(|link| link.sent.iter())
            .filter(|frame| frame["request"] == kind)
            .cloned()
            .collect()
    }

    /// Write one line to the window on connection `index`, as if the daemon
    /// had sent it.
    pub(crate) fn inject(&self, index: usize, line: &str) {
        let client = Arc::clone(&self.state.lock().unwrap().links[index].client);
        let mut client = client.lock().unwrap();
        client.write_all(line.as_bytes()).unwrap();
        client.write_all(b"\n").unwrap();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn forward(
    client: UnixStream,
    daemon: UnixStream,
    state: &Arc<Mutex<State>>,
    frozen: &Arc<AtomicBool>,
) {
    let client_writer = Arc::new(Mutex::new(client.try_clone().unwrap()));
    let index = {
        let mut state = state.lock().unwrap();
        state.links.push(Link {
            sent: Vec::new(),
            client: Arc::clone(&client_writer),
        });
        state.links.len() - 1
    };

    // The window's frames, recorded and passed on.
    let mut upstream = daemon.try_clone().unwrap();
    let up_state = Arc::clone(state);
    let up_client = client.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(up_client);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(frame) = serde_json::from_str(&line) {
                        up_state.lock().unwrap().links[index].sent.push(frame);
                    }
                    if upstream.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = upstream.shutdown(std::net::Shutdown::Both);
    });

    // The daemon's frames, passed on unless frozen.
    let frozen = Arc::clone(frozen);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(daemon);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if frozen.load(Ordering::SeqCst) {
                        continue;
                    }
                    if client_writer
                        .lock()
                        .unwrap()
                        .write_all(line.as_bytes())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        if !frozen.load(Ordering::SeqCst) {
            let _ = client.shutdown(std::net::Shutdown::Both);
        }
    });
}

/// Wait in real time until `condition` holds, or fail naming `what`.
pub(crate) fn wait_until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
