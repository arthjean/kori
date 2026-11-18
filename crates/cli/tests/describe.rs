// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! `korictl describe`, from the built binary, with no daemon anywhere.
//!
//! What an agent does on a cold start: read the description, then run what it
//! lists. Both halves are exercised here against the binary itself, so a
//! command the description lists and the parser refuses fails this file.

mod support;

use serde_json::Value;
use support::{dead_socket, korictl, korictl_with};

#[test]
fn describe_exits_0_without_a_daemon_and_prints_one_document() {
    let run = korictl(&dead_socket("describe"), &["describe"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(run.stderr, "");
    let document = run.document();
    assert_eq!(document["tool"], "korictl");
    assert_eq!(
        document["protocol_version"],
        kori_core::ipc::PROTOCOL_VERSION
    );
    for section in ["commands", "exit_codes", "global_options", "environment"] {
        assert!(
            !document[section].as_array().unwrap().is_empty(),
            "{section}"
        );
    }
    assert!(
        !document["error_codes"]["daemon"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !document["error_codes"]["client"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let pretty = korictl(&dead_socket("describe-pretty"), &["--pretty", "describe"]);
    assert_eq!(pretty.code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&pretty.stdout).unwrap(),
        document
    );
}

/// Every listed command is one the binary parses: with no daemon, a read or a
/// write gets as far as connecting (exit 3), and the commands that need no
/// daemon succeed. A usage error (exit 2) would mean the description lists a
/// command line the parser refuses.
#[test]
fn every_described_command_is_accepted_by_the_parser() {
    let document = korictl(&dead_socket("describe-list"), &["describe"]).document();
    for entry in document["commands"].as_array().unwrap() {
        let command = entry["command"].as_str().unwrap();
        let mut line: Vec<&str> = command.split(' ').collect();
        let stdin = match entry["operand"]["kind"].as_str() {
            Some("payload") => {
                line.push("-");
                Some("{}")
            }
            Some("name") => {
                line.push("Quiet");
                None
            }
            _ => None,
        };
        if command == "watch" {
            line.extend(["--count", "1"]);
        }
        let run = korictl_with(&dead_socket("describe-each"), &line, stdin);
        let expected = match command {
            "version" | "schema" | "describe" => 0,
            _ => 3,
        };
        assert_eq!(run.code, expected, "{command}: {run:?}");
        // Payload operands point at a definition the schema really has.
        if let Some(pointer) = entry["operand"]["schema"].as_str() {
            let schema: Value =
                serde_json::from_str(&korictl(&dead_socket("describe-schema"), &["schema"]).stdout)
                    .unwrap();
            assert!(
                schema.pointer(&pointer.replacen('#', "", 1)).is_some(),
                "{command} points at {pointer}"
            );
        }
    }
}
