// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The write commands, from the built binary to a daemon serving a fake machine.
//!
//! What is proven here is the division of labour: `korictl` carries the
//! payload and checks nothing but that it is JSON, and the daemon is the one
//! that judges it, writes it, refuses it, and reports what the hardware did.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use kori_core::display::DisplayPreset;
use kori_core::profile::{MIN_PUMP_DUTY, SAFE_PROFILE_NAME};
use serde_json::json;

use kori_daemon::state::Daemon;
use kori_daemon::{LcdBackend, RgbBackend, Server};
use kori_hardware_linux::SysfsRoot;

use common::{FAST_INTERVAL, Harness, read_attribute, snapshot};
use support::{korictl, korictl_with};

#[test]
fn set_cooling_applies_a_program_and_prints_the_hardware_outcome() {
    let harness = Harness::start("cli-set-cooling");
    let hwmon = harness.hwmon_path();

    let run = korictl(
        harness.socket(),
        &["set", "cooling", r#"{"mode":"fixed","pump":180,"fan":90}"#],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let document = run.document();
    assert_eq!(document["response"], "applied");
    assert_eq!(document["hardware"], json!({ "state": "confirmed" }));
    assert_eq!(document["writes"], 4);
    assert_eq!(read_attribute(&hwmon, "pwm1"), "180");
    assert_eq!(read_attribute(&hwmon, "pwm2"), "90");
}

#[test]
fn a_payload_of_dash_is_read_from_stdin() {
    let harness = Harness::start("cli-stdin");
    let hwmon = harness.hwmon_path();
    let run = korictl_with(
        harness.socket(),
        &["set", "cooling", "-"],
        Some("{\"mode\":\"fixed\",\"pump\":200,\"fan\":40}\n"),
    );
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(read_attribute(&hwmon, "pwm1"), "200");
}

/// The safety floor is the daemon's to enforce. `korictl` sends a pump duty
/// below it unchanged, and prints the daemon's typed refusal unchanged.
#[test]
fn a_pump_duty_under_the_floor_reaches_the_daemon_and_its_refusal_is_printed() {
    let harness = Harness::start("cli-floor");
    let before = snapshot(&harness.hwmon_path());
    let under = MIN_PUMP_DUTY - 1;

    let payload = format!(r#"{{"mode":"fixed","pump":{under},"fan":90}}"#);
    let run = korictl(harness.socket(), &["set", "cooling", &payload]);
    assert_eq!(run.code, 1, "{run:?}");
    let document = run.document();
    assert_eq!(document["error"], "validation", "{run:?}");
    assert!(document.get("response").is_none(), "{run:?}");
    assert!(run.stderr.contains("refused"), "{run:?}");
    assert_eq!(before, snapshot(&harness.hwmon_path()), "hwmon was written");
}

/// An unknown field is valid JSON, so it is the daemon that refuses it.
#[test]
fn an_unknown_field_is_refused_by_the_daemon_rather_than_by_the_client() {
    let harness = Harness::start("cli-unknown-field");
    let before = snapshot(&harness.hwmon_path());
    let run = korictl(
        harness.socket(),
        &[
            "profile",
            "save",
            r#"{"name":"x","program":{"mode":"onboard"},"device":null,"force_detach":true}"#,
        ],
    );
    assert_eq!(run.code, 1, "{run:?}");
    assert_eq!(run.document()["error"], "malformed");
    assert_eq!(before, snapshot(&harness.hwmon_path()));
}

#[test]
fn a_payload_that_is_not_json_sends_nothing_and_exits_2() {
    let harness = Harness::start("cli-invalid-payload");
    let before = snapshot(&harness.hwmon_path());
    for command in ["cooling", "lighting", "display"] {
        let run = korictl(harness.socket(), &["set", command, "{pump: 10"]);
        assert_eq!(run.code, 2, "{command}: {run:?}");
        assert_eq!(run.document()["error"], "invalid_payload");
    }
    let run = korictl_with(harness.socket(), &["profile", "save", "-"], Some("nope"));
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(run.document()["error"], "invalid_payload");
    assert_eq!(before, snapshot(&harness.hwmon_path()));
}

/// A daemon that came up read-only because another one holds the devices: the
/// refusal and its reason reach stdout unchanged and nothing is written.
#[test]
fn a_read_only_daemon_refuses_set_cooling_with_its_reason() {
    let owner = Harness::start("cli-read-only");
    let before = snapshot(&owner.hwmon_path());

    // A second daemon over the same runtime directory finds every device lock
    // taken, which is how a real one ends up read-only next to a first.
    let mut paths = owner.paths.clone();
    paths.socket = paths.runtime_dir.join("second.sock");
    let second = Daemon::start_with(
        paths.clone(),
        &SysfsRoot::new(owner.fake.root_path()),
        &owner.proc_root,
        FAST_INTERVAL,
        RgbBackend::None,
        LcdBackend::None,
    )
    .unwrap();
    let listener = kori_daemon::server::bind_socket(&paths.socket).unwrap();
    let server = Server::attach(listener, second);
    let shutdown = server.shutdown_handle();
    let thread = std::thread::spawn(move || server.run().unwrap());

    let run = korictl(
        &paths.socket,
        &["set", "cooling", r#"{"mode":"fixed","pump":180,"fan":90}"#],
    );
    shutdown.stop();
    thread.join().unwrap();

    assert_eq!(run.code, 1, "{run:?}");
    let document = run.document();
    assert_eq!(document["error"], "read_only", "{run:?}");
    let reason = document["reason"].as_str().unwrap();
    assert!(reason.contains("device lock"), "{reason}");
    assert!(run.stderr.contains(reason), "{run:?}");
    assert_eq!(
        before,
        snapshot(&owner.hwmon_path()),
        "a read-only daemon wrote"
    );
}

/// Without the udev rule nothing is writable, and the daemon says which
/// capability blocks the program rather than a bare "read-only".
#[test]
fn a_daemon_without_write_access_names_what_blocks_the_program() {
    let harness = Harness::start_read_only("cli-no-udev");
    let before = snapshot(&harness.hwmon_path());
    let run = korictl(
        harness.socket(),
        &["set", "cooling", r#"{"mode":"fixed","pump":180,"fan":90}"#],
    );
    assert_eq!(run.code, 1, "{run:?}");
    let document = run.document();
    assert_eq!(document["error"], "incompatible", "{run:?}");
    assert!(!document["details"].as_array().unwrap().is_empty());
    assert_eq!(before, snapshot(&harness.hwmon_path()));
}

#[test]
fn a_profile_is_saved_activated_and_deleted_from_the_command_line() {
    let harness = Harness::start("cli-profiles");
    let hwmon = harness.hwmon_path();
    let profile = json!({
        "name": "Quiet",
        "program": { "mode": "fixed", "pump": 150, "fan": 60 },
        "device": null,
    })
    .to_string();

    let saved = korictl(harness.socket(), &["profile", "save", &profile]);
    assert_eq!(saved.code, 0, "{saved:?}");
    assert_eq!(
        saved.document(),
        json!({ "response": "saved", "name": "Quiet" })
    );

    let activated = korictl(harness.socket(), &["profile", "activate", "Quiet"]);
    assert_eq!(activated.code, 0, "{activated:?}");
    let document = activated.document();
    assert_eq!(document["response"], "activated");
    assert_eq!(document["name"], "Quiet");
    assert_eq!(read_attribute(&hwmon, "pwm1"), "150");

    let deleted = korictl(harness.socket(), &["profile", "delete", "Quiet"]);
    assert_eq!(deleted.code, 0, "{deleted:?}");
    assert_eq!(
        deleted.document(),
        json!({
            "response": "deleted",
            "name": "Quiet",
            "activated_instead": SAFE_PROFILE_NAME,
        })
    );

    let missing = korictl(harness.socket(), &["profile", "activate", "Quiet"]);
    assert_eq!(missing.code, 1, "{missing:?}");
    assert_eq!(
        missing.document(),
        json!({ "error": "profile_not_found", "name": "Quiet" })
    );
}

/// The lighting and panel commands carry their payload to the executors that
/// own those writes. With neither device answering, the daemon's refusal is
/// what comes back, typed and unchanged, which is the case an agent meets on a
/// machine without the controller.
#[test]
fn set_lighting_and_set_display_reach_their_executors() {
    let harness = Harness::start("cli-absent-devices");
    let lighting = korictl(
        harness.socket(),
        &[
            "set",
            "lighting",
            r#"{"channel":1,"program":{"mode":"off"}}"#,
        ],
    );
    assert_eq!(lighting.code, 1, "{lighting:?}");
    assert!(lighting.document()["error"].is_string(), "{lighting:?}");

    let preset = serde_json::to_string(&DisplayPreset::default_infographic()).unwrap();
    let display = korictl(harness.socket(), &["set", "display", &preset]);
    assert_eq!(display.code, 1, "{display:?}");
    assert!(display.document()["error"].is_string(), "{display:?}");
}

/// On a panel that answers, `set display` is a full round trip: the preset
/// travels, the daemon renders and sends it, and the outcome says so.
#[test]
fn set_display_shows_a_preset_on_a_validated_panel() {
    for firmware in kori_hardware_linux::lcd::VALIDATED_FIRMWARE {
        let harness = Harness::start_lcd("cli-display", firmware);
        let preset = serde_json::to_string(&DisplayPreset::default_infographic()).unwrap();
        let run = korictl(harness.socket(), &["set", "display", &preset]);
        assert_eq!(run.code, 0, "{firmware}: {run:?}");
        let document = run.document();
        assert_eq!(document["response"], "shown");
        assert_eq!(document["hardware"], json!({ "state": "confirmed" }));
    }
}

/// The same for lighting, on every controller firmware a write probe validated.
#[test]
fn set_lighting_lights_a_channel_on_a_validated_controller() {
    for firmware in kori_hardware_linux::rgb::VALIDATED_FIRMWARE {
        let harness = Harness::start_lit("cli-lighting", firmware, 3);
        let run = korictl(
            harness.socket(),
            &[
                "set",
                "lighting",
                r##"{"channel":1,"program":{"mode":"fixed","color":{"r":51,"g":102,"b":255},"brightness":60}}"##,
            ],
        );
        assert_eq!(run.code, 0, "{firmware}: {run:?}");
        let document = run.document();
        assert_eq!(document["response"], "lit");
        assert_eq!(document["channel"], 1);
    }
}
