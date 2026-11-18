// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! `korictl watch`, the built binary reading a real subscription.
//!
//! What a caller holds is a pipe: lines arrive one at a time, the process ends
//! on a bound, a closed reader or a daemon that left, and the exit code says
//! which.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use common::Harness;
use support::korictl;

/// Start `korictl` without waiting for it, stdout piped.
fn spawn(socket: &std::path::Path, arguments: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_korictl"))
        .args(arguments)
        .env("KORI_SOCKET", socket)
        .env_remove("KORI_RUNTIME_DIR")
        .env_remove("KORI_CLIENT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("korictl starts")
}

/// Wait for the child to exit, at most `limit`.
fn exit_code(child: &mut Child, limit: Duration) -> i32 {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code().expect("exited rather than killed");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("korictl did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

#[test]
fn watch_prints_one_push_per_line_and_stops_at_the_count() {
    let harness = Harness::start("cli-watch-count");
    let run = korictl(
        harness.socket(),
        &["watch", "--topic", "telemetry", "--count", "3"],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let pushes = lines(&run.stdout);
    assert_eq!(pushes.len(), 3, "{run:?}");
    for push in &pushes {
        assert_eq!(push["push"], "telemetry");
        assert!(push["kraken"].is_object());
    }

    // Every topic by default: the status comes first.
    let run = korictl(harness.socket(), &["watch", "--count", "1"]);
    assert_eq!(run.code, 0, "{run:?}");
    let first = &lines(&run.stdout)[0];
    assert!(
        first["push"] == "status" || first["push"] == "telemetry",
        "{first}"
    );
}

#[test]
fn watch_stops_after_its_duration() {
    let harness = Harness::start("cli-watch-for");
    let started = Instant::now();
    let run = korictl(
        harness.socket(),
        &["watch", "--topic", "status", "--for", "1s"],
    );
    let elapsed = started.elapsed();
    assert_eq!(run.code, 0, "{run:?}");
    assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    assert!(!lines(&run.stdout).is_empty());
}

/// `korictl watch | head -1`: the reader takes a line and goes away.
#[test]
fn watch_exits_quietly_when_its_reader_closes_the_pipe() {
    let harness = Harness::start("cli-watch-head");
    let mut child = spawn(harness.socket(), &["watch", "--topic", "telemetry"]);
    {
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        assert!(line.contains("\"push\":\"telemetry\""), "{line}");
        // The read end is dropped here, as `head` drops it.
    }
    assert_eq!(exit_code(&mut child, Duration::from_secs(5)), 0);
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.is_empty(), "no broken-pipe message: {stderr}");

    // The daemon gave the subscription back.
    let mut client = harness.client();
    let status = harness.wait_for_status(&mut client, Duration::from_secs(3), |status| {
        status.clients.len() == 1
    });
    assert_eq!(status.clients.len(), 1, "{:?}", status.clients);
}

#[test]
fn watch_prints_closing_and_exits_5_when_the_daemon_stops() {
    let mut harness = Harness::start("cli-watch-closing");
    let mut child = spawn(harness.socket(), &["watch", "--topic", "events"]);
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
    });
    // Let the subscription settle before the daemon goes.
    let mut client = harness.client();
    harness.wait_for_status(&mut client, Duration::from_secs(3), |status| {
        status.clients.len() == 2
    });
    drop(client);
    harness.stop();

    assert_eq!(exit_code(&mut child, Duration::from_secs(5)), 5);
    let printed = reader.join().unwrap();
    let last: Value = serde_json::from_str(printed.last().unwrap()).unwrap();
    assert_eq!(last["push"], "closing", "{printed:?}");
    assert!(
        printed
            .iter()
            .any(|line| line.contains("\"daemon_stopping\"")),
        "{printed:?}"
    );
}

#[test]
fn a_topic_or_interval_the_daemon_refuses_is_printed_as_its_refusal() {
    let harness = Harness::start("cli-watch-refused");
    let run = korictl(harness.socket(), &["watch", "--topic", "voltage"]);
    assert_eq!(run.code, 1, "{run:?}");
    assert_eq!(run.document()["error"], "malformed");

    let run = korictl(harness.socket(), &["watch", "--interval", "2m"]);
    assert_eq!(run.code, 1, "{run:?}");
    assert_eq!(run.document()["error"], "malformed");

    let run = korictl(harness.socket(), &["watch", "--for", "soon"]);
    assert_eq!(run.code, 2, "{run:?}");
}
