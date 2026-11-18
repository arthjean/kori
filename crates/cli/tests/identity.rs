// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The name `korictl` gives itself, and where it comes from.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use serde_json::Value;

use common::Harness;
use support::{korictl, korictl_env};

/// The `client` of the one `program_applied` event in the log.
fn program_writer(harness: &Harness) -> Value {
    let run = korictl(harness.socket(), &["diagnostics"]);
    assert_eq!(run.code, 0, "{run:?}");
    let document = run.document();
    let mut writers = document["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "program_applied")
        .map(|event| event["by"].clone());
    let by = writers.next().expect("one write was made");
    assert_eq!(by["origin"], "client");
    assert!(by["pid"].as_i64().unwrap() > 0, "{by}");
    by["client"].clone()
}

const PROGRAM: &str = r#"{"mode":"fixed","pump":180,"fan":90}"#;

#[test]
fn korictl_names_itself_korictl_by_default() {
    let harness = Harness::start("cli-name-default");
    let run = korictl(harness.socket(), &["set", "cooling", PROGRAM]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(program_writer(&harness), "korictl");
}

#[test]
fn the_environment_names_the_client_and_the_option_overrides_it() {
    let harness = Harness::start("cli-name-env");
    let run = korictl_env(
        harness.socket(),
        &["set", "cooling", PROGRAM],
        &[("KORI_CLIENT", "claude-code")],
    );
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(program_writer(&harness), "claude-code");

    let harness = Harness::start("cli-name-option");
    let run = korictl_env(
        harness.socket(),
        &["--client", "agent.2", "set", "cooling", PROGRAM],
        &[("KORI_CLIENT", "claude-code")],
    );
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(program_writer(&harness), "agent.2");
}

/// The connection running `status` sees itself in the list, under its name.
#[test]
fn status_lists_the_asking_connection_under_its_name() {
    let harness = Harness::start("cli-name-status");
    let run = korictl(harness.socket(), &["--client", "probe", "status"]);
    assert_eq!(run.code, 0, "{run:?}");
    let document = run.document();
    let clients = document["clients"].as_array().unwrap();
    assert!(
        clients
            .iter()
            .any(|entry| entry["client"] == "probe" && entry["pid"].as_i64().unwrap() > 0),
        "{clients:?}"
    );
    assert!(document["started_at_unix_ms"].as_u64().unwrap() > 0);
}

/// `korictl` judges no name. The daemon refuses it, and the refusal is printed
/// as the daemon sent it.
#[test]
fn a_name_the_daemon_refuses_is_printed_as_its_refusal() {
    let harness = Harness::start("cli-name-refused");
    let run = korictl(harness.socket(), &["--client", "Not Valid", "status"]);
    assert_eq!(run.code, 1, "{run:?}");
    assert_eq!(run.document()["error"], "malformed", "{run:?}");
}
