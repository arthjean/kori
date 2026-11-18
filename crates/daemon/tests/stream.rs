// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The pushed stream, from a real subscriber on a real socket.
//!
//! A subscriber is the one client that can hurt the daemon by doing nothing:
//! a connection that stops reading. These tests stall one on purpose and check
//! that the panel and the other subscribers do not notice.

mod common;

use std::io::{BufReader, BufWriter, Write};
use std::time::{Duration, Instant};

use kori_core::client::{Client, DEFAULT_TIMEOUT, Subscription};
use kori_core::diagnostics::EventKind;
use kori_core::display::DisplayMode;
use kori_core::ipc::{
    FrameError, IpcError, MAX_SUBSCRIPTIONS, PROTOCOL_VERSION, Push, Request, Response, Topic,
    read_frame, write_frame,
};
use kori_core::profile::CoolingProgram;
use kori_daemon::LcdBackend;
use kori_hardware_linux::testing::FakeKraken;

use common::{Harness, Machine, apply, preset};

const ALL: [Topic; 3] = [Topic::Telemetry, Topic::Status, Topic::Events];

fn subscribe(harness: &Harness, topics: &[Topic]) -> Subscription {
    harness
        .client()
        .subscribe(topics.to_vec(), None, None)
        .expect("subscribed")
}

/// Read pushes until `matches` accepts one, skipping the rest.
fn next_matching(
    subscription: &mut Subscription,
    limit: Duration,
    mut matches: impl FnMut(&Push) -> bool,
) -> Push {
    let deadline = Instant::now() + limit;
    loop {
        assert!(Instant::now() < deadline, "no matching push arrived");
        let push = subscription.next_push().expect("a push");
        if matches(&push) {
            return push;
        }
    }
}

/// A raw connection past its handshake, for the cases a typed client cannot
/// produce.
fn raw_handshake(
    harness: &Harness,
) -> (
    BufReader<std::os::unix::net::UnixStream>,
    BufWriter<std::os::unix::net::UnixStream>,
) {
    let stream = harness.raw();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
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
    (reader, writer)
}

#[test]
fn a_subscription_pushes_status_at_once_then_telemetry_every_tick() {
    let harness = Harness::start("stream-basic");
    let mut subscription = subscribe(&harness, &[Topic::Status, Topic::Telemetry, Topic::Status]);
    assert_eq!(subscription.topics(), &[Topic::Telemetry, Topic::Status]);
    assert_eq!(subscription.interval_s(), 1);

    let first = subscription.next_push().unwrap();
    assert!(matches!(first, Push::Status(_)), "{first:?}");
    let mut telemetry = 0;
    let started = Instant::now();
    while telemetry < 10 {
        if let Push::Telemetry(_) = subscription.next_push().unwrap() {
            telemetry += 1;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

#[test]
fn the_telemetry_interval_is_counted_in_ticks() {
    let harness = Harness::start("stream-interval");
    let fast = harness
        .client()
        .subscribe(vec![Topic::Telemetry], Some(1), None)
        .unwrap();
    let slow = harness
        .client()
        .subscribe(vec![Topic::Telemetry], Some(5), None)
        .unwrap();
    assert_eq!(slow.interval_s(), 5);
    let count = |mut subscription: Subscription| {
        let deadline = Instant::now() + common::FAST_INTERVAL * 30;
        let mut seen = 0;
        while Instant::now() < deadline {
            if let Ok(Push::Telemetry(_)) = subscription.next_push() {
                seen += 1;
            }
        }
        seen
    };
    let (fast, slow) = std::thread::scope(|scope| {
        let fast = scope.spawn(|| count(fast));
        let slow = scope.spawn(|| count(slow));
        (fast.join().unwrap(), slow.join().unwrap())
    });
    assert!(fast >= 15, "{fast}");
    assert!(slow * 3 <= fast, "every fifth tick: {slow} against {fast}");
    assert!(slow >= 2, "{slow}");
}

#[test]
fn events_and_status_changes_are_pushed_as_they_happen() {
    let harness = Harness::start("stream-events");
    let mut subscription = subscribe(&harness, &[Topic::Events, Topic::Status]);
    let Push::Status(initial) = subscription.next_push().unwrap() else {
        panic!("status comes first");
    };
    assert_eq!(initial.cooling.revision.revision, 1);

    let mut writer = Client::connect_as(harness.socket(), DEFAULT_TIMEOUT, Some("agent")).unwrap();
    apply(&mut writer, CoolingProgram::Fixed { pump: 180, fan: 90 });

    let Push::Event(event) = next_matching(
        &mut subscription,
        Duration::from_secs(5),
        |push| matches!(push, Push::Event(event) if matches!(event.kind, EventKind::ProgramApplied { .. })),
    ) else {
        unreachable!()
    };
    assert!(event.seq > 1);
    let Push::Status(changed) = next_matching(
        &mut subscription,
        Duration::from_secs(5),
        |push| matches!(push, Push::Status(status) if status.cooling.revision.revision == 2),
    ) else {
        unreachable!()
    };
    assert_eq!(
        changed.cooling.committed,
        Some(CoolingProgram::Fixed { pump: 180, fan: 90 })
    );
}

#[test]
fn a_quiet_subscription_is_kept_alive_by_heartbeats() {
    let harness = Harness::start_on(
        "stream-heartbeat",
        Machine {
            heartbeat: Duration::from_millis(200),
            ..Machine::default()
        },
        |_, _| {},
    );
    let mut subscription = subscribe(&harness, &[Topic::Events]);
    let started = Instant::now();
    let push = subscription.next_push().unwrap();
    assert!(
        matches!(push, Push::Heartbeat { at_unix_ms } if at_unix_ms > 0),
        "{push:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_shutdown_sends_its_events_then_closing_then_ends_the_stream() {
    let mut harness = Harness::start("stream-closing");
    let mut subscription = subscribe(&harness, &[Topic::Events]);
    std::thread::scope(|scope| {
        scope.spawn(|| harness.stop());
        let mut stopping = false;
        loop {
            match subscription.next_push().unwrap() {
                Push::Event(event) if matches!(event.kind, EventKind::DaemonStopping { .. }) => {
                    stopping = true;
                }
                Push::Closing { reason } => {
                    assert_eq!(reason, "shutdown requested");
                    break;
                }
                _ => {}
            }
        }
        assert!(stopping, "daemon_stopping is pushed before closing");
        assert!(matches!(
            subscription.next_push(),
            Err(kori_core::client::ClientError::Disconnected(
                FrameError::Closed
            ))
        ));
    });
}

#[test]
fn bytes_sent_after_subscribing_are_never_read() {
    let harness = Harness::start("stream-ignore");
    let (mut reader, mut writer) = raw_handshake(&harness);
    write_frame(
        &mut writer,
        &Request::Subscribe {
            topics: vec![Topic::Telemetry],
            interval_s: None,
            after_seq: None,
        },
    )
    .unwrap();
    let reply: Response = read_frame(&mut reader).unwrap();
    assert!(matches!(reply, Response::Subscribed { .. }), "{reply:?}");

    let before = harness.client().events(None, Some(1)).unwrap().latest_seq;
    writer
        .write_all(b"this is not json\n{\"request\":\"status\"}\n")
        .unwrap();
    writer.flush().unwrap();
    for _ in 0..5 {
        let push: Push = read_frame(&mut reader).unwrap();
        assert!(matches!(push, Push::Telemetry(_)), "{push:?}");
    }
    let after = harness.client().events(None, Some(1)).unwrap().latest_seq;
    assert_eq!(before, after, "the bytes were not parsed into anything");
}

#[test]
fn closing_the_connection_frees_the_subscription() {
    let harness = Harness::start("stream-unsubscribe");
    let subscriptions: Vec<_> = (0..MAX_SUBSCRIPTIONS)
        .map(|_| subscribe(&harness, &[Topic::Events]))
        .collect();
    drop(subscriptions);
    // Each slot is given back once its thread notices the close.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match harness.client().subscribe(vec![Topic::Events], None, None) {
            Ok(_) => break,
            Err(error) => assert!(Instant::now() < deadline, "still refused: {error}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_ninth_subscription_is_refused_and_the_connection_stays_usable() {
    let harness = Harness::start("stream-limit");
    let _held: Vec<_> = (0..MAX_SUBSCRIPTIONS)
        .map(|_| subscribe(&harness, &[Topic::Events]))
        .collect();
    let mut client = harness.client();
    let response = client
        .request(Request::Subscribe {
            topics: vec![Topic::Events],
            interval_s: None,
            after_seq: None,
        })
        .unwrap();
    assert_eq!(
        response,
        Response::Error(IpcError::SubscriptionLimit { max: 8 })
    );
    assert!(
        client.status().is_ok(),
        "the connection still serves requests"
    );
}

#[test]
fn a_cursor_older_than_the_ring_is_answered_with_lagged_first() {
    let harness = Harness::start("stream-lagged");
    let mut client = harness.client();
    for _ in 0..600 {
        client
            .request(Request::ActivateProfile {
                name: "Missing".into(),
            })
            .unwrap();
    }
    let mut subscription = harness
        .client()
        .subscribe(vec![Topic::Events], None, Some(0))
        .unwrap();
    // Read after subscribing: the subscription's own `client_accepted` is an
    // event too, and pushed one more out of the ring.
    let page = client.events(None, Some(1)).unwrap();
    let first = subscription.next_push().unwrap();
    assert_eq!(
        first,
        Push::Lagged {
            missed: page.oldest_seq - 1
        }
    );
    // Then the whole ring, in order, however many passes it takes.
    let mut expected = page.oldest_seq;
    while expected <= page.latest_seq {
        let Push::Event(event) = subscription.next_push().unwrap() else {
            panic!("events follow the notice");
        };
        assert_eq!(event.seq, expected);
        expected += 1;
    }
}

#[test]
fn an_empty_or_unknown_topic_list_or_a_bad_interval_is_malformed() {
    let harness = Harness::start("stream-malformed");
    for frame in [
        r#"{"request":"subscribe","topics":[]}"#,
        r#"{"request":"subscribe","topics":["voltage"]}"#,
        r#"{"request":"subscribe","topics":["events"],"interval_s":0}"#,
        r#"{"request":"subscribe","topics":["events"],"interval_s":61}"#,
    ] {
        let (mut reader, mut writer) = raw_handshake(&harness);
        writer.write_all(frame.as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
        let reply: Response = read_frame(&mut reader).unwrap();
        assert!(
            matches!(reply, Response::Error(IpcError::Malformed { .. })),
            "{frame}: {reply:?}"
        );
    }
}

#[test]
fn a_subscriber_is_announced_like_a_writer() {
    let harness = Harness::start("stream-announced");
    let client = Client::connect_as(harness.socket(), DEFAULT_TIMEOUT, Some("watcher")).unwrap();
    let connection = client.connection();
    let subscription = client.subscribe(vec![Topic::Events], None, None).unwrap();
    let events = harness.client().events(None, Some(512)).unwrap().events;
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ClientAccepted { connection: c, client: Some(name), .. }
            if *c == connection && name == "watcher"
    )));
    drop(subscription);
}

/// The subscriber that stops reading is the one that pays for it.
#[test]
fn a_stalled_subscriber_is_dropped_and_nobody_else_misses_a_tick() {
    let harness = Harness::start_on(
        "stream-stalled",
        Machine {
            lcd: LcdBackend::Link(Box::new(FakeKraken::new("2.0.0").link())),
            ..Machine::default()
        },
        |_, _| {},
    );
    let mut client = harness.client();
    client
        .apply_display(preset(DisplayMode::DualInfographic))
        .unwrap();

    // Never read. The daemon's writes pile up in the socket until it is full.
    let stalled_client = harness.client();
    let stalled_connection = stalled_client.connection();
    let stalled = stalled_client.subscribe(ALL.to_vec(), None, None).unwrap();
    let mut reading = subscribe(&harness, &[Topic::Telemetry]);

    let ticks_before = client.history(None, None, None, false).unwrap().available_s;
    let dropped_before = client.status().unwrap().display.dropped_frames;
    let started = Instant::now();
    let mut received = 0u32;
    let mut disconnected = None;
    while disconnected.is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the stalled subscriber was never dropped"
        );
        if let Push::Telemetry(_) = reading.next_push().unwrap() {
            received += 1;
        }
        if received.is_multiple_of(25) {
            disconnected = client
                .events(None, Some(512))
                .unwrap()
                .events
                .into_iter()
                .find_map(|event| match event.kind {
                    EventKind::ClientDisconnected {
                        connection, detail, ..
                    } if connection == stalled_connection => Some(detail),
                    _ => None,
                });
        }
    }
    let detail = disconnected.unwrap();
    assert!(detail.contains("within 2 s"), "{detail}");

    let status = client.status().unwrap();
    assert_eq!(
        status.display.dropped_frames - dropped_before,
        0,
        "the panel stream missed a tick while a subscriber was stalled"
    );
    assert!(status.display.frames_sent > 0);
    let ticks = client.history(None, None, None, false).unwrap().available_s - ticks_before;
    assert!(
        received * 10 >= ticks.min(900) * 8,
        "the reading subscriber got {received} pushes over {ticks} ticks"
    );
    drop(stalled);
}
