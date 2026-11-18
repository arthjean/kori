// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The event log as a client pages through it.
//!
//! The log is a ring of 512, so what it keeps is decided by what it records.
//! These tests drive the two things that used to flush it, a streaming panel
//! and a crowd of one-shot reads, and check that the startup survives both.

mod common;

use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kori_core::client::{Client, DEFAULT_TIMEOUT};
use kori_core::diagnostics::{DiagnosticEvent, EventKind};
use kori_core::display::{DisplayPreset, LcdMetric};
use kori_core::ipc::{
    HardwareState, IpcError, PROTOCOL_VERSION, Request, Response, read_frame, write_frame,
};
use kori_daemon::state::Daemon;
use kori_daemon::{LcdBackend, Paths, RgbBackend};
use kori_hardware_linux::SysfsRoot;
use kori_hardware_linux::testing::{FakeKraken, FakeSysfs};
use kori_hardware_linux::usbfs::UsbfsError;

use common::{FAST_INTERVAL, Harness, Machine};

/// A preset whose first reading is the coolant, which a test can move.
fn liquid_preset() -> DisplayPreset {
    let mut preset = DisplayPreset::default_infographic();
    preset.readings[0].metric = LcdMetric::LiquidTemperature;
    preset
}

fn all_events(client: &mut Client) -> Vec<DiagnosticEvent> {
    client.events(Some(0), Some(512)).unwrap().events
}

/// Move the coolant reading every few milliseconds until `stop` is set, so
/// each tick renders a different picture and the stream sends a frame.
fn wobble(fake: &FakeSysfs, hwmon: &std::path::Path, stop: &AtomicBool) {
    let mut step = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let millidegrees = 25_000 + (step % 400) * 37;
        fake.set_reading(hwmon, "temp1_input", &millidegrees.to_string());
        step += 1;
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn events_are_numbered_from_one_without_gaps() {
    let harness = Harness::start("events-seq");
    let mut client = harness.client();
    let page = client.events(None, Some(512)).unwrap();
    let first = page.events.first().unwrap();
    assert_eq!(first.seq, 1);
    assert!(matches!(first.kind, EventKind::DaemonStarted { .. }));
    for pair in page.events.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1);
    }
    assert_eq!(page.oldest_seq, 1);
    assert_eq!(page.latest_seq, page.events.last().unwrap().seq);
    assert_eq!(page.missed, 0);

    // Paging forward from a cursor returns what came after it, oldest first.
    client
        .request(Request::ActivateProfile {
            name: "Missing".into(),
        })
        .unwrap();
    let after = client.events(Some(page.latest_seq), None).unwrap();
    assert!(!after.events.is_empty());
    assert_eq!(after.events[0].seq, page.latest_seq + 1);
}

/// Ten minutes of a panel streaming at one frame a second used to be six
/// hundred events, which is more than the ring holds.
#[test]
fn ten_minutes_of_streaming_leave_the_startup_in_the_log() {
    let harness = Harness::start_on(
        "events-stream",
        Machine {
            interval: Duration::from_millis(5),
            lcd: LcdBackend::Link(Box::new(FakeKraken::new("2.0.0").link())),
            ..Machine::default()
        },
        |_, _| {},
    );
    let mut client = harness.client();
    client.apply_display(liquid_preset()).unwrap();

    let stop = AtomicBool::new(false);
    let status = std::thread::scope(|scope| {
        scope.spawn(|| wobble(&harness.fake, &harness.hwmon, &stop));
        let status = harness.wait_for_status(&mut client, Duration::from_secs(180), |status| {
            status.display.frames_sent >= 600
        });
        stop.store(true, Ordering::SeqCst);
        status
    });
    assert!(
        status.display.frames_sent >= 600,
        "the stream sent {} frames",
        status.display.frames_sent
    );
    assert!(status.display.last_frame_at_unix_ms.is_some());
    assert_eq!(status.display.failed_transfers, 0);

    let events = all_events(&mut client);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::DaemonStarted { .. })),
        "the startup was flushed out of the log"
    );
    let display = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::DisplayApplied { .. }))
        .count();
    assert_eq!(
        display, 1,
        "only the command is an event, not the frames after it"
    );
}

#[test]
fn two_hundred_one_shot_reads_add_no_event() {
    let harness = Harness::start("events-reads");
    let before = harness.client().events(None, Some(1)).unwrap().latest_seq;
    for _ in 0..200 {
        let mut client = harness.client();
        client.status().unwrap();
        client.telemetry().unwrap();
    }
    // The connection reading the log is itself a one-shot read.
    let after = harness.client().events(None, Some(1)).unwrap().latest_seq;
    assert_eq!(before, after, "a read is not an event");
}

#[test]
fn a_writer_is_announced_once_and_its_departure_recorded() {
    let harness = Harness::start("events-writer");
    let connection = {
        let mut writer =
            Client::connect_as(harness.socket(), DEFAULT_TIMEOUT, Some("agent")).unwrap();
        for _ in 0..3 {
            writer
                .request(Request::ActivateProfile {
                    name: "Missing".into(),
                })
                .unwrap();
        }
        writer.connection()
    };

    let mut reader = harness.client();
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        events = all_events(&mut reader);
        if events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ClientDisconnected { .. }))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let accepted: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ClientAccepted {
                connection, client, ..
            } => Some((*connection, client.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(accepted, vec![(connection, Some("agent".to_string()))]);
    let departed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ClientDisconnected { connection, .. } => Some(*connection),
            _ => None,
        })
        .collect();
    assert_eq!(departed, vec![connection]);
}

/// A frame the daemon could not read ends the connection, and that is
/// recorded whoever sent it.
#[test]
fn a_framing_error_is_recorded_even_for_a_reader() {
    let harness = Harness::start("events-framing");
    {
        let stream = harness.raw();
        let mut writer = BufWriter::new(stream.try_clone().unwrap());
        let mut reader = BufReader::new(stream);
        write_frame(
            &mut writer,
            &Request::Hello {
                protocol_version: PROTOCOL_VERSION,
                client: None,
            },
        )
        .unwrap();
        let _: Response = read_frame(&mut reader).unwrap();
        writer.write_all(b"not json\n").unwrap();
        writer.flush().unwrap();
        let answer: Response = read_frame(&mut reader).unwrap();
        assert!(matches!(
            answer,
            Response::Error(IpcError::Malformed { .. })
        ));
    }
    let mut client = harness.client();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let found = all_events(&mut client)
            .iter()
            .any(|event| matches!(event.kind, EventKind::ClientDisconnected { .. }));
        if found {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the framing error was not recorded"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_cursor_older_than_the_ring_reports_what_is_gone() {
    let harness = Harness::start("events-missed");
    let mut client = harness.client();
    // Each refusal is an event, so this overflows the 512-event ring.
    for _ in 0..600 {
        client
            .request(Request::ActivateProfile {
                name: "Missing".into(),
            })
            .unwrap();
    }
    let page = client.events(Some(0), Some(10)).unwrap();
    assert!(page.oldest_seq > 1);
    assert_eq!(page.missed, page.oldest_seq - 1);
    assert_eq!(page.events.len(), 10);
    assert_eq!(page.events[0].seq, page.oldest_seq);

    let Response::Error(IpcError::Malformed { detail }) = client
        .request(Request::Events {
            after_seq: None,
            limit: Some(513),
        })
        .unwrap()
    else {
        panic!("a limit past the ring is refused");
    };
    assert!(detail.contains("512"), "{detail}");
}

#[test]
fn the_orderly_shutdown_is_recorded() {
    let mut harness = Harness::start("events-stopping");
    harness.stop();
    let Response::Events(page) = harness.daemon.lock().unwrap().handle(
        0,
        Request::Events {
            after_seq: None,
            limit: Some(5),
        },
    ) else {
        panic!("events are served");
    };
    let last = page.events.last().unwrap();
    assert!(
        matches!(&last.kind, EventKind::DaemonStopping { reason } if reason == "shutdown requested"),
        "{last:?}"
    );
}

/// A stream that stops is one event per reason, and its end is one more.
///
/// Driven through the daemon's own clock entry point with instants chosen by
/// the test, because the retry waits are five seconds and more: the ticker
/// passes `Instant::now()` to exactly this function.
#[test]
fn a_stream_fault_is_recorded_once_per_reason_and_its_recovery_once() {
    let fake = FakeSysfs::new("events-fault");
    fake.add_kraken();
    let hwmon = fake.add_kraken_hwmon();
    fake.add_cpu_hwmon();
    let proc_root = fake.add_proc();
    let base = std::env::temp_dir().join(format!("kori-events-fault-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let kraken = FakeKraken::new("2.0.0");
    let bulk = kraken.bulk_recorder();
    let mut daemon = Daemon::start_with(
        Paths::new(base.join("run"), base.join("config")),
        &SysfsRoot::new(fake.root_path()),
        &proc_root,
        FAST_INTERVAL,
        RgbBackend::None,
        LcdBackend::Link(Box::new(kraken.link())),
    )
    .unwrap();
    let connection = daemon.open_connection(0, 1);
    let shown = daemon.handle(
        connection,
        Request::ApplyDisplay {
            preset: liquid_preset(),
            expect_revision: None,
        },
    );
    assert!(matches!(shown, Response::Shown(_)), "{shown:?}");

    let display_events = |daemon: &mut Daemon| -> Vec<(HardwareState, u32, bool)> {
        let Response::Events(page) = daemon.handle(
            0,
            Request::Events {
                after_seq: Some(0),
                limit: Some(512),
            },
        ) else {
            panic!("events are served");
        };
        page.events
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::DisplayApplied {
                    hardware,
                    frames,
                    by,
                    ..
                } => Some((hardware, frames, by.is_some())),
                _ => None,
            })
            .collect()
    };
    // A reading that moved, so the next tick has a new picture to send.
    let mut reading = 30_000;
    let mut move_reading = || {
        reading += 1_000;
        fake.set_reading(&hwmon, "temp1_input", &reading.to_string());
        std::thread::sleep(FAST_INTERVAL * 3);
    };

    let start = Instant::now();
    move_reading();
    daemon.tick(start);
    assert_eq!(
        display_events(&mut daemon).len(),
        1,
        "a landed frame is no event"
    );

    bulk.fail_with(UsbfsError::PermissionDenied {
        path: "/dev/bus/usb/001/009".into(),
    });
    move_reading();
    daemon.tick(start + Duration::from_secs(1));
    // Past the first wait: the retry fails for the same reason.
    move_reading();
    daemon.tick(start + Duration::from_secs(7));
    let status = daemon.status();
    assert_eq!(status.display.failed_transfers, 2);
    let events = display_events(&mut daemon);
    assert_eq!(events.len(), 2, "one fault, one event: {events:?}");
    assert!(matches!(events[1].0, HardwareState::Uncertain { .. }));
    assert!(!events[1].2, "no client caused the fault");

    bulk.recover();
    move_reading();
    // Past the doubled wait: the frame lands.
    daemon.tick(start + Duration::from_secs(30));
    move_reading();
    daemon.tick(start + Duration::from_secs(31));
    let events = display_events(&mut daemon);
    assert_eq!(events.len(), 3, "the recovery is one event: {events:?}");
    assert_eq!(events[2].0, HardwareState::Confirmed);
    assert_eq!(events[2].1, 1);
    assert!(!events[2].2);
    assert!(daemon.status().display.frames_sent >= 3);

    drop(daemon);
    let _ = std::fs::remove_dir_all(&base);
}
