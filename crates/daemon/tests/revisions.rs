// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Write revisions and the precondition that refuses a stale write.
//!
//! Two clients that both read the state and both write it are the case this
//! exists for: without a precondition the second silently overwrites the
//! first, and with one it is told who got there first.

mod common;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use kori_core::client::{Client, DEFAULT_TIMEOUT};
use kori_core::display::DisplayMode;
use kori_core::ipc::{Attribution, IpcError, Request, Response, Revision, WriteDomain};
use kori_core::profile::{CoolingProgram, MIN_PROGRAM_INTERVAL_MS, Profile};
use kori_daemon::state::Daemon;
use kori_daemon::{LcdBackend, Paths, RgbBackend};
use kori_hardware_linux::SysfsRoot;
use kori_hardware_linux::testing::{FakeController, FakeKraken};

use common::{FAST_INTERVAL, Harness, Machine, lighting, preset, snapshot};

/// A machine with both devices answering.
fn full(name: &str) -> Harness {
    Harness::start_on(
        name,
        Machine {
            rgb: RgbBackend::Transport(Box::new(FakeController::new("1.5.0", 3))),
            lcd: LcdBackend::Link(Box::new(FakeKraken::new("2.0.0").link())),
            ..Machine::default()
        },
        |_, _| {},
    )
}

fn named(harness: &Harness, name: &str) -> Client {
    Client::connect_as(harness.socket(), DEFAULT_TIMEOUT, Some(name)).unwrap()
}

fn by(client: &Client, name: &str) -> Attribution {
    Attribution::Client {
        connection: client.connection(),
        client: Some(name.into()),
        pid: std::process::id() as i32,
    }
}

/// Wait out the cooling cadence floor, so the next program is written rather
/// than answered `not_applied`, which advances no revision.
fn past_the_cadence() {
    std::thread::sleep(Duration::from_millis(MIN_PROGRAM_INTERVAL_MS + 20));
}

fn apply_if(client: &mut Client, program: CoolingProgram, expect: Option<u64>) -> Response {
    client
        .request(Request::ApplyProgram {
            program,
            expect_revision: expect,
        })
        .unwrap()
}

fn lighting_revision(client: &mut Client, channel: u8) -> Revision {
    client
        .status()
        .unwrap()
        .lighting
        .into_iter()
        .find(|entry| entry.channel == channel)
        .unwrap()
        .revision
}

#[test]
fn every_domain_starts_at_revision_one_by_the_daemon() {
    let harness = full("revisions-initial");
    let status = harness.client().status().unwrap();
    assert_eq!(status.cooling.revision, Revision::initial());
    assert_eq!(status.display.revision, Revision::initial());
    assert_eq!(status.lighting.len(), 3);
    for channel in &status.lighting {
        assert_eq!(channel.revision, Revision::initial(), "{}", channel.channel);
    }

    // On the wire the revision sits beside what it describes.
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(wire["cooling"]["revision"], 1);
    assert_eq!(
        wire["cooling"]["changed_by"],
        serde_json::json!({"origin": "daemon"})
    );
    assert_eq!(wire["display"]["revision"], 1);
    assert_eq!(wire["lighting"][0]["revision"], 1);
}

#[test]
fn each_accepted_write_advances_its_own_domain_and_names_the_writer() {
    let harness = full("revisions-advance");
    let mut agent = named(&harness, "agent");

    assert!(matches!(
        apply_if(
            &mut agent,
            CoolingProgram::Fixed { pump: 180, fan: 90 },
            None
        ),
        Response::Applied(_)
    ));
    agent.apply_lighting(lighting(2, "ff8800")).unwrap();
    agent
        .apply_display(preset(DisplayMode::DualInfographic))
        .unwrap();

    let status = agent.status().unwrap();
    let expected = Revision {
        revision: 2,
        changed_by: by(&agent, "agent"),
    };
    assert_eq!(status.cooling.revision, expected);
    assert_eq!(status.display.revision, expected);
    assert_eq!(lighting_revision(&mut agent, 2), expected);
    assert_eq!(
        lighting_revision(&mut agent, 1),
        Revision::initial(),
        "each channel has its own revision"
    );

    // The stream keeps drawing the infographic on its own; that is not a write.
    std::thread::sleep(FAST_INTERVAL * 10);
    assert_eq!(agent.status().unwrap().display.revision.revision, 2);

    // A write with no precondition still wins and is attributed.
    let mut window = named(&harness, "kori");
    past_the_cadence();
    assert!(matches!(
        apply_if(
            &mut window,
            CoolingProgram::Fixed { pump: 200, fan: 90 },
            None
        ),
        Response::Applied(_)
    ));
    let cooling = window.status().unwrap().cooling;
    assert_eq!(cooling.revision.revision, 3);
    assert_eq!(cooling.revision.changed_by, by(&window, "kori"));
}

#[test]
fn a_stale_precondition_is_a_conflict_that_writes_nothing() {
    let harness = full("revisions-conflict");
    let mut window = named(&harness, "kori");
    let mut agent = named(&harness, "agent");
    apply_if(
        &mut window,
        CoolingProgram::Fixed { pump: 180, fan: 90 },
        Some(1),
    );

    let hwmon = harness.hwmon_path();
    let before_hwmon = snapshot(&hwmon);
    let config = harness.paths.config_dir.join("config.toml");
    let before_config = std::fs::read_to_string(&config).unwrap();

    let response = apply_if(
        &mut agent,
        CoolingProgram::Fixed { pump: 220, fan: 60 },
        Some(1),
    );
    match response {
        Response::Error(IpcError::Conflict {
            domain,
            channel,
            expected,
            current,
            changed_by,
        }) => {
            assert_eq!(domain, WriteDomain::Cooling);
            assert_eq!(channel, None);
            assert_eq!((expected, current), (1, 2));
            assert_eq!(changed_by, by(&window, "kori"));
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(before_hwmon, snapshot(&hwmon), "hwmon was written");
    assert_eq!(
        before_config,
        std::fs::read_to_string(&config).unwrap(),
        "config.toml was written"
    );

    // The same write with the current revision goes through.
    past_the_cadence();
    assert!(matches!(
        apply_if(
            &mut agent,
            CoolingProgram::Fixed { pump: 220, fan: 60 },
            Some(2)
        ),
        Response::Applied(_)
    ));

    // A lighting conflict names its channel; a panel conflict its domain.
    agent.apply_lighting(lighting(1, "00ff00")).unwrap();
    let response = agent
        .request(Request::ApplyLighting {
            command: lighting(1, "0000ff"),
            expect_revision: Some(1),
        })
        .unwrap();
    assert!(
        matches!(
            response,
            Response::Error(IpcError::Conflict {
                domain: WriteDomain::Lighting,
                channel: Some(1),
                expected: 1,
                current: 2,
                ..
            })
        ),
        "{response:?}"
    );
    let response = agent
        .request(Request::ApplyDisplay {
            preset: preset(DisplayMode::SingleReading),
            expect_revision: Some(7),
        })
        .unwrap();
    assert!(
        matches!(
            response,
            Response::Error(IpcError::Conflict {
                domain: WriteDomain::Display,
                current: 1,
                ..
            })
        ),
        "{response:?}"
    );
}

#[test]
fn two_writers_holding_the_same_revision_cannot_both_win() {
    let harness = full("revisions-race");
    let mut setup = harness.client();
    for pump in [150, 160, 170] {
        apply_if(&mut setup, CoolingProgram::Fixed { pump, fan: 90 }, None);
        past_the_cadence();
    }
    assert_eq!(setup.status().unwrap().cooling.revision.revision, 4);

    let barrier = Arc::new(Barrier::new(2));
    let writers: Vec<_> = [("first", 180u8), ("second", 190u8)]
        .into_iter()
        .map(|(name, pump)| {
            let socket = harness.socket().to_path_buf();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut client = Client::connect_as(&socket, DEFAULT_TIMEOUT, Some(name)).unwrap();
                barrier.wait();
                apply_if(
                    &mut client,
                    CoolingProgram::Fixed { pump, fan: 90 },
                    Some(4),
                )
            })
        })
        .collect();
    let responses: Vec<Response> = writers.into_iter().map(|w| w.join().unwrap()).collect();

    let won = responses
        .iter()
        .filter(|response| matches!(response, Response::Applied(_)))
        .count();
    let conflicts: Vec<_> = responses
        .iter()
        .filter_map(|response| match response {
            Response::Error(IpcError::Conflict { current, .. }) => Some(*current),
            _ => None,
        })
        .collect();
    assert_eq!(won, 1, "{responses:?}");
    let applied = responses.iter().find_map(|response| match response {
        Response::Applied(outcome) => Some(outcome.hardware.clone()),
        _ => None,
    });
    assert_eq!(applied, Some(kori_core::ipc::HardwareState::Confirmed));
    assert_eq!(conflicts, vec![5], "{responses:?}");
    assert_eq!(setup.status().unwrap().cooling.revision.revision, 5);
}

#[test]
fn activating_a_profile_advances_every_domain_it_changes() {
    let harness = full("revisions-profile");
    let mut agent = named(&harness, "agent");
    let profile = Profile {
        name: "Evening".into(),
        program: CoolingProgram::Fixed { pump: 170, fan: 80 },
        device: None,
        lighting: vec![lighting(3, "112233")],
        display: Some(preset(DisplayMode::SingleReading)),
    };
    agent.request(Request::SaveProfile { profile }).unwrap();
    let response = agent
        .request(Request::ActivateProfile {
            name: "Evening".into(),
        })
        .unwrap();
    assert!(matches!(response, Response::Activated(_)), "{response:?}");

    let status = agent.status().unwrap();
    assert_eq!(status.cooling.revision.revision, 2);
    assert_eq!(status.display.revision.revision, 2);
    assert_eq!(lighting_revision(&mut agent, 3).revision, 2);
    assert_eq!(
        lighting_revision(&mut agent, 1).revision,
        1,
        "untouched channel"
    );
    assert_eq!(status.cooling.revision.changed_by, by(&agent, "agent"));
}

/// Revisions live in memory: a fresh daemon starts every domain at 1 again,
/// whatever it restored, and its start time tells a client the numbers it
/// held are void.
#[test]
fn a_restart_starts_every_revision_at_one() {
    let harness = Harness::start("revisions-restart");
    let before = {
        let mut client = harness.client();
        for pump in [150, 160] {
            apply_if(&mut client, CoolingProgram::Fixed { pump, fan: 90 }, None);
            past_the_cadence();
        }
        let status = client.status().unwrap();
        assert_eq!(status.cooling.revision.revision, 3);
        status.started_at_unix_ms
    };
    std::thread::sleep(Duration::from_millis(5));

    let restarted = Daemon::start_with(
        Paths::new(
            harness.paths.runtime_dir.join("second"),
            harness.paths.config_dir.clone(),
        ),
        &SysfsRoot::new(harness.fake.root_path()),
        &harness.proc_root,
        FAST_INTERVAL,
        RgbBackend::None,
        LcdBackend::None,
    )
    .unwrap();
    let status = restarted.status();
    assert_eq!(status.cooling.revision, Revision::initial());
    assert_eq!(
        status.cooling.committed,
        Some(CoolingProgram::Fixed { pump: 160, fan: 90 }),
        "the restore wrote, and revision 1 is what it wrote"
    );
    assert!(status.started_at_unix_ms > before);
}
