// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The telemetry history, asked for over the socket.
//!
//! One slot per tick of the daemon's own clock, which these tests run at the
//! fixture's fast interval: a "second" here is one tick, and a slot count is
//! what every assertion is about.

mod common;

use std::time::{Duration, Instant};

use kori_core::client::Client;
use kori_core::history::{HistoryLimit, HistoryMetric, HistoryReply};
use kori_core::ipc::{IpcError, Request, Response};

use common::Harness;

fn points(reply: &HistoryReply, metric: HistoryMetric) -> Vec<Option<f32>> {
    reply.metrics[&metric]
        .avg
        .clone()
        .expect("points were asked for")
}

fn wait_for_history(
    client: &mut Client,
    limit: Duration,
    mut condition: impl FnMut(&HistoryReply) -> bool,
) -> HistoryReply {
    let deadline = Instant::now() + limit;
    loop {
        let reply = client.history(None, None, None, true).unwrap();
        if condition(&reply) || Instant::now() >= deadline {
            return reply;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_young_daemon_reports_the_depth_it_has_and_nulls_beyond_it() {
    let harness = Harness::start("history-young");
    let mut client = harness.client();
    let reply = wait_for_history(&mut client, Duration::from_secs(10), |reply| {
        reply.available_s >= 40
    });
    let available = reply.available_s as usize;
    assert!((40..900).contains(&available), "{available}");
    assert_eq!((reply.step_s, reply.slots), (1, 900));
    assert_eq!(reply.metrics.len(), 10, "every metric by default");

    let liquid = points(&reply, HistoryMetric::LiquidTemperature);
    assert_eq!(liquid.len(), 900);
    assert!(
        liquid[..900 - available].iter().all(Option::is_none),
        "slots older than the daemon are null"
    );
    let recorded = liquid[900 - available..].iter().flatten().count();
    assert!(
        recorded * 2 > available,
        "most ticks found a fresh coolant sample: {recorded} of {available}"
    );
    let summary = reply.metrics[&HistoryMetric::LiquidTemperature]
        .summary
        .expect("the coolant was read");
    assert_eq!((summary.min, summary.max, summary.last), (27.9, 27.9, 27.9));
    assert_eq!(summary.samples as usize, recorded);
    assert_eq!(
        reply.metrics[&HistoryMetric::LiquidTemperature].unit,
        "celsius"
    );
    assert!(
        reply.metrics[&HistoryMetric::LiquidTemperature]
            .min
            .is_none(),
        "a one-second point folds nothing, so it carries no extremes"
    );
    assert!(reply.end_unix_ms > 0 && reply.end_unix_ms <= common::now_unix_ms());
}

#[test]
fn a_summary_without_points_carries_no_arrays_and_honours_the_metric_list() {
    let harness = Harness::start("history-summary");
    let mut client = harness.client();
    std::thread::sleep(common::FAST_INTERVAL * 5);
    let reply = client
        .history(
            Some(vec![HistoryMetric::PumpSpeed, HistoryMetric::FanSpeed]),
            Some(60),
            Some(5),
            false,
        )
        .unwrap();
    assert_eq!((reply.step_s, reply.slots), (5, 12));
    assert_eq!(
        reply.metrics.keys().copied().collect::<Vec<_>>(),
        vec![HistoryMetric::PumpSpeed, HistoryMetric::FanSpeed]
    );
    let pump = &reply.metrics[&HistoryMetric::PumpSpeed];
    assert_eq!(pump.unit, "rpm");
    assert!(pump.avg.is_none() && pump.min.is_none() && pump.max.is_none());
    assert_eq!(pump.summary.unwrap().max, 2970.0);
}

#[test]
fn an_unplugged_kraken_nulls_its_own_metrics_and_no_other() {
    let harness = Harness::start("history-unplug");
    let mut client = harness.client();
    let hwmon = harness.hwmon_path();
    wait_for_history(&mut client, Duration::from_secs(5), |reply| {
        reply.available_s >= 10
    });

    for attribute in ["temp1_input", "fan1_input", "fan2_input", "name"] {
        harness.fake.remove_attribute(&hwmon, attribute);
    }
    // Thirty ticks counted from the daemon's side, not from the removal: a
    // sampling pass that read the attributes just before they went still lands
    // a valid reading on the next tick, so counting from here could leave 29.
    let unplugged = wait_for_history(&mut client, Duration::from_secs(10), |reply| {
        let liquid = points(reply, HistoryMetric::LiquidTemperature);
        liquid.len() >= 30 && liquid[liquid.len() - 30..].iter().all(Option::is_none)
    });
    let liquid = points(&unplugged, HistoryMetric::LiquidTemperature);
    assert!(
        liquid[liquid.len() - 30..].iter().all(Option::is_none),
        "the history never showed the Kraken gone for 30 ticks"
    );
    for (attribute, value) in [
        ("name", "kraken2023"),
        ("temp1_input", "27900"),
        ("fan1_input", "2970"),
        ("fan2_input", "1764"),
    ] {
        std::fs::write(hwmon.join(attribute), format!("{value}\n")).unwrap();
    }
    let reply = wait_for_history(&mut client, Duration::from_secs(10), |reply| {
        points(reply, HistoryMetric::LiquidTemperature)
            .last()
            .copied()
            .flatten()
            .is_some()
    });

    // The longest run of null coolant slots is the unplugged stretch.
    let liquid = points(&reply, HistoryMetric::LiquidTemperature);
    let (mut longest, mut run, mut end) = (0, 0, 0);
    for (index, slot) in liquid.iter().enumerate() {
        if slot.is_none() && index >= 900 - reply.available_s as usize {
            run += 1;
            if run > longest {
                longest = run;
                end = index;
            }
        } else {
            run = 0;
        }
    }
    assert!(longest >= 30, "the Kraken was gone for {longest} slots");
    let stretch = end + 1 - longest..=end;
    for metric in [HistoryMetric::PumpSpeed, HistoryMetric::FanSpeed] {
        let series = points(&reply, metric);
        assert!(
            series[stretch.clone()].iter().all(Option::is_none),
            "{metric:?} held a reading while the device was gone"
        );
    }
    // The duty attributes outlive the readings in the fixture for one pass:
    // the collector reads them before it notices the instance is gone and
    // stops resolving it. After that pass the duty is null like the rest.
    for metric in [HistoryMetric::PumpDuty, HistoryMetric::FanDuty] {
        let held = points(&reply, metric)[stretch.clone()]
            .iter()
            .flatten()
            .count();
        assert!(held <= 2, "{metric:?} held {held} readings while unplugged");
    }
    // The CPU collector kept answering through the same stretch.
    let cpu = points(&reply, HistoryMetric::CpuTemperature);
    let answered = cpu[stretch.clone()].iter().flatten().count();
    assert!(
        answered * 2 > longest,
        "the CPU temperature answered {answered} of {longest} slots"
    );
}

#[test]
fn a_request_past_a_limit_names_the_limit() {
    let harness = Harness::start("history-limits");
    let mut client = harness.client();
    let cases = [
        (Some(900), Some(7), None, HistoryLimit::WindowMultipleOfStep),
        (Some(1_800), Some(30), None, HistoryLimit::SecondTierWindow),
        (
            Some(172_800),
            Some(60),
            None,
            HistoryLimit::MinuteTierWindow,
        ),
        (Some(86_400), Some(60), Some(true), HistoryLimit::MaxValues),
        (Some(900), Some(90), None, HistoryLimit::Step),
    ];
    for (window_s, step_s, points, expected) in cases {
        let response = client
            .request(Request::History {
                metrics: None,
                window_s,
                step_s,
                points,
            })
            .unwrap();
        match response {
            Response::Error(IpcError::HistoryOutOfRange { limit, detail }) => {
                assert_eq!(limit, expected, "{detail}");
            }
            other => panic!("{window_s:?}/{step_s:?}: expected a refusal, got {other:?}"),
        }
    }
    // The connection is still usable after a refusal.
    assert!(client.history(None, Some(86_400), Some(60), false).is_ok());
}

/// The daemon writes `config.toml` and nothing else, so the only files under
/// its directories after it has recorded are the ones it had before.
#[test]
fn the_history_never_reaches_the_disk() {
    let harness = Harness::start("history-disk");
    let mut client = harness.client();
    wait_for_history(&mut client, Duration::from_secs(10), |reply| {
        reply.available_s >= 70
    });
    let mut files = Vec::new();
    for root in [&harness.paths.config_dir, &harness.paths.runtime_dir] {
        let mut stack = vec![root.clone()];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory)
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.push(path.file_name().unwrap().to_string_lossy().to_string());
                }
            }
        }
    }
    for file in &files {
        assert!(
            !file.contains("history") && !file.ends_with(".json"),
            "unexpected file {file} in {files:?}"
        );
    }
    // A minute has folded, and it is in memory.
    let minutes = client.history(None, Some(120), Some(60), true).unwrap();
    assert!(minutes.available_s >= 60, "{}", minutes.available_s);
}
