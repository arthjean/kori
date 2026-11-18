// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(dead_code)]

//! Running the built `korictl` binary, and the stand-in daemons for the
//! failures a real one cannot be made to produce on demand.
//!
//! The binary is spawned rather than called in-process, so every assertion is
//! about what a caller actually receives: the bytes on each stream, and the
//! exit status the kernel reports.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use kori_core::ipc::{PROTOCOL_VERSION, Response, write_frame};
use serde_json::Value;

/// What one invocation left behind.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// The one JSON document stdout must hold, and nothing else.
    pub fn document(&self) -> Value {
        let line = self
            .stdout
            .strip_suffix('\n')
            .unwrap_or_else(|| panic!("stdout does not end in a newline: {self:?}"));
        assert!(
            !line.contains('\n'),
            "more than one line on stdout: {self:?}"
        );
        serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {self:?}"))
    }
}

/// Run `korictl` with `KORI_SOCKET` pointing at `socket`.
pub fn korictl(socket: &Path, arguments: &[&str]) -> Run {
    korictl_with(socket, arguments, None)
}

/// The same, with `stdin` fed to the process.
pub fn korictl_with(socket: &Path, arguments: &[&str], stdin: Option<&str>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_korictl"));
    command
        .args(arguments)
        // Set explicitly, never inherited: a test that fell back to the
        // runtime directory would be talking to the machine's own daemon.
        .env("KORI_SOCKET", socket)
        .env_remove("KORI_RUNTIME_DIR")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("korictl starts");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    Run {
        code: output
            .status
            .code()
            .expect("korictl exited rather than being killed"),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// A socket path nothing listens on.
pub fn dead_socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "korictl-dead-{name}-{}-{}.sock",
        std::process::id(),
        unique()
    ))
}

fn unique() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// How a stand-in daemon behaves once a client connected.
#[derive(Clone, Copy)]
pub enum Stand {
    /// Refuses the handshake, as a daemon of another protocol version does.
    OtherProtocol,
    /// Completes the handshake, reads the request, and never answers.
    Silent,
    /// Completes the handshake, reads the request, and hangs up.
    HangUp,
}

/// A daemon that misbehaves in one specific way, serving one connection.
///
/// Returns the socket path. The listener thread outlives the test only as long
/// as the stalled connection it holds, which is bounded.
pub fn stand_in(name: &str, stand: Stand) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "korictl-stand-{name}-{}-{}.sock",
        std::process::id(),
        unique()
    ));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        serve(stream, stand);
    });
    path
}

fn serve(stream: UnixStream, stand: Stand) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    if let Stand::OtherProtocol = stand {
        let refusal = Response::Error(kori_core::ipc::IpcError::UnsupportedProtocol {
            requested: PROTOCOL_VERSION,
            supported: PROTOCOL_VERSION + 1,
        });
        let _ = write_frame(&mut writer, &refusal);
        return;
    }
    let hello = Response::Hello {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: "stand-in".into(),
    };
    let _ = write_frame(&mut writer, &hello);
    line.clear();
    let _ = reader.read_line(&mut line);
    if let Stand::Silent = stand {
        // Long enough for any timeout the tests pass, short enough not to
        // hold a test binary open.
        std::thread::sleep(Duration::from_secs(3));
    }
}
