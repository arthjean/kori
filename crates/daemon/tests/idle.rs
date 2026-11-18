// SPDX-FileCopyrightText: 2026 Arthur Jean
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! What an idle daemon costs the processor.
//!
//! One test in this binary, on purpose: the wakeups are counted over every
//! `korid-` thread of the process, and a second daemon started by a test
//! running in parallel would be counted with the first.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::{Harness, Machine};
use kori_daemon::wake::THREAD_PREFIX;

/// The budget the PRD sets: at most six a second with no client and no panel
/// stream, against the four the three collectors and the ticker need.
const WAKEUPS_PER_SECOND: f64 = 6.0;

/// How long the wakeups are counted over.
const WINDOW: Duration = Duration::from_secs(5);

/// Every thread of this process, by thread ID: its name and its voluntary
/// context switches so far.
fn threads() -> BTreeMap<u32, (String, u64)> {
    let mut threads = BTreeMap::new();
    for entry in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let Some(tid) = entry.file_name().to_str().and_then(|tid| tid.parse().ok()) else {
            continue;
        };
        // A thread that exited between the listing and the read has nothing
        // left to count.
        let (Ok(name), Ok(status)) = (
            std::fs::read_to_string(entry.path().join("comm")),
            std::fs::read_to_string(entry.path().join("status")),
        ) else {
            continue;
        };
        let switches = status
            .lines()
            .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|count| count.trim().parse().ok())
            .unwrap();
        threads.insert(tid, (name.trim().to_string(), switches));
    }
    threads
}

fn daemon_threads() -> BTreeMap<u32, (String, u64)> {
    threads()
        .into_iter()
        .filter(|(_, (name, _))| name.starts_with(THREAD_PREFIX))
        .collect()
}

fn names(threads: &BTreeMap<u32, (String, u64)>) -> Vec<String> {
    let mut names: Vec<String> = threads.values().map(|(name, _)| name.clone()).collect();
    names.sort();
    names
}

#[test]
fn an_idle_daemon_wakes_at_most_six_times_a_second_and_stops_at_once() {
    // The shipped cadence for both the collectors and the daemon's clock, no
    // client and no panel preset: the state an operator's machine sits in.
    let mut harness = Harness::start_on(
        "idle",
        Machine {
            interval: kori_daemon::telemetry::default_interval(),
            ..Machine::default()
        },
        |_, _| {},
    );

    // Past the start: the first pass of every collector resolves its sources,
    // which is work rather than idling.
    std::thread::sleep(Duration::from_millis(2_500));

    let before = daemon_threads();
    assert_eq!(
        names(&before),
        ["korid-gpu", "korid-kraken", "korid-system", "korid-ticker"],
        "every thread the idle daemon runs is named"
    );
    let started = Instant::now();
    std::thread::sleep(WINDOW);
    let after = daemon_threads();
    let elapsed = started.elapsed().as_secs_f64();

    let mut per_thread = BTreeMap::new();
    for (tid, (name, switches)) in &after {
        let earlier = before.get(tid).map_or(0, |(_, switches)| *switches);
        per_thread.insert(name.clone(), switches - earlier);
    }
    let total: u64 = per_thread.values().sum();
    let rate = total as f64 / elapsed;
    // Shown with `--nocapture`: the margin is worth seeing, not just the verdict.
    eprintln!("{rate:.2} wakeups a second over {elapsed:.1} s: {per_thread:?}");
    assert!(
        rate <= WAKEUPS_PER_SECOND,
        "{rate:.2} wakeups a second over {elapsed:.1} s: {per_thread:?}"
    );

    // A connection is served on a named thread too.
    let mut client = harness.client();
    client.status().unwrap();
    assert!(
        names(&daemon_threads()).contains(&"korid-client".to_string()),
        "{:?}",
        names(&daemon_threads())
    );
    drop(client);

    // Every one of those threads is waiting on a deadline up to a second away.
    // A stop ends the waits rather than waiting them out.
    let asked = Instant::now();
    harness.stop();
    let ticker_gone = asked.elapsed();
    assert!(
        ticker_gone < Duration::from_millis(100),
        "the server and its ticker took {ticker_gone:?} to stop"
    );
    // The collectors belong to the daemon state, which stops them when the
    // last handle on it goes.
    let asked = Instant::now();
    drop(harness);
    let deadline = asked + Duration::from_millis(100);
    while !daemon_threads().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        daemon_threads().is_empty(),
        "still running 100 ms after the stop: {:?}",
        names(&daemon_threads())
    );
}
