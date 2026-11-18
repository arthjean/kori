// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Who a connection is, and who made each change.
//!
//! The name a client gives in its handshake is a label: the daemon checks its
//! shape, never its truth. What the kernel vouches for is the process ID, so
//! every attribution carries both, and these tests read them back from the
//! events and from `status` exactly as another client would.

mod common;

use std::io::{BufReader, BufWriter, Write};
use std::time::Duration;

use kori_core::client::{Client, DEFAULT_TIMEOUT};
use kori_core::diagnostics::{DiagnosticEvent, EventKind};
use kori_core::display::DisplayMode;
use kori_core::ipc::{
    Attribution, FrameError, IpcError, PROTOCOL_VERSION, Request, Response, read_frame,
};
use kori_core::profile::CoolingProgram;
use kori_daemon::state::Daemon;
use kori_daemon::{LcdBackend, Paths, RgbBackend};
use kori_hardware_linux::SysfsRoot;
use kori_hardware_linux::testing::FakeKraken;

use common::{FAST_INTERVAL, Harness, apply, fixed, lighting, preset};

fn named(harness: &Harness, name: &str) -> Client {
    Client::connect_as(harness.socket(), DEFAULT_TIMEOUT, Some(name)).expect("named handshake")
}

/// The attribution a write from `client` must carry.
fn by(client: &Client, name: &str) -> Attribution {
    Attribution::Client {
        connection: client.connection(),
        client: Some(name.to_string()),
        pid: std::process::id() as i32,
    }
}

fn events(client: &mut Client) -> Vec<DiagnosticEvent> {
    client.diagnostics().unwrap().events
}

/// Send one raw handshake line and read the answer, then report whether the
/// daemon closed the connection afterwards.
fn raw_hello(harness: &Harness, line: &str) -> (Response, bool) {
    let stream = harness.raw();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut writer = BufWriter::new(stream.try_clone().unwrap());
    let mut reader = BufReader::new(stream);
    writer.write_all(line.as_bytes()).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
    let response: Response = read_frame(&mut reader).unwrap();
    // A closed connection reads as end of stream; an open one would wait for
    // the next request and time out instead.
    let closed = matches!(
        read_frame::<_, Response>(&mut reader),
        Err(FrameError::Closed)
    );
    (response, closed)
}

#[test]
fn a_named_connection_is_numbered_and_listed_in_status() {
    let harness = Harness::start("identity-status");
    let first = named(&harness, "agent-1");
    let mut second = named(&harness, "kori");
    assert!(first.connection() >= 1);
    assert!(
        second.connection() > first.connection(),
        "connection numbers are unique and never reused"
    );

    let status = second.status().unwrap();
    assert!(status.started_at_unix_ms > 0);
    assert!(status.started_at_unix_ms <= common::now_unix_ms());
    let listed = |number: u64| {
        status
            .clients
            .iter()
            .find(|entry| entry.connection == number)
            .unwrap_or_else(|| panic!("connection {number} missing from {:?}", status.clients))
    };
    let agent = listed(first.connection());
    assert_eq!(agent.client.as_deref(), Some("agent-1"));
    assert_eq!(agent.pid, std::process::id() as i32);
    assert!(agent.connected_at_unix_ms >= status.started_at_unix_ms);
    assert_eq!(
        listed(second.connection()).client.as_deref(),
        Some("kori"),
        "the asking connection is listed too"
    );

    // A connection that closed leaves the list.
    let gone = first.connection();
    drop(first);
    let status = harness.wait_for_status(&mut second, Duration::from_secs(2), |status| {
        status.clients.iter().all(|entry| entry.connection != gone)
    });
    assert!(status.clients.iter().all(|entry| entry.connection != gone));

    // An anonymous connection is listed without a name.
    let anonymous = harness.client();
    let status = second.status().unwrap();
    let entry = status
        .clients
        .iter()
        .find(|entry| entry.connection == anonymous.connection())
        .unwrap();
    assert_eq!(entry.client, None);
}

#[test]
fn a_client_name_outside_the_rule_is_malformed_and_closes_the_connection() {
    let harness = Harness::start("identity-refused");
    let long = "a".repeat(33);
    for name in ["", long.as_str(), "Agent", "two words", "slash/name"] {
        let line = serde_json::json!({
            "request": "hello",
            "protocol_version": PROTOCOL_VERSION,
            "client": name,
        })
        .to_string();
        let (response, closed) = raw_hello(&harness, &line);
        assert!(
            matches!(response, Response::Error(IpcError::Malformed { .. })),
            "{name:?}: {response:?}"
        );
        assert!(closed, "{name:?}: the connection must be closed");
    }

    // The limit itself is accepted.
    let line = serde_json::json!({
        "request": "hello",
        "protocol_version": PROTOCOL_VERSION,
        "client": "a".repeat(32),
    })
    .to_string();
    let (response, closed) = raw_hello(&harness, &line);
    assert!(matches!(response, Response::Hello { .. }), "{response:?}");
    assert!(!closed);
}

#[test]
fn a_protocol_9_client_is_told_both_versions() {
    let harness = Harness::start("identity-protocol-9");
    let (response, closed) = raw_hello(&harness, r#"{"request":"hello","protocol_version":9}"#);
    assert_eq!(
        response,
        Response::Error(IpcError::UnsupportedProtocol {
            requested: 9,
            supported: 10,
        })
    );
    assert!(closed);
}

#[test]
fn a_cooling_write_and_the_profile_events_name_who_made_them() {
    let harness = Harness::start("identity-cooling");
    let mut agent = named(&harness, "agent");
    let expected = by(&agent, "agent");

    apply(&mut agent, CoolingProgram::Fixed { pump: 180, fan: 90 });
    agent
        .request(Request::SaveProfile {
            profile: fixed("Silent", 120, 80),
        })
        .unwrap();
    agent
        .request(Request::ActivateProfile {
            name: "Silent".into(),
        })
        .unwrap();
    agent
        .request(Request::SaveProfile {
            profile: fixed("Spare", 120, 80),
        })
        .unwrap();
    agent
        .request(Request::DeleteProfile {
            name: "Spare".into(),
        })
        .unwrap();

    let log = events(&mut agent);
    let mut seen = Vec::new();
    for event in &log {
        let found = match &event.kind {
            EventKind::ProgramApplied { by, .. } => Some(("program_applied", by)),
            EventKind::ProfileSaved { by, .. } => Some(("profile_saved", by)),
            EventKind::ProfileActivated { by, .. } => Some(("profile_activated", by)),
            EventKind::ProfileDeleted { by, .. } => Some(("profile_deleted", by)),
            _ => None,
        };
        if let Some((kind, attribution)) = found {
            assert_eq!(attribution, &expected, "{kind}");
            seen.push(kind);
        }
    }
    for kind in [
        "program_applied",
        "profile_saved",
        "profile_activated",
        "profile_deleted",
    ] {
        assert!(seen.contains(&kind), "{kind} missing from {seen:?}");
    }

    // The attribution travels in the documented shape.
    let wire = serde_json::to_value(&log).unwrap();
    let program = wire
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "program_applied")
        .unwrap();
    assert_eq!(
        program["by"],
        serde_json::json!({
            "origin": "client",
            "connection": agent.connection(),
            "client": "agent",
            "pid": std::process::id(),
        })
    );
}

#[test]
fn a_lighting_write_names_who_made_it() {
    let harness = Harness::start_lit("identity-lighting", "1.5.0", 3);
    let mut agent = named(&harness, "agent");
    agent.apply_lighting(lighting(1, "ff8800")).unwrap();

    let attribution = events(&mut agent)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::LightingApplied { by, .. } => Some(by),
            _ => None,
        })
        .expect("a lighting event");
    assert_eq!(attribution, by(&agent, "agent"));
}

#[test]
fn a_panel_write_names_who_made_it_and_a_stream_frame_names_nobody() {
    let harness = Harness::start_lcd("identity-display", "2.0.0");
    let mut agent = named(&harness, "agent");
    agent
        .apply_display(preset(DisplayMode::DualInfographic))
        .unwrap();

    let attributions: Vec<_> = events(&mut agent)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::DisplayApplied { by, .. } => Some(by),
            _ => None,
        })
        .collect();
    assert_eq!(attributions.first(), Some(&Some(by(&agent, "agent"))));
    assert!(
        attributions.iter().skip(1).all(Option::is_none),
        "nothing after the command was caused by a client: {attributions:?}"
    );
}

/// What a start puts back is the daemon's doing, and says so.
#[test]
fn a_restore_at_startup_is_attributed_to_the_daemon() {
    let harness = Harness::start_lcd("identity-restore", "2.0.0");
    {
        let mut client = named(&harness, "agent");
        apply(&mut client, CoolingProgram::Fixed { pump: 170, fan: 80 });
        client
            .apply_display(preset(DisplayMode::SingleReading))
            .unwrap();
    }

    let mut restarted = Daemon::start_with(
        Paths::new(
            harness.paths.runtime_dir.join("second"),
            harness.paths.config_dir.clone(),
        ),
        &SysfsRoot::new(harness.fake.root_path()),
        &harness.proc_root,
        FAST_INTERVAL,
        RgbBackend::None,
        LcdBackend::Link(Box::new(FakeKraken::new("2.0.0").link())),
    )
    .unwrap();
    let Response::Diagnostics(export) = restarted.handle(0, Request::Diagnostics) else {
        panic!("diagnostics are served");
    };
    let program = export
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ProgramApplied { by, .. } => Some(by.clone()),
            _ => None,
        })
        .expect("the restore wrote the program");
    assert_eq!(program, Attribution::Daemon);
    let display = export
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::DisplayApplied { by, .. } => Some(by.clone()),
            _ => None,
        })
        .expect("the restore showed the picture");
    assert_eq!(display, Some(Attribution::Daemon));
}
