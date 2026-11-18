// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The read commands and the output contract, from the built binary to a
//! daemon serving a fake machine over a real socket.
//!
//! The harness is the daemon's own (`crates/daemon/tests/common`), included by
//! path rather than copied, so the machine these tests drive is the one every
//! daemon test drives.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use kori_core::client::Client;
use kori_core::ipc::{PROTOCOL_VERSION, Response};
use serde_json::{Value, json};

use common::{Harness, snapshot};
use support::{Stand, dead_socket, korictl, stand_in};

const READS: [(&str, &str); 5] = [
    ("status", "status"),
    ("telemetry", "telemetry"),
    ("capabilities", "capabilities"),
    ("profiles", "profiles"),
    ("diagnostics", "diagnostics"),
];

/// Each read prints the daemon's reply as one document, in the reply's own
/// shape, and exits 0. Decoding it back into the protocol's `Response` is what
/// proves the shape: a field renamed or dropped on the way would fail here.
#[test]
fn every_read_command_prints_the_daemon_reply_and_changes_nothing() {
    let harness = Harness::start("cli-reads");
    let hwmon = harness.hwmon_path();
    let before = snapshot(&hwmon);
    let config_before = snapshot(&harness.paths.config_dir);

    for (command, kind) in READS {
        let run = korictl(harness.socket(), &[command]);
        assert_eq!(run.code, 0, "{command}: {run:?}");
        assert!(run.stderr.is_empty(), "{command} wrote to stderr: {run:?}");
        let document = run.document();
        assert_eq!(document["response"], kind, "{command}");
        let reply: Response = serde_json::from_value(document).unwrap();
        assert_eq!(reply.kind(), kind);
    }

    // The two replies that hold no reading are compared whole with what the
    // library client receives on its own connection.
    // `clients` is the one field that differs by construction: each answer
    // lists the connection that asked for it, and these are two connections.
    let mut client = harness.client();
    let mut status = korictl(harness.socket(), &["status"]).document();
    let mut expected =
        serde_json::to_value(Response::Status(Box::new(client.status().unwrap()))).unwrap();
    let listed = status.as_object_mut().unwrap().remove("clients").unwrap();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["client"] == "korictl"),
        "{listed}"
    );
    expected.as_object_mut().unwrap().remove("clients");
    assert_eq!(status, expected);
    let profiles = korictl(harness.socket(), &["profiles"]).document();
    let (active, stored) = client.profiles().unwrap();
    assert_eq!(
        profiles,
        serde_json::to_value(Response::Profiles {
            active,
            profiles: stored
        })
        .unwrap()
    );

    assert_eq!(before, snapshot(&hwmon), "a read wrote to hwmon");
    assert_eq!(
        config_before,
        snapshot(&harness.paths.config_dir),
        "a read wrote the configuration"
    );
}

#[test]
fn pretty_output_is_the_same_document_indented() {
    let harness = Harness::start("cli-pretty");
    let compact = korictl(harness.socket(), &["profiles"]);
    let pretty = korictl(harness.socket(), &["--pretty", "profiles"]);
    assert_eq!(pretty.code, 0);
    assert!(pretty.stdout.lines().count() > 1, "{pretty:?}");
    let reparsed: Value = serde_json::from_str(&pretty.stdout).unwrap();
    assert_eq!(reparsed, compact.document());
}

/// `KORI_SOCKET` is how the window and the daemon agree on the socket, and the
/// command line honours it the same way; `--socket` overrides it.
#[test]
fn the_socket_comes_from_kori_socket_unless_an_option_names_one() {
    let harness = Harness::start("cli-socket");
    let socket = harness.socket().display().to_string();

    let from_environment = korictl(harness.socket(), &["status"]);
    assert_eq!(from_environment.code, 0, "{from_environment:?}");

    let dead = dead_socket("override");
    let from_option = korictl(&dead, &["--socket", &socket, "status"]);
    assert_eq!(from_option.code, 0, "{from_option:?}");
    let inline = korictl(&dead, &[&format!("--socket={socket}"), "status"]);
    assert_eq!(inline.code, 0, "{inline:?}");

    let overridden = korictl(
        harness.socket(),
        &["--socket", &dead.display().to_string(), "status"],
    );
    assert_eq!(overridden.code, 3, "{overridden:?}");
}

#[test]
fn version_reports_both_sides_when_the_daemon_answers() {
    let harness = Harness::start("cli-version");
    let run = korictl(harness.socket(), &["version"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        run.document(),
        json!({
            "korictl": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "daemon_version": kori_daemon::DAEMON_VERSION,
        })
    );
}

#[test]
fn version_still_succeeds_with_no_daemon() {
    let run = korictl(&dead_socket("version"), &["version"]);
    assert_eq!(run.code, 0, "{run:?}");
    let document = run.document();
    assert_eq!(document["daemon_version"], Value::Null);
    assert_eq!(document["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(document["korictl"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn every_read_command_reports_an_unreachable_daemon_with_exit_3() {
    let dead = dead_socket("unreachable");
    for (command, _) in READS {
        let run = korictl(&dead, &[command]);
        assert_eq!(run.code, 3, "{command}: {run:?}");
        let document = run.document();
        assert_eq!(document["error"], "unreachable", "{command}");
        assert_eq!(document["path"], dead.display().to_string(), "{command}");
        assert!(run.stderr.contains("korid"), "{command}: {run:?}");
    }
}

#[test]
fn a_daemon_of_another_protocol_is_reported_with_its_own_refusal_and_exit_4() {
    let socket = stand_in("protocol", Stand::OtherProtocol);
    let run = korictl(&socket, &["status"]);
    assert_eq!(run.code, 4, "{run:?}");
    assert_eq!(
        run.document(),
        json!({
            "error": "unsupported_protocol",
            "requested": PROTOCOL_VERSION,
            "supported": PROTOCOL_VERSION + 1,
        })
    );
}

#[test]
fn a_daemon_that_stops_answering_is_a_timeout_with_exit_5() {
    let socket = stand_in("silent", Stand::Silent);
    let started = std::time::Instant::now();
    let run = korictl(&socket, &["--timeout", "0.3", "telemetry"]);
    assert_eq!(run.code, 5, "{run:?}");
    let document = run.document();
    assert_eq!(document["error"], "timeout");
    assert_eq!(document["timeout_s"], 0.3);
    // The option bounds the wait: the stand-in holds the connection for 3 s.
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn a_daemon_that_hangs_up_mid_command_is_a_disconnection_with_exit_5() {
    let socket = stand_in("hang-up", Stand::HangUp);
    let run = korictl(&socket, &["status"]);
    assert_eq!(run.code, 5, "{run:?}");
    assert_eq!(run.document()["error"], "disconnected");
}

#[test]
fn a_usage_error_is_json_on_stdout_with_the_token_named_on_stderr() {
    let harness = Harness::start("cli-usage");
    let before = snapshot(&harness.hwmon_path());
    for (line, token) in [
        (&["frobnicate"][..], "frobnicate"),
        (&["--verbose", "status"][..], "--verbose"),
        (&["status", "--pretty"][..], "--pretty"),
        (&["set", "fans", "{}"][..], "fans"),
    ] {
        let run = korictl(harness.socket(), line);
        assert_eq!(run.code, 2, "{line:?}: {run:?}");
        let document = run.document();
        assert_eq!(document["error"], "usage", "{line:?}");
        assert_eq!(document["token"], token, "{line:?}");
        assert!(run.stderr.contains(&format!("'{token}'")), "{run:?}");
    }
    assert_eq!(before, snapshot(&harness.hwmon_path()));
}

/// The library change behind the option: a caller-chosen bound reaches the
/// socket. The window keeps calling `connect`, which is the same five seconds
/// it always used.
#[test]
fn the_library_client_connects_with_a_caller_chosen_timeout() {
    let harness = Harness::start("cli-timeout-library");
    let mut client =
        Client::connect_with_timeout(harness.socket(), std::time::Duration::from_millis(750))
            .unwrap();
    assert_eq!(client.daemon_version(), kori_daemon::DAEMON_VERSION);
    let export = client.diagnostics().unwrap();
    assert_eq!(export.daemon_version, kori_daemon::DAEMON_VERSION);
}
