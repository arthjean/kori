// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The panel path, from the client to the glass.
//!
//! The same write gate the lighting path has, plus the geometry: a frame is laid
//! out for a panel that answered, or it is not laid out at all. A device that
//! never answered `0x31 0x01` may carry no panel, and is reported that way rather
//! than assumed to have one.

mod common;

use std::time::Duration;

use kori_core::KRAKEN_BASE;
use kori_core::capability::CapabilityId;
use kori_core::display::DisplayMode;
use kori_core::ipc::{HardwareState, IpcError, Request};
use kori_core::profile::{CoolingProgram, Profile};
use kori_daemon::state::Daemon;
use kori_daemon::{LcdBackend, Paths, RgbBackend};
use kori_hardware_linux::SysfsRoot;
use kori_hardware_linux::testing::FakeKraken;

use common::{FAST_INTERVAL, Harness, preset};

#[test]
fn a_machine_with_no_reachable_panel_reports_no_geometry_and_refuses_frames() {
    let harness = Harness::start("lcd-absent");
    let mut client = harness.client();

    let status = client.status().unwrap();
    assert!(
        status.display.panel.is_none(),
        "a panel nothing answered for must not be given a resolution"
    );
    assert!(status.display.committed.is_none());
    assert!(!status.display.streaming);

    let error = client
        .apply_display(preset(DisplayMode::DualInfographic))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            kori_core::client::ClientError::Refused(IpcError::Incompatible { .. })
        ),
        "{error:?}"
    );
}

#[test]
fn a_panel_that_answered_is_recorded_with_its_geometry_and_its_settings() {
    let harness = Harness::start_lcd("lcd-topology", "2.0.4");
    let mut client = harness.client();

    let record = client.capabilities().unwrap();
    let lcd = record.device(KRAKEN_BASE).unwrap().lcd.as_ref().unwrap();
    assert_eq!(lcd.firmware.value().map(String::as_str), Some("2.0.4"));
    assert!(lcd.answered(), "the display report was answered");

    let panel = lcd.panel.value().unwrap();
    assert_eq!((panel.width, panel.height), (240, 240));
    assert_eq!(panel.frame_bytes, 240 * 240 * 2);
    assert_eq!(panel.bulk_endpoint, 0x02);
    assert_eq!(panel.bulk_interface, 0);

    // The geometry is a candidate this product carries, and the record says so
    // rather than implying the device reported it.
    match &lcd.panel {
        kori_core::capability::Evidenced::Known { source, .. } => {
            assert!(source.contains("candidate"), "{source}");
            assert!(source.contains("reports no"), "{source}");
        }
        other => panic!("expected a recorded candidate, got {other:?}"),
    }

    // And the daemon serves that same geometry in its status, so the client
    // can size a preview without asking for the whole capability record.
    let status = client.status().unwrap();
    assert_eq!(status.display.panel.map(|panel| panel.width), Some(240));
}

#[test]
fn an_unvalidated_firmware_refuses_every_frame_and_names_the_missing_evidence() {
    let harness = Harness::start_lcd("lcd-unvalidated", "2.9.9");
    let mut client = harness.client();

    let error = client
        .apply_display(preset(DisplayMode::DualInfographic))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            kori_core::client::ClientError::Refused(IpcError::Incompatible { .. })
        ),
        "{error:?}"
    );

    let record = client.capabilities().unwrap();
    let reason = record
        .device(KRAKEN_BASE)
        .unwrap()
        .capability(CapabilityId::LcdFrame)
        .unwrap()
        .state
        .blocked_reason()
        .unwrap();
    assert!(reason.contains("2.9.9"), "{reason}");

    // Nothing was sent, so the daemon claims nothing about the panel.
    let status = client.status().unwrap();
    assert!(status.display.committed.is_none());
    assert!(!status.display.streaming);
}

#[test]
fn an_invalid_preset_is_refused_before_the_capability_gate_is_even_reached() {
    // Image mode with no file is wrong whatever the panel reports, so it is
    // refused as a validation error rather than as an incompatibility.
    let harness = Harness::start_lcd("lcd-invalid", "2.0.4");
    let mut client = harness.client();

    let error = client
        .apply_display(preset(DisplayMode::Image))
        .unwrap_err();
    match &error {
        kori_core::client::ClientError::Refused(IpcError::Display(display)) => {
            assert_eq!(display.field(), Some("image"));
        }
        other => panic!("expected a typed display refusal, got {other:?}"),
    }
}

#[test]
fn every_validated_firmware_completes_the_whole_frame_path_from_the_client() {
    // Vacuous until a `--lcd-write-probe` an operator watched fills the list,
    // which is the correct failure direction: nothing is claimed to work on a
    // firmware nobody has driven. Once filled, this proves the round trip from
    // the client's own entry point.
    for firmware in kori_hardware_linux::lcd::VALIDATED_FIRMWARE {
        let harness = Harness::start_lcd("lcd-validated", firmware);
        let mut client = harness.client();

        let wanted = preset(DisplayMode::DualInfographic);
        let outcome = client.apply_display(wanted.clone()).unwrap();
        assert_eq!(outcome.frames, 1, "{firmware} sent no frame");
        assert!(!outcome.deduplicated);
        assert_eq!(outcome.hardware, HardwareState::Confirmed);

        // The daemon now knows what the panel holds, and says it is streaming
        // because the preset reads telemetry.
        let status = client.status().unwrap();
        assert_eq!(status.display.committed.as_ref(), Some(&wanted));
        assert!(status.display.streaming);

        // A solid field is not streamed: nothing in it changes with a sample.
        let solid = preset(DisplayMode::Solid);
        assert_eq!(client.apply_display(solid).unwrap().frames, 1);
        assert!(!client.status().unwrap().display.streaming);
    }
}

#[test]
fn a_saved_profile_round_trips_its_panel_preset_without_pixels_reaching_the_file() {
    let harness = Harness::start_lcd("lcd-profile", "2.0.4");
    let mut wanted = preset(DisplayMode::DualInfographic);
    wanted.readings[0].metric = kori_core::display::LcdMetric::LiquidTemperature;
    wanted.orientation = kori_core::display::Orientation::Deg180;

    let profile = Profile {
        name: "Panel".into(),
        program: CoolingProgram::Onboard,
        device: None,
        lighting: Vec::new(),
        display: Some(wanted.clone()),
    };

    {
        let mut client = harness.client();
        client
            .request(Request::SaveProfile {
                profile: profile.clone(),
            })
            .unwrap();
    }

    // The stored file carries a description, not a picture.
    let stored = std::fs::read_to_string(harness.paths.config_file()).unwrap();
    assert!(stored.contains("dual_infographic"), "{stored}");
    assert!(stored.contains("liquid_temperature"), "{stored}");
    assert!(stored.contains("deg180"), "{stored}");
    for leaked in ["rgb565", "0x36", "framebuffer", "pixels", "115200"] {
        assert!(
            !stored.to_ascii_lowercase().contains(leaked),
            "{leaked:?} leaked into the configuration file:\n{stored}"
        );
    }

    // A fresh daemon over the same directory reads the same preset back.
    let restarted = Daemon::start_with(
        Paths::new(
            harness.paths.runtime_dir.join("second"),
            harness.paths.config_dir.clone(),
        ),
        &SysfsRoot::new(harness.fake.root_path()),
        &harness.proc_root,
        FAST_INTERVAL,
        RgbBackend::None,
        LcdBackend::Link(Box::new(FakeKraken::new("2.0.4").link())),
    )
    .unwrap();
    let reloaded = restarted
        .status()
        .devices
        .iter()
        .map(|device| device.id)
        .collect::<Vec<_>>();
    assert!(reloaded.contains(&KRAKEN_BASE));

    let mut client = harness.client();
    let (_, profiles) = client.profiles().unwrap();
    let stored_profile = profiles
        .iter()
        .find(|candidate| candidate.name == "Panel")
        .unwrap();
    assert_eq!(stored_profile.display.as_ref(), Some(&wanted));
}

#[test]
fn an_animation_frame_lands_within_50_ms_of_its_due_time_at_the_shipped_cadence() {
    // The daemon's clock at its shipped one second, so nothing but the
    // animation's own deadline can wake the ticker in time for its frames.
    let firmware = kori_hardware_linux::lcd::VALIDATED_FIRMWARE[0];
    let harness = Harness::start_on(
        "lcd-animation-cadence",
        common::Machine {
            interval: kori_daemon::telemetry::default_interval(),
            lcd: LcdBackend::Link(Box::new(FakeKraken::new(firmware).link())),
            ..common::Machine::default()
        },
        |_, _| {},
    );
    let mut client = harness.client();

    // Eight distinct frames of 200 ms each.
    const DELAY_MS: i64 = 200;
    let directory = kori_lcd_renderer::testing::scratch("animation-cadence").unwrap();
    let path = directory.join("animation.gif");
    let pictures: Vec<(kori_core::lighting::Rgb, u16)> = (0..8)
        .map(|index| {
            (
                kori_core::lighting::Rgb::new(index as u8 * 32, 0x40, 0x80),
                (DELAY_MS / 10) as u16,
            )
        })
        .collect();
    kori_lcd_renderer::testing::write_gif(&path, 16, &pictures).unwrap();
    let mut animated = preset(DisplayMode::Image);
    animated.image = Some(path);

    // The apply sends the first frame; the second is due 200 ms later, long
    // before the next telemetry pass the ticker was waiting for.
    let outcome = client.apply_display(animated).unwrap();
    assert_eq!(outcome.frames, 1);
    std::thread::sleep(Duration::from_millis(1_100));

    for _ in 0..6 {
        let display = client.status().unwrap().display;
        let started = display.animation_started_at_unix_ms.unwrap() as i64;
        let landed = display.last_frame_at_unix_ms.unwrap() as i64;
        // Frame k is due `k * DELAY_MS` after the start, so how far past its
        // due instant the last one landed is the remainder.
        let late = (landed - started).rem_euclid(DELAY_MS);
        assert!(late < 50, "a frame landed {late} ms after it was due");
        // And none was skipped for lateness: one picture per 200 ms elapsed.
        let expected = ((landed - started) / DELAY_MS) as u64 + 1;
        assert_eq!(display.frames_sent, expected, "{display:?}");
        assert_eq!(display.dropped_frames, 0, "{display:?}");
        std::thread::sleep(Duration::from_millis(130));
    }
}

#[test]
fn the_daemon_clock_redraws_on_a_moved_reading_and_on_a_returning_kraken_only() {
    let firmware = kori_hardware_linux::lcd::VALIDATED_FIRMWARE[0];
    let harness = Harness::start_on(
        "lcd-steady",
        common::Machine {
            lcd: LcdBackend::Link(Box::new(FakeKraken::new(firmware).link())),
            ..common::Machine::default()
        },
        |_, _| {},
    );
    let mut client = harness.client();
    let hwmon = harness.hwmon_path();

    // Both readings come from the fixture, so nothing moves unless the test
    // moves it.
    let mut steady = preset(DisplayMode::DualInfographic);
    steady.readings[0].metric = kori_core::display::LcdMetric::LiquidTemperature;
    steady.readings[1].metric = kori_core::display::LcdMetric::CpuTemperature;
    assert_eq!(client.apply_display(steady).unwrap().frames, 1);
    let frames =
        |client: &mut kori_core::client::Client| client.status().unwrap().display.frames_sent;
    let wait_for_frames = |client: &mut kori_core::client::Client, wanted: u64| {
        harness.wait_for_status(client, Duration::from_secs(5), |status| {
            status.display.frames_sent >= wanted
        });
        frames(client)
    };

    // Ten ticks of identical readings put nothing on the wire.
    let settled = wait_for_frames(&mut client, 1);
    std::thread::sleep(common::FAST_INTERVAL * 10);
    assert_eq!(frames(&mut client), settled);

    // A reading that moved is drawn and sent on the next tick.
    harness.fake.set_reading(&hwmon, "temp1_input", "31500");
    assert_eq!(wait_for_frames(&mut client, settled + 1), settled + 1);
    std::thread::sleep(common::FAST_INTERVAL * 10);
    let moved = frames(&mut client);
    assert_eq!(moved, settled + 1);

    // The Kraken goes away and comes back with exactly the readings it left
    // with. The panel it returns with holds nothing this daemon sent, so the
    // first tick draws and sends the same picture again.
    let attributes = ["temp1_input", "fan1_input", "fan2_input", "name"];
    let saved: Vec<(&str, String)> = attributes
        .iter()
        .map(|attribute| {
            (
                *attribute,
                std::fs::read_to_string(hwmon.join(attribute)).unwrap(),
            )
        })
        .collect();
    for attribute in attributes {
        harness.fake.remove_attribute(&hwmon, attribute);
    }
    harness.wait_for_telemetry(&mut client, Duration::from_secs(5), |snapshot| {
        !snapshot.kraken.present
    });
    // The daemon's own clock has to see the absence too, not just the sampler.
    std::thread::sleep(common::FAST_INTERVAL * 5);
    for (attribute, contents) in &saved {
        std::fs::write(hwmon.join(attribute), contents).unwrap();
    }
    assert_eq!(
        wait_for_frames(&mut client, moved + 1),
        moved + 1,
        "the returning panel was sent its picture"
    );
}
