// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! `korictl history`, against a daemon serving a fake machine.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use common::{FAST_INTERVAL, Harness};
use support::korictl;

#[test]
fn history_prints_summaries_and_points_for_the_metrics_asked() {
    let harness = Harness::start("cli-history");
    std::thread::sleep(FAST_INTERVAL * 10);

    let run = korictl(
        harness.socket(),
        &[
            "history",
            "--metric",
            "liquid_temperature",
            "--metric=pump_speed",
            "--window",
            "90s",
            "--step",
            "30",
            "--points",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let reply = run.document();
    assert_eq!(reply["response"], "history");
    assert_eq!(reply["step_s"], 30);
    assert_eq!(reply["slots"], 3);
    let metrics = reply["metrics"].as_object().unwrap();
    assert_eq!(
        metrics.keys().collect::<Vec<_>>(),
        vec!["liquid_temperature", "pump_speed"]
    );
    let liquid = &metrics["liquid_temperature"];
    assert_eq!(liquid["unit"], "celsius");
    assert_eq!(liquid["summary"]["max"], 27.9);
    for array in ["avg", "min", "max"] {
        assert_eq!(liquid[array].as_array().unwrap().len(), 3, "{array}");
    }

    // Defaults: every metric, a summary only.
    let run = korictl(harness.socket(), &["history"]);
    assert_eq!(run.code, 0, "{run:?}");
    let reply = run.document();
    assert_eq!(reply["slots"], 900);
    assert_eq!(reply["metrics"].as_object().unwrap().len(), 10);
    assert!(reply["metrics"]["cpu_load"].get("avg").is_none());

    // A day at one minute is the minute tier.
    let run = korictl(harness.socket(), &["history", "--window", "24h"]);
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(run.document()["step_s"], 60);
}

#[test]
fn a_history_past_its_limits_is_the_daemons_refusal() {
    let harness = Harness::start("cli-history-limits");
    let run = korictl(
        harness.socket(),
        &["history", "--window", "24h", "--step", "1m", "--points"],
    );
    assert_eq!(run.code, 1, "{run:?}");
    let refusal = run.document();
    assert_eq!(refusal["error"], "history_out_of_range");
    assert_eq!(refusal["limit"], "max_values");

    // An unknown metric is judged by the daemon too.
    let run = korictl(harness.socket(), &["history", "--metric", "voltage"]);
    assert_eq!(run.code, 1, "{run:?}");
    assert_eq!(run.document()["error"], "malformed");

    // A duration korictl cannot read never leaves the process.
    let run = korictl(harness.socket(), &["history", "--window", "1.5h"]);
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(run.document()["token"], "1.5h");
}
