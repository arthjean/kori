// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! `korictl events`, against a daemon serving a fake machine.

#[path = "../../daemon/tests/common/mod.rs"]
mod common;
mod support;

use common::Harness;
use support::korictl;

#[test]
fn events_prints_a_numbered_page_and_pages_forward_from_a_cursor() {
    let harness = Harness::start("cli-events");
    let run = korictl(harness.socket(), &["events", "--limit", "512"]);
    assert_eq!(run.code, 0, "{run:?}");
    let page = run.document();
    assert_eq!(page["response"], "events");
    assert_eq!(page["missed"], 0);
    assert_eq!(page["events"][0]["seq"], 1);
    assert_eq!(page["events"][0]["event"], "daemon_started");
    let latest = page["latest_seq"].as_u64().unwrap();

    // One-shot reads, korictl's own included, leave the log alone.
    for _ in 0..20 {
        assert_eq!(korictl(harness.socket(), &["status"]).code, 0);
    }
    let run = korictl(
        harness.socket(),
        &["events", "--after", &latest.to_string(), "--limit=5"],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let page = run.document();
    assert_eq!(page["latest_seq"], latest, "{page}");
    assert_eq!(page["events"].as_array().unwrap().len(), 0);
}

#[test]
fn an_events_option_that_is_not_a_number_is_a_usage_error() {
    let harness = Harness::start("cli-events-usage");
    let run = korictl(harness.socket(), &["events", "--after", "soon"]);
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(run.document()["token"], "soon");

    let run = korictl(harness.socket(), &["events", "--limit", "513"]);
    assert_eq!(run.code, 1, "the ceiling is the daemon's: {run:?}");
    assert_eq!(run.document()["error"], "malformed");
}
